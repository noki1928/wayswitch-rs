//! Контроллер: нажатия → решение → исправление.
//!
//! Работает в одном потоке и разбирает события по очереди. Исправление
//! исполняется синхронно; нажатия, пришедшие во время него, потоки ввода
//! отмечают в [`Phys`], и по ним исправление прерывается до точки невозврата
//! (первого Backspace).
//!
//! Политика паролей:
//! * в поле пароля, в менеджерах паролей и в окнах polkit/pinentry нажатия
//!   даже не попадают в буфер — исправлять и запоминать нечего;
//! * слово, хотя бы одна буква которого набрана, когда фокус был неизвестен
//!   или «нельзя», автоматически не исправляется;
//! * при смене фокуса, блокировке экрана и паузе буфер затирается.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::backend::LayoutBackend;
use crate::buffer::{BufferEvent, InputBuffer, Keys};
use crate::config::{Config, TerminalMode, UnknownFocus};
use crate::detector::{Action, Detector};
use crate::focus::FocusTracker;
use crate::gesture::{Gesture, ShiftGesture};
use crate::input::Phys;
use crate::keycodes as kc;
use crate::keymap::{Alphabet, KeyPress};
use crate::lang::Lang;
use crate::secure::SecretText;
use crate::typist::UInputTypist;

const RELEASE_WAIT: Duration = Duration::from_millis(500);

/// Встроенные «чувствительные» программы: менеджеры паролей, запросы
/// паролей и ключей. В них демон не делает ничего, даже по жесту.
const SENSITIVE_APPS: [&str; 22] = [
    "keepassxc", "keepass", "bitwarden", "1password", "enpass", "lastpass", "proton pass",
    "pinentry", "gcr-prompter", "gnome-keyring", "seahorse", "polkit", "ssh-askpass",
    "ksshaskpass", "kwalletd", "kwalletmanager", "lxqt-policykit", "mate-polkit",
    "authentication agent", "x11-ssh-askpass", "gpg", "passwordsafe",
];
const TERMINAL_APPS: [&str; 22] = [
    "terminal", "kgx", "konsole", "kitty", "alacritty", "foot", "wezterm", "ptyxis", "tilix",
    "xterm", "terminator", "guake", "tilda", "terminology", "blackbox", "contour", "rio",
    "urxvt", "st-256color", "yakuake", "cool-retro-term", "ghostty",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Password,
    Sensitive,
    Terminal,
    Excluded,
    Unknown,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Allow => "ok",
            Verdict::Password => "password",
            Verdict::Sensitive => "sensitive-app",
            Verdict::Terminal => "terminal",
            Verdict::Excluded => "excluded-app",
            Verdict::Unknown => "unknown",
        }
    }
}

#[derive(Default, Clone)]
pub struct Status {
    pub paused: bool,
    pub locked: bool,
    pub backend: String,
    pub layout: i32,
    pub auto_correct: bool,
    pub focus: String,
    pub fixes: u32,
    pub undos: u32,
    pub learned: u32,
    pub setgid: bool,
    pub atspi_events: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    Aborted,
    Failed,
}

struct FixReq {
    delete: usize,
    text: SecretText,
    target: usize,
    switch: bool,
    caps_off: bool,
    restore: Option<usize>,
}

struct LastFix {
    original: Keys,
    target_keys: Vec<KeyPress>,
    group_before: usize,
    at: Instant,
    learnable: bool,
}

struct Busy {
    after_seq: u64,
    until_seq: u64,
    aborted: bool,
}

pub struct Controller {
    pub cfg: Config,
    apps: Vec<String>,
    det: Detector,
    buffer: InputBuffer,
    backend: Box<dyn LayoutBackend>,
    typist: Option<UInputTypist>,
    focus: Option<Arc<FocusTracker>>,
    phys: Arc<Phys>,
    pub status: Arc<Mutex<Status>>,
    gesture: ShiftGesture,
    paused: bool,
    locked: bool,
    dry_run: bool,
    context: VecDeque<Option<Lang>>,
    last_fix: Option<LastFix>,
    undo_counts: HashMap<u64, u32>,
    last_edit: Option<Instant>,
    last_external_layout: Option<Instant>,
    /// В текущем слове есть буквы, набранные там, где исправлять нельзя.
    taint_hard: bool,
    /// …или когда фокус был неизвестен.
    taint_unknown: bool,
    busy: Option<Busy>,
    trigger: Option<(u64, u64, (u32, u16))>,
    physical_gesture: bool,
    started: Instant,
}

