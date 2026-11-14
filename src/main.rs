//! wayswitch-rs — автоисправление раскладки на Wayland с защитой паролей.
#![deny(unsafe_code)]

mod logging;

mod alloc;
mod backend;
mod buffer;
mod config;
mod controller;
mod detector;
mod focus;
mod gesture;
mod input;
mod keycodes;
mod keymap;
mod lang;
mod secure;
mod service;
mod session;
mod typist;

use std::os::unix::process::CommandExt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::controller::{Controller, Status};
use crate::detector::{Detector, Exceptions, Sensitivity};
use crate::gesture::Gesture;

pub enum Cmd {
    Pause,
    Resume,
    FixWord,
    FixPhrase,
    Reload,
    ForgetLearned,
}

pub enum Event {
    Key { code: u16, value: i32, device: u32, seq: u64, presses: u64, at: std::time::Instant },
    Click,
    Scroll,
    Caps(bool),
    DeviceGone(u32),
    Layout { index: usize, external: bool },
    LayoutsChanged,
    Locked(bool),
    FocusChanged,
    Cmd(Cmd),
    Tick,
}

// Вся куча затирается при освобождении: см. alloc.rs.
#[global_allocator]
static ALLOCATOR: alloc::ZeroingAlloc = alloc::ZeroingAlloc;

const USAGE: &str = "\
wayswitch-rs — автоисправление русско-английской раскладки (Wayland)

  wayswitch-rs run [--dry-run] [-v]   запустить демон
  wayswitch-rs status                 состояние демона
  wayswitch-rs pause | resume         пауза / продолжить
  wayswitch-rs fix-word | fix-phrase  исправить последнее слово / фразу
  wayswitch-rs reload                 перечитать конфиг
  wayswitch-rs forget-learned         забыть выученные слова
  wayswitch-rs doctor                 проверить систему и безопасность
  wayswitch-rs focus-test             20 с показывать, что at-spi видит в фокусе
";

