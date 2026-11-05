//! Бэкенды раскладки: узнать, переключить, дождаться, подписаться.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::keymap::LayoutSpec;
use crate::Event;

pub mod gnome;
pub mod hyprland;
pub mod kde;
pub mod sway;

pub type Result<T> = std::result::Result<T, String>;

/// Печать текстом (keysym-ами), не зависящая от сканкодов.
pub trait TextTypist {
    fn backspace(&self, count: u32) -> Result<()>;
    fn type_text(&self, text: &str) -> Result<()>;
}

pub trait LayoutBackend: Send {
    fn name(&self) -> &'static str;
    fn layouts(&self) -> Vec<LayoutSpec>;
    fn xkb_options(&self) -> Vec<String>;
    fn current(&self) -> Option<usize>;
    fn set(&self, index: usize) -> Result<()>;
    /// Дождаться, пока композитор применит раскладку.
    fn wait_applied(&self, index: usize, settle: Duration) -> bool;
    /// Переключать перед каждой печатью (у каждого устройства своя группа).
    fn always_switch(&self) -> bool {
        false
    }
    fn text_typist(&self) -> Option<&dyn TextTypist> {
        None
    }
}

/// Общее состояние: текущая раскладка и та, которую ждём от себя.
#[derive(Default)]
pub struct Shared {
    pub current: Option<usize>,
    pub expected: Option<usize>,
}

pub type SharedState = Arc<Mutex<Shared>>;

/// Пришла новая раскладка: обновить состояние и сообщить контроллеру.
pub fn report(state: &SharedState, tx: &Sender<Event>, index: Option<usize>) {
    let mut s = state.lock().unwrap();
    let external = index != s.expected;
    s.expected = None;
    let changed = s.current != index;
    s.current = index;
    drop(s);
    if let (Some(index), true) = (index, changed || external) {
        let _ = tx.send(Event::Layout { index, external });
    }
}

pub fn select(prefer: &str, conn: &zbus::blocking::Connection, tx: Sender<Event>, use_text: bool) -> Result<Box<dyn LayoutBackend>> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    let order: Vec<&str> = match prefer {
        "auto" if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() => vec!["hyprland"],
        "auto" if std::env::var_os("SWAYSOCK").is_some() => vec!["sway"],
        "auto" if desktop.contains("kde") => vec!["kde"],
        "auto" => vec!["gnome", "kde"],
        other => vec![other],
    };
    let mut errors = Vec::new();
    for name in order {
        let made: Result<Box<dyn LayoutBackend>> = match name {
            "gnome" => gnome::GnomeBackend::new(conn.clone(), tx.clone(), use_text).map(|b| Box::new(b) as _),
            "kde" => kde::KdeBackend::new(tx.clone()).map(|b| Box::new(b) as _),
            "sway" => sway::SwayBackend::new(tx.clone()).map(|b| Box::new(b) as _),
            "hyprland" => hyprland::HyprlandBackend::new(tx.clone()).map(|b| Box::new(b) as _),
            _ => Err("неизвестный бэкенд".into()),
        };
        match made {
            Ok(b) => return Ok(b),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }
    Err(errors.join("; "))
}

// --- база XKB: описание → раскладка ------------------------------------------

const RULES_PATHS: [&str; 3] = [
    "/usr/share/X11/xkb/rules/evdev.xml",
    "/usr/share/xkeyboard-config-2/rules/evdev.xml",
    "/usr/local/share/X11/xkb/rules/evdev.xml",
];

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&close)? + start;
    Some(s[start..end].trim())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// «Russian» → ru, «English (Dvorak)» → us(dvorak) по evdev.xml.
pub fn spec_for_description(desc: &str) -> Option<LayoutSpec> {
    let xml = RULES_PATHS.iter().find_map(|p| std::fs::read_to_string(p).ok())?;
    let desc = desc.trim();
    for layout in xml.split("<layout>").skip(1) {
        let layout = layout.split("</layout>").next().unwrap_or("");
        let (head, variants) = layout.split_once("<variantList>").unwrap_or((layout, ""));
        let name = tag(head, "name")?;
        if tag(head, "description").map(unescape).as_deref() == Some(desc) {
            return Some(LayoutSpec::new(name, ""));
        }
        for variant in variants.split("<variant>").skip(1) {
            if tag(variant, "description").map(unescape).as_deref() == Some(desc) {
                return Some(LayoutSpec::new(name, tag(variant, "name")?));
            }
        }
    }
    None
}

/// «us,ru» + «,phonetic» → [us, ru(phonetic)].
pub fn parse_layout_list(layouts: &str, variants: &str) -> Vec<LayoutSpec> {
    let vars: Vec<&str> = variants.split(',').map(str::trim).collect();
    layouts
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, l)| LayoutSpec::new(l, vars.get(i).copied().unwrap_or("")))
        .collect()
}
