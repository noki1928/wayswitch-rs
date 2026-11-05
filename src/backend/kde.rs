//! KDE Plasma 6: org.kde.keyboard /Layouts (KWin).

use std::sync::mpsc::Sender;
use std::time::Duration;

use zbus::blocking::Connection;

use super::{report, LayoutBackend, Result, SharedState};
use crate::keymap::LayoutSpec;
use crate::Event;

const SERVICE: &str = "org.kde.keyboard";
const OBJECT: &str = "/Layouts";
const IFACE: &str = "org.kde.KeyboardLayouts";

pub struct KdeBackend {
    conn: Connection,
    specs: Vec<LayoutSpec>,
    options: Vec<String>,
    state: SharedState,
}

fn kxkbrc(key: &str) -> Option<String> {
    let path = crate::secure::config_dir().parent()?.join("kxkbrc");
    let text = std::fs::read_to_string(path).ok()?;
    let mut section = "";
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
        } else if section == "Layout" {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

fn layouts_list(conn: &Connection) -> Option<Vec<(String, String, String)>> {
    conn.call_method(Some(SERVICE), OBJECT, Some(IFACE), "getLayoutsList", &())
        .and_then(|r| r.body().deserialize())
        .ok()
}

impl KdeBackend {
    pub fn new(tx: Sender<Event>) -> Result<Self> {
        let conn = Connection::session().map_err(|e| e.to_string())?;
        let rows: Vec<(String, String, String)> = conn
            .call_method(Some(SERVICE), OBJECT, Some(IFACE), "getLayoutsList", &())
            .and_then(|r| r.body().deserialize())
            .map_err(|e| format!("KWin не отвечает: {e}"))?;
        // KWin отдаёт только короткое имя; варианты — из kxkbrc.
        let variants: Vec<String> = kxkbrc("VariantList").map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default();
        let specs: Vec<LayoutSpec> = rows
            .iter()
            .enumerate()
            .map(|(i, (short, _, _))| {
                let variant = if variants.len() == rows.len() { variants[i].trim() } else { "" };
                LayoutSpec::new(short, variant)
            })
            .collect();
        if specs.len() < 2 {
            return Err("в KDE меньше двух раскладок".into());
        }
        let options = kxkbrc("Options")
            .map(|o| o.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        let backend = KdeBackend { conn, specs, options, state: SharedState::default() };
        backend.state.lock().unwrap().current = backend.query();
        backend.listen(tx);
        Ok(backend)
    }

    fn query(&self) -> Option<usize> {
        let index: u32 = self
            .conn
            .call_method(Some(SERVICE), OBJECT, Some(IFACE), "getLayout", &())
            .and_then(|r| r.body().deserialize())
            .ok()?;
        ((index as usize) < self.specs.len()).then_some(index as usize)
    }

    fn listen(&self, tx: Sender<Event>) {
        let conn = self.conn.clone();
        let state = self.state.clone();
        let count = self.specs.len();
        let rows = layouts_list(&self.conn);
        std::thread::Builder::new()
            .name("kde-layout".into())
            .spawn(move || {
                let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).interface(IFACE).map(|b| b.build());
                let Ok(iter) = rule.and_then(|r| zbus::blocking::MessageIterator::for_match_rule(r, &conn, Some(64))) else {
                    crate::error!("KDE: не подписаться на смену раскладки");
                    return;
                };
                for msg in iter.flatten() {
                    match msg.header().member().map(|m| m.as_str()) {
                        Some("layoutChanged") => {
                            if let Ok(index) = msg.body().deserialize::<u32>() {
                                report(&state, &tx, ((index as usize) < count).then_some(index as usize));
                            }
                        }
                        // KWin шлёт его и когда появляется новая клавиатура —
                        // в том числе наша виртуальная. Перезапуск только
                        // если сам список раскладок стал другим.
                        Some("layoutListChanged") if layouts_list(&conn) != rows => {
                            let _ = tx.send(Event::LayoutsChanged);
                        }
                        _ => {}
                    }
                }
            })
            .expect("поток раскладки KDE");
    }
}

impl LayoutBackend for KdeBackend {
    fn name(&self) -> &'static str {
        "kde"
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
        let ok: bool = self
            .conn
            .call_method(Some(SERVICE), OBJECT, Some(IFACE), "setLayout", &(index as u32,))
            .and_then(|r| r.body().deserialize())
            .map_err(|e| e.to_string())?;
        if !ok {
            self.state.lock().unwrap().expected = None;
            return Err("KWin отказал в переключении".into());
        }
        self.state.lock().unwrap().current = Some(index);
        Ok(())
    }

    fn wait_applied(&self, index: usize, settle: Duration) -> bool {
        std::thread::sleep(settle);
        self.query() == Some(index)
    }
}