fn main() {
    // Первым делом: дальше в процессе нет ни одного участка с лишними правами.
    secure::init_privileges();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    if cmd != "run" {
        secure::drop_privileges_permanently();
    }
    logging::init_from_env();
    let code = match cmd {
        "run" => run(args.iter().any(|a| a == "--dry-run"), args.iter().any(|a| a == "-v")),
        "status" => status(),
        "pause" | "resume" | "fix-word" | "fix-phrase" | "reload" | "forget-learned" => {
            let method = match cmd {
                "pause" => "Pause",
                "resume" => "Resume",
                "fix-word" => "FixLastWord",
                "fix-phrase" => "FixPhrase",
                "reload" => "Reload",
                _ => "ForgetLearned",
            };
            match service::call(method) {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        "doctor" => doctor(),
        "focus-test" => focus_test(),
        _ => {
            print!("{USAGE}");
            if cmd == "help" || cmd == "--help" || cmd == "-h" { 0 } else { 2 }
        }
    };
    std::process::exit(code);
}

fn status() -> i32 {
    let msg = match service::call("GetStatus") {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let map: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = match msg.body().deserialize() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    for k in keys {
        let val = &map[k];
        let shown = match &**val {
            zbus::zvariant::Value::Bool(b) => if *b { "да".into() } else { "нет".into() },
            zbus::zvariant::Value::Str(s) => s.to_string(),
            zbus::zvariant::Value::I32(i) => i.to_string(),
            zbus::zvariant::Value::U32(i) => i.to_string(),
            other => format!("{other:?}"),
        };
        println!("{k:14} {shown}");
    }
    0
}

fn run(dry_run: bool, verbose: bool) -> i32 {
    secure::harden_process();
    if secure::setgid_mode() {
        // Запустить setgid-бинарник может кто угодно, и его stderr достаётся
        // запустившему. Подробный журнал и dry-run рассказали бы тому длины
        // слов и решения детектора по ним.
        if verbose || dry_run {
            error!("-v и --dry-run недоступны в setgid-установке");
            return 2;
        }
        logging::set_level(logging::INFO);
    } else if verbose {
        logging::set_level(logging::DEBUG);
    }
    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            error!("конфиг: {e}");
            return 2;
        }
    };
    let (tx, rx) = mpsc::channel::<Event>();
    let status = Arc::new(Mutex::new(Status::default()));
    let conn = match service::start(tx.clone(), status.clone()) {
        Ok(c) => c,
        Err(e) => {
            error!("имя {} занято — демон уже запущен? ({e})", service::BUS_NAME);
            return 3;
        }
    };
    // В setgid-режиме текст не отправляется по D-Bus: шину задаёт окружение, и
    // запустивший бинарник с подставной шиной получал бы набранные слова.
    let use_text = cfg.backend.gnome_typing == "extension" && !secure::setgid_mode();
    let backend = match backend::select(&cfg.backend.prefer, &conn, tx.clone(), use_text) {
        Ok(b) => b,
        Err(e) => {
            error!("нечем переключать раскладку: {e}");
            return 4;
        }
    };
    let keymap = match keymap::Keymap::from_names(&backend.layouts(), &backend.xkb_options()) {
        Ok(k) => k,
        Err(e) => {
            error!("{e}");
            return 4;
        }
    };
    info!(
        "бэкенд: {}, раскладки: {}",
        backend.name(),
        backend.layouts().iter().map(|s| if s.variant.is_empty() { s.layout.clone() } else { format!("{}({})", s.layout, s.variant) }).collect::<Vec<_>>().join(", ")
    );
    let learned = if cfg.security.learn_words {
        match secure::LearnedStore::open(&secure::data_dir()) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("выученные слова недоступны: {e}");
                None
            }
        }
    } else {
        None
    };
    let det = Detector {
        keymap,
        models: lang::Models::builtin(),
        exceptions: Exceptions { plain: config::load_exceptions(), learned },
        sensitivity: Sensitivity::named(&cfg.general.sensitivity),
    };
    let typist = match typist::UInputTypist::new() {
        Ok(t) => Some(t),
        Err(e) => {
            if backend.text_typist().is_none() && !dry_run {
                error!("нет доступа к /dev/uinput ({e}): печатать нечем, см. `wayswitch-rs doctor`");
                return 4;
            }
            warn!("нет доступа к /dev/uinput ({e}): Caps Lock исправляться не будет");
            None
        }
    };
    // Композитору нужно время, чтобы подхватить новое устройство.
    std::thread::sleep(Duration::from_millis(200));

    let phys = Arc::new(input::Phys::default());
    input::start(tx.clone(), phys.clone());
    let focus = focus::start(tx.clone());
    session::start(tx.clone());
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("tick".into())
            .spawn(move || while tx.send(Event::Tick).is_ok() {
                std::thread::sleep(Duration::from_millis(500));
            })
            .expect("поток таймера");
    }

    let mut c = Controller::new(cfg, det, backend, typist, Some(focus), phys.clone(), status, dry_run);
    info!("wayswitch-rs запущен{}", if dry_run { " (dry-run)" } else { "" });
    for ev in rx {
        match ev {
            Event::Key { code, value, device, seq, presses, at } => c.on_key(code, value, device, seq, presses, at),
            Event::Click => c.click(),
            Event::Scroll => c.scroll(),
            Event::Caps(on) => c.set_caps(on),
            Event::DeviceGone(id) => c.device_gone(id),
            Event::Layout { external, .. } => c.layout_changed(external),
            Event::LayoutsChanged => {
                info!("список раскладок изменился — перезапуск");
                drop(c);
                let exe = std::env::current_exe().unwrap_or_else(|_| "wayswitch-rs".into());
                let err = std::process::Command::new(exe).args(std::env::args().skip(1)).exec();
                error!("перезапуск не удался: {err}");
                return 1;
            }
            Event::Locked(on) => c.set_locked(on),
            Event::FocusChanged => c.focus_changed(),
            Event::Tick => c.tick(),
            Event::Cmd(cmd) => match cmd {
                Cmd::Pause => c.pause(),
                Cmd::Resume => c.resume(),
                Cmd::FixWord => c.manual(Gesture::Word, None),
                Cmd::FixPhrase => c.manual(Gesture::Phrase, None),
                Cmd::Reload => match Config::load() {
                    Ok(cfg) => {
                        c.reload(cfg);
                        info!("конфиг перечитан");
                    }
                    Err(e) => warn!("конфиг не перечитан: {e}"),
                },
                Cmd::ForgetLearned => c.forget_learned(),
            },
        }
    }
    0
}

// --- focus-test ------------------------------------------------------------------