#[allow(clippy::too_many_arguments)]
impl Controller {
    pub fn new(
        cfg: Config,
        det: Detector,
        backend: Box<dyn LayoutBackend>,
        typist: Option<UInputTypist>,
        focus: Option<Arc<FocusTracker>>,
        phys: Arc<Phys>,
        status: Arc<Mutex<Status>>,
        dry_run: bool,
    ) -> Self {
        let buffer = InputBuffer::new(cfg.general.phrase_timeout_sec, terminators(&cfg));
        let c = Controller {
            apps: crate::config::load_apps(),
            cfg,
            det,
            buffer,
            backend,
            typist,
            focus,
            phys,
            status,
            gesture: ShiftGesture::new(),
            paused: false,
            locked: false,
            dry_run,
            context: VecDeque::new(),
            last_fix: None,
            undo_counts: HashMap::new(),
            last_edit: None,
            last_external_layout: None,
            taint_hard: false,
            taint_unknown: false,
            busy: None,
            trigger: None,
            physical_gesture: false,
            started: Instant::now(),
        };
        c.publish();
        c
    }

    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn reload(&mut self, cfg: Config) {
        self.buffer.phrase_timeout = cfg.general.phrase_timeout_sec;
        self.buffer.set_terminators(terminators(&cfg));
        self.det.sensitivity = crate::detector::Sensitivity::named(&cfg.general.sensitivity);
        self.det.exceptions.plain = crate::config::load_exceptions();
        self.apps = crate::config::load_apps();
        self.cfg = cfg;
        self.forget("перечитан конфиг");
        self.publish();
    }

    // --- состояние ------------------------------------------------------------

    pub fn publish(&self) {
        let mut s = self.status.lock().unwrap();
        s.paused = self.paused;
        s.locked = self.locked;
        s.backend = self.backend.name().into();
        s.layout = self.backend.current().map_or(-1, |i| i as i32);
        s.auto_correct = self.cfg.general.auto_correct;
        s.focus = self.verdict().as_str().into();
        s.learned = self.det.exceptions.learned.as_ref().map_or(0, |l| l.len() as u32);
        s.setgid = crate::secure::setgid_mode();
        s.atspi_events = self.focus.as_ref().is_some_and(|f| f.events_working());
    }

    /// Затереть всё, что помним о набранном.
    fn forget(&mut self, why: &str) {
        self.buffer.reset();
        self.context.clear();
        self.last_fix = None;
        self.taint_hard = false;
        self.taint_unknown = false;
        crate::debug!("буфер затёрт: {why}");
    }

    pub fn pause(&mut self) {
        self.paused = true;
        self.forget("пауза");
        crate::info!("пауза");
        self.publish();
    }

    pub fn resume(&mut self) {
        self.paused = false;
        crate::info!("работаю");
        self.publish();
    }

    pub fn set_locked(&mut self, on: bool) {
        if on != self.locked {
            crate::info!("сессия {}", if on { "заблокирована" } else { "активна" });
        }
        self.locked = on;
        if on {
            self.forget("блокировка экрана");
            self.undo_counts.clear();
        }
        self.publish();
    }

    pub fn forget_learned(&mut self) {
        if let Some(store) = self.det.exceptions.learned.as_mut() {
            match store.clear() {
                Ok(()) => crate::info!("выученные слова забыты"),
                Err(e) => crate::warn!("не удалось очистить выученные слова: {e}"),
            }
        }
        self.undo_counts.clear();
        self.publish();
    }

    pub fn set_caps(&mut self, on: bool) {
        self.buffer.caps = on;
    }

    pub fn device_gone(&mut self, device: u32) {
        self.buffer.device_gone(device);
    }

    pub fn focus_changed(&mut self) {
        self.forget("сменился фокус");
        self.publish();
    }

    pub fn layout_changed(&mut self, external: bool) {
        if external {
            self.last_external_layout = Some(Instant::now());
            self.context.clear();
        }
        self.publish();
    }

    pub fn click(&mut self) {
        self.gesture.reset();
        self.forget("щелчок мышью");
    }

    pub fn scroll(&mut self) {
        self.gesture.reset();
    }

    pub fn tick(&mut self) {
        if self.buffer.expire(self.now()) {
            self.taint_hard = false;
            self.taint_unknown = false;
        }
        let window = Duration::from_secs_f64(self.cfg.general.undo_window_sec);
        if self.last_fix.as_ref().is_some_and(|l| l.at.elapsed() > window) {
            self.last_fix = None;
        }
    }

    // --- фокус ------------------------------------------------------------------

