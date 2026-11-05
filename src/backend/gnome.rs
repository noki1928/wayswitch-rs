//! GNOME: раскладка и печать через расширение WaySwitch (D-Bus).
//!
//! Используется то же расширение, что и у WaySwitch. Оно пускает к печати
//! только владельца имени `ru.siberia.WaySwitch`, поэтому все вызовы идут
//! через соединение, на котором демон держит это имя.

use std::process::Command;
use std::sync::mpsc::Sender;
use std::time::Duration;

use zbus::blocking::Connection;

use super::{report, LayoutBackend, Result, SharedState, TextTypist};
use crate::keymap::LayoutSpec;
use crate::Event;

const NAME: &str = "ru.siberia.WaySwitch.Shell";
const PATH: &str = "/ru/siberia/WaySwitch/Shell";
const UNKNOWN: u32 = 0xFFFF_FFFF;

pub struct GnomeBackend {
    conn: Connection,
    specs: Vec<LayoutSpec>,
    options: Vec<String>,
    state: SharedState,
    use_text: bool,
}

fn gsettings(key: &str) -> Result<String> {
    // Абсолютный путь и закрытое окно прав: PATH задаёт тот, кто запустил
    // setgid-бинарник, и подменённая программа не должна получить группу input.
    let _closed = crate::secure::privilege_window_closed();
    let out = Command::new("/usr/bin/gsettings")
        .args(["get", "org.gnome.desktop.input-sources", key])
        .output()
        .map_err(|e| format!("gsettings: {e}"))?;
    if !out.status.success() {
        return Err("нет схемы org.gnome.desktop.input-sources".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Строки в одинарных кавычках из вывода gsettings.
fn quoted(s: &str) -> Vec<String> {
    s.split('\'').skip(1).step_by(2).map(str::to_string).collect()
}

/// "[('xkb', 'us'), ('xkb', 'ru')]" → [us, ru].
pub fn parse_sources(s: &str) -> Result<Vec<LayoutSpec>> {
    let items = quoted(s);
    let mut specs = Vec::new();
    for pair in items.chunks(2) {
        let [kind, id] = pair else { break };
        if kind != "xkb" {
            return Err(format!("источник ввода {kind}:{id} — не xkb-раскладка"));
        }
        specs.push(LayoutSpec::parse(id));
    }
    if specs.is_empty() {
        return Err("в gsettings нет источников ввода".into());
    }
    Ok(specs)
}

impl GnomeBackend {
    pub fn new(conn: Connection, tx: Sender<Event>, use_text: bool) -> Result<Self> {
        let has: bool = conn
            .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "NameHasOwner", &(NAME,))
            .and_then(|r| r.body().deserialize())
            .map_err(|e| e.to_string())?;
        if !has {
            return Err("расширение WaySwitch для GNOME Shell не активно".into());
        }
        let specs = parse_sources(&gsettings("sources")?)?;
        let options = quoted(&gsettings("xkb-options").unwrap_or_default());
        let backend = GnomeBackend { conn, specs, options, state: SharedState::default(), use_text };
        backend.state.lock().unwrap().current = backend.query();
        backend.listen(tx);
        Ok(backend)
    }

    fn call<R>(&self, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> Result<R>
    where
        R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        self.conn
            .call_method(Some(NAME), PATH, Some(NAME), method, body)
            .and_then(|r| r.body().deserialize())
            .map_err(|e| e.to_string())
    }

    fn query(&self) -> Option<usize> {
        let (index, _id): (u32, String) = self.call("GetLayout", &()).ok()?;
        (index != UNKNOWN && (index as usize) < self.specs.len()).then_some(index as usize)
    }

    fn listen(&self, tx: Sender<Event>) {
        let conn = self.conn.clone();
        let state = self.state.clone();
        let count = self.specs.len();
        let sources = gsettings("sources").unwrap_or_default();
        std::thread::Builder::new()
            .name("gnome-layout".into())
            .spawn(move || {
                let rule = zbus::MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface(NAME)
                    .and_then(|b| b.member("LayoutChanged"))
                    .map(|b| b.build());
                let Ok(iter) = rule.and_then(|r| zbus::blocking::MessageIterator::for_match_rule(r, &conn, Some(64))) else {
                    crate::error!("GNOME: не подписаться на LayoutChanged");
                    return;
                };
                // Список раскладок поменялся — демону проще перезапуститься.
                let tx2 = tx.clone();
                std::thread::spawn(move || loop {
                    std::thread::sleep(Duration::from_secs(5));
                    if gsettings("sources").unwrap_or_default() != sources {
                        let _ = tx2.send(Event::LayoutsChanged);
                        return;
                    }
                });
                for msg in iter.flatten() {
                    if let Ok(index) = msg.body().deserialize::<u32>() {
                        let index = ((index as usize) < count).then_some(index as usize);
                        report(&state, &tx, index);
                    }
                }
            })
            .expect("поток раскладки GNOME");
    }
}

impl LayoutBackend for GnomeBackend {
    fn name(&self) -> &'static str {
        "gnome"
    }

    fn layouts(&self) -> Vec<LayoutSpec> {
        self.specs.clone()
    }

    fn xkb_options(&self) -> Vec<String> {
        self.options.clone()
    }

    fn current(&self) -> Option<usize> {
        self.state.lock().unwrap().current
    }

    fn set(&self, index: usize) -> Result<()> {
        self.state.lock().unwrap().expected = Some(index);
        if let Err(e) = self.call::<()>("SetLayout", &(index as u32,)) {
            self.state.lock().unwrap().expected = None;
            return Err(e);
        }
        self.state.lock().unwrap().current = Some(index);
        Ok(())
    }

    fn wait_applied(&self, index: usize, settle: Duration) -> bool {
        std::thread::sleep(settle);
        self.current() == Some(index)
    }

    fn text_typist(&self) -> Option<&dyn TextTypist> {
        self.use_text.then_some(self as &dyn TextTypist)
    }
}

impl TextTypist for GnomeBackend {
    fn backspace(&self, count: u32) -> Result<()> {
        self.call::<()>("Backspace", &(count,))
    }

    fn type_text(&self, text: &str) -> Result<()> {
        // Расширение не пишет печатаемый текст в журнал; мы — тоже.
        self.call::<()>("TypeText", &(text,))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        let specs = parse_sources("[('xkb', 'us'), ('xkb', 'ru+phonetic')]").unwrap();
        assert_eq!(specs, vec![LayoutSpec::new("us", ""), LayoutSpec::new("ru", "phonetic")]);
        assert!(parse_sources("[('ibus', 'anthy')]").is_err());
    }
}