fn focus_test() -> i32 {
    println!("20 секунд: переключайтесь между полями. Набранный текст не читается.");
    // Тот же трекер, что в демоне: события плюс опрос.
    let (tx, _rx) = mpsc::channel();
    let tracker = focus::start(tx);
    let describe = |i: &focus::FocusInfo| {
        format!(
            "{:20} роль: {:16} {}",
            i.app.as_deref().unwrap_or("?"),
            i.role,
            if i.password {
                "ПОЛЕ ПАРОЛЯ"
            } else if i.terminal {
                "терминал"
            } else if i.known {
                "обычное поле"
            } else {
                "фокус не найден"
            }
        )
    };
    let mut last = None;
    for _ in 0..80 {
        tracker.poke();
        let masked = tracker.masked_now();
        let line = format!("{}{}", describe(&tracker.snapshot()), if masked { " (текст замаскирован)" } else { "" });
        if last.as_ref() != Some(&line) {
            println!("{line}");
            last = Some(line);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    0
}

// --- doctor ----------------------------------------------------------------------

/// Даст ли setgid-бинарник доступ к устройству: группа файла совпадает.
fn setgid_reaches(exe: Option<&std::fs::Metadata>, path: &std::path::Path, bits: u32) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let (Some(exe), Ok(dev)) = (exe, std::fs::metadata(path)) else { return false };
    exe.permissions().mode() & 0o2000 != 0 && exe.gid() == dev.gid() && dev.permissions().mode() & bits == bits
}

fn doctor() -> i32 {
    let mut problems = 0;
    let mut check = |good: bool, text: String| {
        println!("{}{text}", if good { "  ок " } else { "  !! " });
        if !good {
            problems += 1;
        }
    };
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    check(
        session == "wayland",
        if session == "x11" {
            "сеанс X11: любая программа и так видит нажатия через сам X-сервер, защита от кейлоггеров здесь не работает".into()
        } else {
            format!("сеанс {session}")
        },
    );

    // Права уже сброшены: всё ниже — то, что может обычная программа пользователя.
    let exe = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
    let is_setgid = exe.as_ref().is_some_and(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o2000 != 0);
    let events: Vec<_> = std::fs::read_dir("/dev/input")
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event"))).collect())
        .unwrap_or_default();
    let readable = events.iter().filter(|p| std::fs::File::open(p).is_ok()).count();
    let reachable = events.iter().filter(|p| setgid_reaches(exe.as_ref(), p, 0o040)).count();
    let uinput = std::path::Path::new("/dev/uinput");

    println!("\nУстройства:");
    check(reachable > 0 || readable > 0, format!("демону доступно устройств ввода: {} из {}", reachable.max(readable), events.len()));
    check(
        setgid_reaches(exe.as_ref(), uinput, 0o020) || std::fs::OpenOptions::new().write(true).open(uinput).is_ok(),
        "демон может писать в /dev/uinput".into(),
    );
    check(is_setgid, format!("бинарник setgid: {}", if is_setgid { "да" } else { "нет (запущен не из установки)" }));

    println!("\nПароли и приватность:");
    check(
        readable == 0,
        if readable > 0 {
            format!("обычные программы читают {readable} устройств ввода напрямую, любая может работать как кейлоггер")
        } else {
            "обычные программы не читают клавиатуру напрямую".into()
        },
    );
    check(!secure::in_group("input"), "вы не состоите в группе input".into());
    let ptrace = std::fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").unwrap_or_default();
    check(ptrace.trim() != "0", format!("kernel.yama.ptrace_scope = {} (0 — программы читают память друг друга)", ptrace.trim()));
    let swaps = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    let plain: Vec<&str> = swaps.lines().skip(1).filter(|l| !l.contains("/dev/dm-") && !l.contains("zram")).filter_map(|l| l.split_whitespace().next()).collect();
    check(plain.is_empty(), if plain.is_empty() { "swap не на открытом диске".into() } else { format!("незашифрованный swap: {}", plain.join(", ")) });
    match focus::bridge_available() {
        Ok(n) => check(n > 0, format!("at-spi отвечает, видно приложений: {n}")),
        Err(e) => check(false, format!("at-spi недоступен ({e}): автоисправление будет выключено")),
    }
    println!("     Что at-spi видит в конкретном поле: wayswitch-rs focus-test");
    match Config::load() {
        Ok(cfg) => {
            check(cfg.security.unknown_focus == config::UnknownFocus::Skip, format!("unknown_focus = {:?}", cfg.security.unknown_focus).to_lowercase());
            check(cfg.security.terminals != config::TerminalMode::Auto, format!("terminals = {:?}", cfg.security.terminals).to_lowercase());
        }
        Err(e) => check(false, format!("конфиг: {e}")),
    }
    println!("\n{}", if problems == 0 { "Всё в порядке." } else { "Есть замечания (помечены !!)." });
    i32::from(problems > 0)
}