    pub fn verdict(&self) -> Verdict {
        let unknown = match self.cfg.security.unknown_focus {
            UnknownFocus::Skip => Verdict::Unknown,
            UnknownFocus::Allow => Verdict::Allow,
        };
        let Some(focus) = self.focus.as_ref().filter(|f| f.healthy()) else { return unknown };
        let s = focus.snapshot();
        if !s.fresh() {
            return unknown;
        }
        if s.password {
            return Verdict::Password;
        }
        let app = s.app.as_deref().unwrap_or("").to_lowercase();
        let matches = |list: &[&str]| {
            !app.is_empty() && list.iter().any(|p| !p.trim().is_empty() && app.contains(&p.trim().to_lowercase()))
        };
        let own = |list: &[String]| matches(&list.iter().map(String::as_str).collect::<Vec<_>>());
        if matches(&SENSITIVE_APPS) || own(&self.cfg.security.sensitive_apps) {
            return Verdict::Sensitive;
        }
        if s.terminal || matches(&TERMINAL_APPS) {
            return Verdict::Terminal;
        }
        if own(&self.apps) {
            return Verdict::Excluded;
        }
        if !s.known {
            return unknown;
        }
        Verdict::Allow
    }

    /// Нажатия в этом состоянии нельзя даже держать в буфере.
    fn must_not_buffer(&self, v: Verdict) -> bool {
        match v {
            Verdict::Password | Verdict::Sensitive => true,
            Verdict::Terminal => self.cfg.security.terminals == TerminalMode::Off,
            _ => false,
        }
    }

    fn auto_ok(&self, v: Verdict) -> bool {
        match v {
            Verdict::Allow => true,
            Verdict::Terminal => self.cfg.security.terminals == TerminalMode::Auto,
            _ => false,
        }
    }

    // --- ввод -------------------------------------------------------------------

    pub fn on_key(&mut self, code: u16, value: i32, device: u32, seq: u64, presses: u64) {
        if let Some(b) = &self.busy {
            if seq > b.after_seq && seq <= b.until_seq {
                // Нажато, пока демон печатал.
                let aborted = b.aborted;
                if !aborted {
                    self.buffer.note(code, value, device);
                    self.gesture.on_key(code, value, self.now());
                    if value == 1 && !kc::is_modifier(code) && !kc::is_lock(code) {
                        self.forget("ввод во время исправления");
                    }
                    return;
                }
            } else if seq > b.until_seq {
                self.busy = None;
            }
        }
        if value == 1 && kc::is_edit(code) {
            self.last_edit = Some(Instant::now());
        }
        if let Some(f) = &self.focus {
            if value == 1 {
                f.poke();
            }
        }
        if self.locked {
            self.buffer.note(code, value, device);
            return;
        }
        let gesture = if self.cfg.gesture.manual == "double_shift" {
            self.gesture.on_key(code, value, self.now())
        } else {
            None
        };
        if self.paused {
            self.buffer.note(code, value, device);
            return;
        }

        let verdict = self.verdict();
        if value != 0 && self.must_not_buffer(verdict) {
            self.buffer.note(code, value, device);
            if !self.buffer.is_empty() || self.last_fix.is_some() {
                self.forget("поле пароля или чувствительная программа");
            }
            if let Some(g) = gesture {
                self.manual(g, None);
            }
            return;
        }
        let event = self.buffer.feed(&self.det.keymap, code, value, device, self.now());
        if let Some(g) = gesture {
            self.trigger = Some((seq, presses, (device, code)));
            self.manual(g, Some(presses));
            return;
        }
        let Some(event) = event else { return };
        match event {
            BufferEvent::Reset => {
                self.context.clear();
                self.taint_hard = false;
                self.taint_unknown = false;
            }
            BufferEvent::Letter(keys) => {
                self.note_taint(verdict);
                if self.cfg.general.auto_correct && !self.taint_hard && !self.taint_unknown && self.auto_ok(verdict) {
                    self.trigger = Some((seq, presses, (device, code)));
                    self.on_letter(&keys);
                }
            }
            BufferEvent::Word(keys) => {
                let (hard, unknown) = (self.taint_hard, self.taint_unknown);
                self.taint_hard = false;
                self.taint_unknown = false;
                if !self.cfg.general.auto_correct || hard || !self.auto_ok(verdict) {
                    self.context.push_back(None);
                    return;
                }
                if unknown && !self.wait_fresh_allow() {
                    self.context.push_back(None);
                    return;
                }
                self.trigger = Some((seq, presses, (device, code)));
                self.on_word(&keys);
            }
        }
    }

    fn note_taint(&mut self, v: Verdict) {
        match v {
            Verdict::Allow => {}
            Verdict::Unknown => self.taint_unknown = true,
            Verdict::Terminal if self.cfg.security.terminals == TerminalMode::Auto => {}
            _ => self.taint_hard = true,
        }
    }

