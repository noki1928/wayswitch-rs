//! Чтение клавиатур и мышей через evdev: по потоку на устройство.
//!
//! Устройства определяются по возможностям, не по имени; горячее
//! подключение — inotify на /dev/input. Своя виртуальная клавиатура не
//! читается.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evdev::{Device, InputEventKind, Key, LedType};

use crate::keycodes as kc;
use crate::Event;

const INPUT_DIR: &str = "/dev/input";

/// Физическое состояние клавиатур, общее для потоков чтения и исполнителя
/// исправления (тому нужно видеть нажатия, пока он сам печатает).
#[derive(Default)]
pub struct Phys {
    pub seq: AtomicU64,
    /// Счётчик нажатий не-модификаторов: изменился — человек печатает.
    pub presses: AtomicU64,
    pub held: Mutex<HashSet<(u32, u16)>>,
}

impl Phys {
    /// Зажато что-то, кроме Ctrl/Alt/Super (и кроме `except`).
    pub fn blocking_held(&self, except: Option<(u32, u16)>) -> bool {
        self.held
            .lock()
            .unwrap()
            .iter()
            .any(|&k| Some(k) != except && !kc::is_command_modifier(k.1))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Keyboard,
    Pointer,
}

struct Watcher {
    tx: Sender<Event>,
    phys: Arc<Phys>,
    open: Mutex<HashSet<PathBuf>>,
    next_id: AtomicU32,
}

pub fn start(tx: Sender<Event>, phys: Arc<Phys>) {
    let w = Arc::new(Watcher { tx, phys, open: Mutex::new(HashSet::new()), next_id: AtomicU32::new(1) });
    rescan(&w);
    std::thread::Builder::new()
        .name("hotplug".into())
        .spawn(move || hotplug(w))
        .expect("поток hotplug");
}

fn rescan(w: &Arc<Watcher>) {
    let Ok(entries) = std::fs::read_dir(INPUT_DIR) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        open(w, &p);
    }
}

fn hotplug(w: Arc<Watcher>) {
    let mut inotify = match inotify::Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            crate::warn!("inotify: {e}; новые клавиатуры — только после перезапуска");
            return;
        }
    };
    if inotify.watches().add(INPUT_DIR, inotify::WatchMask::CREATE | inotify::WatchMask::ATTRIB).is_err() {
        return;
    }
    let mut buf = [0u8; 4096];
    loop {
        let Ok(events) = inotify.read_events_blocking(&mut buf) else { return };
        let names: Vec<PathBuf> = events.filter_map(|e| e.name.map(|n| Path::new(INPUT_DIR).join(n))).collect();
        // udev успевает выставить права.
        std::thread::sleep(Duration::from_millis(300));
        for p in names {
            open(&w, &p);
        }
    }
}

fn classify(dev: &Device) -> Option<Kind> {
    if dev.name() == Some(crate::typist::VIRTUAL_NAME) {
        return None;
    }
    let keys = dev.supported_keys()?;
    if [Key::KEY_A, Key::KEY_Z, Key::KEY_SPACE, Key::KEY_ENTER].iter().all(|k| keys.contains(*k)) {
        Some(Kind::Keyboard)
    } else if keys.contains(Key::BTN_LEFT) || keys.contains(Key::BTN_TOUCH) {
        Some(Kind::Pointer)
    } else {
        None
    }
}

fn open(w: &Arc<Watcher>, path: &Path) {
    if !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")) {
        return;
    }
    if w.open.lock().unwrap().contains(path) {
        return;
    }
    let dev = match crate::secure::with_input_group(|| Device::open(path)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            crate::warn!("нет доступа к {}: см. `wayswitch-rs doctor`", path.display());
            return;
        }
        Err(_) => return,
    };
    let Some(kind) = classify(&dev) else { return };
    w.open.lock().unwrap().insert(path.to_path_buf());
    let id = w.next_id.fetch_add(1, Ordering::Relaxed);
    crate::info!(
        "слушаю {}: {} ({})",
        if kind == Kind::Keyboard { "клавиатуру" } else { "указатель" },
        dev.name().unwrap_or("?"),
        path.display()
    );
    let (w2, path) = (w.clone(), path.to_path_buf());
    std::thread::Builder::new()
        .name(format!("input-{id}"))
        .spawn(move || {
            read_loop(&w2, dev, id, kind);
            w2.open.lock().unwrap().remove(&path);
            w2.phys.held.lock().unwrap().retain(|&(d, _)| d != id);
            if kind == Kind::Keyboard {
                let _ = w2.tx.send(Event::DeviceGone(id));
            }
            crate::info!("устройство отключено: {}", path.display());
        })
        .expect("поток устройства");
}

fn read_loop(w: &Watcher, mut dev: Device, id: u32, kind: Kind) {
    loop {
        let events: Vec<_> = match dev.fetch_events() {
            Ok(evs) => evs.collect(),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        for ev in events {
            let msg = match ev.kind() {
                InputEventKind::RelAxis(axis) if kc::is_wheel(axis.0) => Event::Scroll,
                InputEventKind::Key(key) => {
                    let (code, value) = (key.code(), ev.value());
                    if kc::is_pointer_button(code) {
                        if value != 1 {
                            continue;
                        }
                        Event::Click
                    } else if kind != Kind::Keyboard {
                        continue;
                    } else {
                        {
                            let mut held = w.phys.held.lock().unwrap();
                            match value {
                                0 => {
                                    held.remove(&(id, code));
                                }
                                1 => {
                                    held.insert((id, code));
                                }
                                _ => {}
                            }
                        }
                        if value != 0 && !kc::is_modifier(code) && !kc::is_lock(code) {
                            w.phys.presses.fetch_add(1, Ordering::SeqCst);
                        }
                        // Счётчик нажатий на момент события: если к началу
                        // исправления он вырос, человек успел напечатать ещё.
                        let presses = w.phys.presses.load(Ordering::SeqCst);
                        let seq = w.phys.seq.fetch_add(1, Ordering::SeqCst) + 1;
                        if w.tx.send(Event::Key { code, value, device: id, seq, presses }).is_err() {
                            return;
                        }
                        if code == kc::KEY_CAPSLOCK && value == 0 {
                            if let Ok(leds) = dev.get_led_state() {
                                let _ = w.tx.send(Event::Caps(leds.contains(LedType::LED_CAPSL)));
                            }
                        }
                        continue;
                    }
                }
                _ => continue,
            };
            if w.tx.send(msg).is_err() {
                return;
            }
        }
    }
}
