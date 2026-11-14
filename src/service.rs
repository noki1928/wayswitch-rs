//! D-Bus-сервис демона: управление и статус для CLI.
//!
//! В отличие от WaySwitch, сервис не рассылает сигнал `Corrected` с
//! исходным и исправленным текстом: его мог слушать любой процесс на
//! сессионной шине, а при неточном распознавании поля пароля туда уходил
//! бы и пароль. Статус — только флаги и счётчики.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use zbus::zvariant::{OwnedValue, Value};

use crate::controller::Status;
use crate::{Cmd, Event};

pub const BUS_NAME: &str = "ru.siberia.WaySwitch";
pub const OBJECT_PATH: &str = "/ru/siberia/WaySwitch";
pub const INTERFACE: &str = "ru.siberia.WaySwitch.Daemon";

pub struct Service {
    tx: Mutex<Sender<Event>>,
    status: Arc<Mutex<Status>>,
}

impl Service {
    fn send(&self, cmd: Cmd) -> bool {
        self.tx.lock().unwrap().send(Event::Cmd(cmd)).is_ok()
    }
}

fn v<'a>(x: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(x.into()).expect("простое значение")
}

#[zbus::interface(name = "ru.siberia.WaySwitch.Daemon")]
impl Service {
    fn pause(&self) {
        self.send(Cmd::Pause);
    }

    fn resume(&self) {
        self.send(Cmd::Resume);
    }

    fn fix_last_word(&self) -> bool {
        self.send(Cmd::FixWord)
    }

    fn fix_phrase(&self) -> bool {
        self.send(Cmd::FixPhrase)
    }

    fn reload(&self) {
        self.send(Cmd::Reload);
    }

    fn forget_learned(&self) {
        self.send(Cmd::ForgetLearned);
    }

    fn get_status(&self) -> HashMap<String, OwnedValue> {
        let s = self.status.lock().unwrap().clone();
        HashMap::from([
            ("paused".into(), v(s.paused)),
            ("locked".into(), v(s.locked)),
            ("backend".into(), v(s.backend)),
            ("layout".into(), v(s.layout)),
            ("auto_correct".into(), v(s.auto_correct)),
            ("focus".into(), v(s.focus)),
            ("fixes".into(), v(s.fixes)),
            ("undos".into(), v(s.undos)),
            ("learned".into(), v(s.learned)),
            ("setgid".into(), v(s.setgid)),
            ("atspi_events".into(), v(s.atspi_events)),
            ("gestures".into(), v(s.gestures)),
            ("gestures_no_word".into(), v(s.gestures_no_word)),
            ("focus_resets".into(), v(s.focus_resets)),
        ])
    }
}

/// Занять имя на шине. Ошибка — уже запущен другой экземпляр.
pub fn start(tx: Sender<Event>, status: Arc<Mutex<Status>>) -> zbus::Result<zbus::blocking::Connection> {
    let svc = Service { tx: Mutex::new(tx), status };
    let conn = zbus::blocking::connection::Builder::session()?.serve_at(OBJECT_PATH, svc)?.build()?;
    // Обычный Builder::name просит ReplaceExisting и разрешает замену: второй
    // запуск отбирал имя у работающего демона. Здесь — ни того, ни другого.
    conn.request_name_with_flags(BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())?;
    Ok(conn)
}

/// Клиент для CLI.
pub fn call(method: &str) -> Result<zbus::Message, String> {
    let conn = zbus::blocking::Connection::session().map_err(|e| e.to_string())?;
    conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, &())
        .map_err(|e| format!("демон не отвечает: {e}"))
}