    /// Начало слова набрано, пока снимок фокуса устарел (поток опроса спал):
    /// ждём свежий снимок и исправляем, только если он разрешает.
    fn wait_fresh_allow(&self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(200);
        loop {
            let v = self.verdict();
            if v != Verdict::Unknown {
                return self.auto_ok(v);
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn groups(&self) -> Option<(usize, usize)> {
        let current = self.backend.current()?;
        let alpha = self.det.keymap.alphabet(current);
        if alpha == Alphabet::Other {
            return None;
        }
        let other = (0..self.det.keymap.num_groups())
            .find(|&g| g != current && self.det.keymap.alphabet(g) != alpha && self.det.keymap.alphabet(g) != Alphabet::Other)?;
        Some((current, other))
    }

    fn edit_guard(&self) -> bool {
        let window = Duration::from_secs_f64(self.cfg.general.edit_guard_sec);
        [self.last_edit, self.last_external_layout].iter().flatten().any(|t| t.elapsed() < window)
    }

    fn context_tuple(&self) -> (Option<Lang>, Option<Lang>) {
        let n = self.context.len();
        (
            if n >= 2 { self.context[n - 2] } else { None },
            if n >= 1 { self.context[n - 1] } else { None },
        )
    }

    fn letters(text: &str) -> SecretText {
        text.chars().filter(|c| c.is_alphabetic() || *c == '-' || *c == '\'').flat_map(char::to_lowercase).collect::<String>().into()
    }

    fn on_letter(&mut self, keys: &[KeyPress]) {
        let Some((current, other)) = self.groups() else { return };
        if self.edit_guard() {
            return;
        }
        if let Some(target) = self.det.early_url_target(keys, other) {
            self.run_fix(keys, 0, FixReq::new(keys.len(), target, other, current), "url-early");
            return;
        }
        let g = &self.cfg.general;
        if !g.early_switch || g.sensitivity == "conservative" {
            return;
        }
        if let Some(target) = self.det.early_word_target(keys, current, other) {
            let req = FixReq::new(keys.len(), target, other, current);
            self.run_fix(keys, 0, req, "early");
        }
    }

    fn on_word(&mut self, keys: &[KeyPress]) {
        let Some((current, other)) = self.groups() else { return };
        if self.edit_guard() {
            crate::debug!("слово пропущено: только что была правка текста");
            self.context.push_back(None);
            return;
        }
        let word = Self::letters(self.det.decode(keys, current).expose());
        if !word.expose().is_empty() && self.det.exceptions.contains(word.expose()) {
            self.context.push_back(None);
            return;
        }
        let n = keys.len() + 1;
        let g = self.cfg.general.clone();
        let same = |text: String| FixReq { switch: false, ..FixReq::new(n, text.into(), current, current) };
        if g.punct_fix {
            if let Some(mark) = self.det.punct_fix(keys, current, other) {
                let stem = self.det.decode(&keys[..keys.len() - 1], current);
                self.run_fix(keys, 1, same(format!("{}{mark} ", stem.expose())), "punct");
                return;
            }
        }
        if g.split_words_fix {
            if let Some(t) = self.det.split_words_fix(keys, current) {
                self.run_fix(keys, 1, same(format!("{} ", t.expose())), "split");
                return;
            }
        }
        if g.particles_fix {
            if let Some(t) = self.det.particles_fix(keys, current) {
                self.run_fix(keys, 1, same(format!("{} ", t.expose())), "particles");
                return;
            }
        }
        if g.caps_fix && self.buffer.caps {
            if let Some((t, group)) = self.det.caps_lock_fix(keys, current, other) {
                let req = FixReq {
                    switch: group != current,
                    caps_off: true,
                    ..FixReq::new(n, format!("{} ", t.expose()).into(), group, current)
                };
                self.run_fix(keys, 1, req, "caps-lock");
                return;
            }
        }
        if g.two_capitals_fix && !self.buffer.caps {
            if let Some(t) = self.det.two_capitals_fix(keys, current) {
                self.run_fix(keys, 1, same(format!("{} ", t.expose())), "capitals");
                return;
            }
        }
        let d = self.det.decide(keys, current, other, self.context_tuple());
        crate::debug!("слово ({} симв.) → {:?} ({})", keys.len(), d.action, d.reason);
        if d.action == Action::Fix {
            let req = FixReq::new(n, format!("{} ", d.target.expose()).into(), other, current);
            if self.run_fix(keys, 1, req, "auto") == Outcome::Done {
                self.context.push_back(d.b_lang);
            }
            return;
        }
        self.context.push_back(if d.a_in_dict { d.a_lang } else { None });
        while self.context.len() > 4 {
            self.context.pop_front();
        }
    }

    // --- ручной режим --------------------------------------------------------------

    pub fn manual(&mut self, g: Gesture, presses: Option<u64>) {
        if self.locked || self.paused {
            return;
        }
        // Запоминаем только по жесту с клавиатуры: вызов по D-Bus может прийти
        // от любой программы и заставить демон записать хеш набранного слова.
        self.physical_gesture = presses.is_some();
        if presses.is_none() {
            self.trigger = None;
        }
        match g {
            Gesture::Word => self.manual_word(),
            Gesture::Phrase => self.manual_phrase(),
        }
    }

    fn manual_word(&mut self) {
        let Some((current, other)) = self.groups() else { return };
        self.buffer.expire(self.now());
        let (word, tail) = self.buffer.last_word_with_tail();
        if word.is_empty() {
            // Слова нет (или мы в поле пароля и ничего не храним) — только
            // переключить раскладку.
            if !self.dry_run {
                let _ = self.backend.set(other);
                self.backend.wait_applied(other, self.settle());
            }
            self.publish();
            return;
        }
        // Отмена последнего автоисправления.
        let undo = self.last_fix.as_ref().filter(|l| {
            l.at.elapsed().as_secs_f64() <= self.cfg.general.undo_window_sec
                && self.buffer.ends_with_before_spaces(&l.target_keys) == Some(tail)
        });
        if let Some(last) = undo {
            let original = self.det.decode(&last.original, last.group_before);
            let mut text = original.expose().to_string();
            text.extend(std::iter::repeat_n(' ', tail));
            let req = FixReq::new(last.target_keys.len() + tail, text.into(), last.group_before, current);
            let learnable = last.learnable;
            let original_keys = last.original.clone();
            if self.run_fix(&original_keys, 0, req, "undo") == Outcome::Done {
                self.status.lock().unwrap().undos += 1;
                self.last_fix = None;
                if learnable && self.physical_gesture {
                    self.note_undo(&Self::letters(original.expose()));
                }
            }
            return;
        }
        let text = self.det.decode(&word, other);
        let mut spaced = text.expose().to_string();
        spaced.extend(std::iter::repeat_n(' ', tail));
        let req = FixReq::new(word.len() + tail, spaced.into(), other, current);
        let mut all: Vec<KeyPress> = word.to_vec();
        all.extend(std::iter::repeat_n(crate::keymap::SPACE, tail));
        self.run_fix(&all, tail, req, "manual");
    }

    fn manual_phrase(&mut self) {
        let Some((current, other)) = self.groups() else { return };
        self.buffer.expire(self.now());
        let phrase = self.buffer.phrase();
        if phrase.is_empty() {
            return;
        }
        let text = self.det.decode(&phrase, other);
        let req = FixReq::new(phrase.len(), text, other, current);
        self.run_fix(&phrase, 0, req, "phrase");
    }

    fn note_undo(&mut self, word: &SecretText) {
        if word.expose().is_empty() || !self.cfg.security.learn_words {
            return;
        }
        let mut h = DefaultHasher::new();
        word.expose().hash(&mut h);
        let key = h.finish();
        let count = self.undo_counts.entry(key).or_default();
        *count += 1;
        if *count < self.cfg.general.learn_after_undos {
            crate::debug!("исправление отменено; слово запомнится после ещё {} отмен", self.cfg.general.learn_after_undos - *count);
            return;
        }
        self.undo_counts.remove(&key);
        if let Some(store) = self.det.exceptions.learned.as_mut() {
            match store.add(word.expose()) {
                Ok(()) => crate::debug!("слово запомнено (хранится как HMAC)"),
                Err(e) => crate::warn!("не удалось сохранить выученное слово: {e}"),
            }
        }
        self.publish();
    }

    // --- исполнение --------------------------------------------------------------

    fn settle(&self) -> Duration {
        Duration::from_millis(self.cfg.typing.settle_ms)
    }

    /// Исправить: `keys` — нажатия, которые сейчас на экране (без хвоста),
    /// `tail` — сколько пробелов после них входит в `req.delete`.
    fn run_fix(&mut self, keys: &[KeyPress], tail: usize, req: FixReq, reason: &str) -> Outcome {
        let learnable = reason == "auto" && !self.taint_hard;
        // Последняя проверка перед автоисправлением: в поле уже есть символы,
        // и Qt-поле пароля выдаёт себя маскированным текстом.
        if !matches!(reason, "undo" | "manual" | "phrase") && self.focus.as_ref().is_some_and(|f| f.masked_now()) {
            self.forget("поле маскирует текст");
            return Outcome::Aborted;
        }
        if self.dry_run {
            crate::info!("[dry-run] {reason}: {} → {} симв.", keys.len() + tail, req.text.chars());
            return Outcome::Aborted;
        }
        let trigger = self.trigger.take();
        let presses0 = trigger.map_or_else(|| self.phys.presses.load(Ordering::SeqCst), |t| t.1);
        let after_seq = trigger.map_or_else(|| self.phys.seq.load(Ordering::SeqCst), |t| t.0);
        let started = Instant::now();
        let outcome = self.execute(&req, presses0, trigger.map(|t| t.2));
        self.busy = Some(Busy {
            after_seq,
            until_seq: self.phys.seq.load(Ordering::SeqCst),
            aborted: outcome != Outcome::Done,
        });
        match outcome {
            Outcome::Done => {
                crate::debug!("исправление {reason}: {} мс", started.elapsed().as_millis());
                if req.caps_off {
                    self.buffer.caps = false;
                }
                let original: Keys = zeroize::Zeroizing::new(keys.to_vec());
                match self.det.keymap.presses_for(req.text.expose(), req.target) {
                    Some(new) => {
                        self.buffer.replace_tail(req.delete, &new);
                        // Для отмены храним нажатия без хвостовых пробелов.
                        let trimmed = new.iter().rposition(|k| k.code != kc::KEY_SPACE).map_or(0, |i| i + 1);
                        let orig_trim = original.iter().rposition(|k| k.code != kc::KEY_SPACE).map_or(0, |i| i + 1);
                        self.last_fix = (reason != "undo").then(|| LastFix {
                            original: zeroize::Zeroizing::new(original[..orig_trim].to_vec()),
                            target_keys: new[..trimmed].to_vec(),
                            group_before: req.restore.unwrap_or(req.target),
                            at: Instant::now(),
                            learnable,
                        });
                    }
                    None => self.forget("текст после исправления не раскладывается в нажатия"),
                }
                self.status.lock().unwrap().fixes += 1;
            }
            Outcome::Aborted => crate::debug!("исправление {reason} прервано вводом"),
            Outcome::Failed => {
                crate::warn!("исправление не удалось");
                self.forget("ошибка исправления");
            }
        }
        self.gesture.reset();
        self.publish();
        outcome
    }

    fn typed_since(&self, presses0: u64) -> bool {
        self.phys.presses.load(Ordering::SeqCst) != presses0
    }

    fn execute(&mut self, req: &FixReq, presses0: u64, trigger: Option<(u32, u16)>) -> Outcome {
        // Набрали что-то после клавиши, на которой приняли решение, — каретка
        // уже не там, где мы думаем.
        if self.typed_since(presses0) {
            return Outcome::Aborted;
        }
        let deadline = Instant::now() + RELEASE_WAIT;
        while self.phys.blocking_held(trigger) {
            if Instant::now() > deadline || self.typed_since(presses0) {
                return Outcome::Aborted;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let text_typist = self.backend.text_typist().is_some();
        let keys = if text_typist {
            None
        } else {
            match self.det.keymap.keys_for_typing(req.text.expose(), req.target, self.buffer.caps && !req.caps_off) {
                Some(k) => Some(k),
                None => return Outcome::Failed,
            }
        };
        if !text_typist && self.typist.is_none() {
            return Outcome::Failed;
        }
        if let Some(t) = self.typist.as_mut() {
            for code in kc::ALL_MODIFIERS {
                let _ = t.key(code, 0);
            }
        }
        let switch = req.switch || self.backend.always_switch();
        if switch && (self.backend.set(req.target).is_err() || !self.backend.wait_applied(req.target, self.settle())) {
            self.rollback(req, switch);
            return Outcome::Failed;
        }
        if self.typed_since(presses0) {
            self.rollback(req, switch);
            return Outcome::Aborted;
        }
        if req.caps_off {
            if let Some(t) = self.typist.as_mut() {
                let _ = t.key(kc::KEY_CAPSLOCK, 1);
                let _ = t.key(kc::KEY_CAPSLOCK, 0);
            }
        }
        // Точка невозврата: дальше доводим до конца.
        let delay = Duration::from_millis(self.cfg.typing.key_delay_ms);
        let result = if let Some(tt) = self.backend.text_typist() {
            tt.backspace(req.delete as u32).and_then(|_| tt.type_text(req.text.expose()))
        } else {
            let t = self.typist.as_mut().unwrap();
            let mut tap = |code: u16, shift: bool| -> std::io::Result<()> {
                if shift {
                    t.key(kc::KEY_LEFTSHIFT, 1)?;
                }
                t.key(code, 1)?;
                t.key(code, 0)?;
                if shift {
                    t.key(kc::KEY_LEFTSHIFT, 0)?;
                }
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                Ok(())
            };
            let mut r = Ok(());
            for _ in 0..req.delete {
                r = r.and_then(|_| tap(kc::KEY_BACKSPACE, false));
            }
            for &(code, shift) in keys.as_deref().unwrap_or(&[]) {
                r = r.and_then(|_| tap(code, shift));
            }
            for code in kc::ALL_MODIFIERS {
                let _ = t.key(code, 0);
            }
            r.map_err(|e| e.to_string())
        };
        match result {
            Ok(()) => Outcome::Done,
            Err(e) => {
                crate::error!("печать прервана ошибкой: {e}");
                Outcome::Failed
            }
        }
    }

    fn rollback(&self, req: &FixReq, switched: bool) {
        if let (true, Some(g)) = (switched, req.restore) {
            if g != req.target {
                let _ = self.backend.set(g);
            }
        }
    }
}

impl FixReq {
    fn new(delete: usize, text: SecretText, target: usize, current: usize) -> Self {
        FixReq { delete, text, target, switch: target != current, caps_off: false, restore: Some(current) }
    }
}

fn terminators(cfg: &Config) -> std::collections::HashSet<u16> {
    let mut t = std::collections::HashSet::new();
    if cfg.general.fix_on_tab {
        t.insert(kc::KEY_TAB);
    }
    if cfg.general.fix_on_enter {
        t.insert(kc::KEY_ENTER);
        t.insert(kc::KEY_KPENTER);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Result as BResult, TextTypist};
    use crate::detector::{Exceptions, Sensitivity};
    use crate::keymap::testing::us_ru;
    use crate::keymap::LayoutSpec;

    /// Бэкенд-заглушка: раскладка в памяти, печать — в журнал.
    struct Fake {
        current: Mutex<usize>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl LayoutBackend for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn layouts(&self) -> Vec<LayoutSpec> {
            vec![LayoutSpec::new("us", ""), LayoutSpec::new("ru", "")]
        }
        fn xkb_options(&self) -> Vec<String> {
            Vec::new()
        }
        fn current(&self) -> Option<usize> {
            Some(*self.current.lock().unwrap())
        }
        fn set(&self, index: usize) -> BResult<()> {
            *self.current.lock().unwrap() = index;
            self.log.lock().unwrap().push(format!("set {index}"));
            Ok(())
        }
        fn wait_applied(&self, _: usize, _: Duration) -> bool {
            true
        }
        fn text_typist(&self) -> Option<&dyn TextTypist> {
            Some(self)
        }
    }

    impl TextTypist for Fake {
        fn backspace(&self, count: u32) -> BResult<()> {
            self.log.lock().unwrap().push(format!("bs {count}"));
            Ok(())
        }
        fn type_text(&self, text: &str) -> BResult<()> {
            self.log.lock().unwrap().push(format!("type {text}"));
            Ok(())
        }
    }

    fn controller(focus: Option<Arc<FocusTracker>>, cfg: Config) -> (Controller, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let backend = Box::new(Fake { current: Mutex::new(0), log: log.clone() });
        let det = Detector {
            keymap: us_ru(),
            models: crate::lang::Models::builtin(),
            exceptions: Exceptions::default(),
            sensitivity: Sensitivity::named("normal"),
        };
        let mut cfg = cfg;
        cfg.typing.settle_ms = 0;
        cfg.general.edit_guard_sec = 0.0;
        let c = Controller::new(cfg, det, backend, None, focus, Arc::new(Phys::default()), Arc::default(), false);
        (c, log)
    }

    fn type_text(c: &mut Controller, text: &str) {
        let keys = c.det.keymap.presses_for(text, 0).unwrap();
        for k in keys {
            if k.shift {
                c.on_key(kc::KEY_LEFTSHIFT, 1, 1, 0, 0);
            }
            c.on_key(k.code, 1, 1, 0, 0);
            c.on_key(k.code, 0, 1, 0, 0);
            if k.shift {
                c.on_key(kc::KEY_LEFTSHIFT, 0, 1, 0, 0);
            }
        }
    }

    fn double_shift(c: &mut Controller) {
        for _ in 0..2 {
            c.on_key(kc::KEY_LEFTSHIFT, 1, 1, 0, 0);
            c.on_key(kc::KEY_LEFTSHIFT, 0, 1, 0, 0);
        }
    }

    fn no_early() -> Config {
        let mut cfg = Config::default();
        cfg.general.early_switch = false;
        cfg
    }

    #[test]
    fn fixes_in_ordinary_field() {
        let (mut c, log) = controller(Some(FocusTracker::fake("gedit", false, false)), no_early());
        type_text(&mut c, "ghbdtn ");
        assert_eq!(*log.lock().unwrap(), ["set 1", "bs 7", "type привет "]);
    }

    #[test]
    fn password_field_is_never_buffered_or_fixed() {
        let focus = FocusTracker::fake("firefox", true, false);
        let (mut c, log) = controller(Some(focus), no_early());
        type_text(&mut c, "ghbdtn ");
        assert!(log.lock().unwrap().is_empty());
        assert!(c.buffer.is_empty(), "нажатия в поле пароля не хранятся");
        // Двойной Shift в поле пароля только переключает раскладку.
        double_shift(&mut c);
        assert_eq!(*log.lock().unwrap(), ["set 1"]);
    }

    #[test]
    fn word_started_in_password_field_is_not_fixed() {
        let focus = FocusTracker::fake("firefox", false, false);
        let (mut c, log) = controller(Some(focus.clone()), no_early());
        focus.set_fake(true);
        type_text(&mut c, "ghb");
        focus.set_fake(false);
        type_text(&mut c, "dtn ");
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_focus_skips_by_default() {
        let (mut c, log) = controller(None, no_early());
        type_text(&mut c, "ghbdtn ");
        assert!(log.lock().unwrap().is_empty());
        let mut cfg = no_early();
        cfg.security.unknown_focus = UnknownFocus::Allow;
        let (mut c, log) = controller(None, cfg);
        type_text(&mut c, "ghbdtn ");
        assert_eq!(log.lock().unwrap().len(), 3);
    }

    #[test]
    fn masked_field_is_not_fixed_and_buffer_is_wiped() {
        let focus = FocusTracker::fake("kdialog", false, false);
        focus.set_fake_masked(true);
        let (mut c, log) = controller(Some(focus), no_early());
        type_text(&mut c, "ghbdtn ");
        assert!(log.lock().unwrap().is_empty());
        assert!(c.buffer.is_empty());
    }

    #[test]
    fn terminal_is_manual_only() {
        let (mut c, log) = controller(Some(FocusTracker::fake("kgx", false, true)), no_early());
        type_text(&mut c, "ghbdtn ");
        assert!(log.lock().unwrap().is_empty());
        double_shift(&mut c);
        assert_eq!(*log.lock().unwrap(), ["set 1", "bs 7", "type привет "]);
    }

    #[test]
    fn sensitive_app_does_nothing() {
        let (mut c, log) = controller(Some(FocusTracker::fake("KeePassXC", false, false)), no_early());
        type_text(&mut c, "ghbdtn ");
        assert!(c.buffer.is_empty());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn undo_over_dbus_is_not_learned() {
        let dir = std::env::temp_dir().join(format!("wsrs-dbus-{}", std::process::id()));
        let (mut c, log) = controller(Some(FocusTracker::fake("gedit", false, false)), no_early());
        c.det.exceptions.learned = Some(crate::secure::LearnedStore::open(&dir).unwrap());
        for _ in 0..3 {
            type_text(&mut c, "ghbdtn ");
            c.manual(Gesture::Word, None);
            c.forget("следующая попытка");
        }
        assert_eq!(c.det.exceptions.learned.as_ref().unwrap().len(), 0);
        log.lock().unwrap().clear();
        type_text(&mut c, "ghbdtn ");
        assert_eq!(log.lock().unwrap().len(), 3, "слово не выучено и исправляется");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undo_restores_and_learns_after_two() {
        let dir = std::env::temp_dir().join(format!("wsrs-ctl-{}", std::process::id()));
        let (mut c, log) = controller(Some(FocusTracker::fake("gedit", false, false)), no_early());
        c.det.exceptions.learned = Some(crate::secure::LearnedStore::open(&dir).unwrap());
        for _ in 0..2 {
            log.lock().unwrap().clear();
            type_text(&mut c, "ghbdtn ");
            double_shift(&mut c);
            let l = log.lock().unwrap().clone();
            assert_eq!(l, ["set 1", "bs 7", "type привет ", "set 0", "bs 7", "type ghbdtn "]);
            c.forget("следующая попытка");
        }
        log.lock().unwrap().clear();
        type_text(&mut c, "ghbdtn ");
        assert!(log.lock().unwrap().is_empty(), "слово выучено");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
