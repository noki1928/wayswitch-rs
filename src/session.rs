//! Охрана сессии: пока экран заблокирован, демон не исправляет и не
//! буферизует ничего — иначе пароль, набранный на экране блокировки, был
//! бы «исправлен» прямо в поле пароля.
//!
//! Источники (блокировка = любой из них): logind (LockedHint, Active),
//! служба хранителя экрана (GNOME/freedesktop) и процессы экранов
//! блокировки Wayland (swaylock, hyprlock…), о которых logind не знает.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::mpsc::Sender;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::Event;

const POLL: Duration = Duration::from_millis(500);
/// Перебор /proc — самая дорогая проверка: раз в несколько опросов.
const PROC_EVERY: u32 = 4;
const LOCKERS: [&str; 9] = [
    "swaylock", "hyprlock", "gtklock", "waylock", "kscreenlocker_g", "kscreenlocker_greet",
    "swaylock-effec", "nwg-lock", "chayang",
];
const SCREENSAVERS: [(&str, &str); 2] = [
    ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
    ("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver"),
];

pub fn start(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("session".into())
        .spawn(move || run(tx))
        .expect("поток охраны сессии");
}

fn run(tx: Sender<Event>) {
    let system = Connection::system().ok();
    let session = Connection::session().ok();
    let logind = system.as_ref().and_then(|c| match find_session(c) {
        Ok(p) => Some(p),
        Err(e) => {
            crate::warn!("logind: своя сессия не найдена ({e}), охрана без него");
            None
        }
    });
    let mut locked = None;
    let (mut tick, mut locker) = (0u32, false);
    loop {
        if tick % PROC_EVERY == 0 {
            locker = locker_running();
        }
        tick = tick.wrapping_add(1);
        let now = logind_locked(system.as_ref(), logind.as_ref()) || screensaver_active(session.as_ref()) || locker;
        if locked != Some(now) {
            locked = Some(now);
            if tx.send(Event::Locked(now)).is_err() {
                return;
            }
        }
        std::thread::sleep(POLL);
    }
}

fn prop<T>(conn: &Connection, path: &str, iface: &str, name: &str) -> Option<T>
where
    T: TryFrom<zbus::zvariant::OwnedValue>,
{
    let reply = conn
        .call_method(Some("org.freedesktop.login1"), path, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, name))
        .ok()?;
    let v: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    T::try_from(v).ok()
}

fn find_session(conn: &Connection) -> zbus::Result<OwnedObjectPath> {
    let manager = ("org.freedesktop.login1", "/org/freedesktop/login1", "org.freedesktop.login1.Manager");
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        let r = conn.call_method(Some(manager.0), manager.1, Some(manager.2), "GetSession", &(id.as_str(),))?;
        return r.body().deserialize();
    }
    // Служба systemd --user живёт вне сессии: берём графическую сессию пользователя.
    let uid = unsafe_getuid();
    let r = conn.call_method(Some(manager.0), manager.1, Some(manager.2), "GetUser", &(uid,))?;
    let user: OwnedObjectPath = r.body().deserialize()?;
    let display: Option<(String, OwnedObjectPath)> = prop(conn, user.as_str(), "org.freedesktop.login1.User", "Display");
    match display {
        Some((_, path)) if path.as_str() != "/" => Ok(path),
        _ => Err(zbus::Error::Failure("нет графической сессии".into())),
    }
}

fn unsafe_getuid() -> u32 {
    // getuid не может завершиться ошибкой.
    #[allow(unsafe_code)]
    unsafe {
        libc::getuid()
    }
}

fn logind_locked(conn: Option<&Connection>, path: Option<&OwnedObjectPath>) -> bool {
    let (Some(conn), Some(path)) = (conn, path) else { return false };
    let iface = "org.freedesktop.login1.Session";
    let hint: bool = prop(conn, path.as_str(), iface, "LockedHint").unwrap_or(false);
    let active: bool = prop(conn, path.as_str(), iface, "Active").unwrap_or(true);
    hint || !active
}

fn screensaver_active(conn: Option<&Connection>) -> bool {
    let Some(conn) = conn else { return false };
    SCREENSAVERS.iter().any(|(name, path)| {
        conn.call_method(Some(*name), *path, Some(*name), "GetActive", &())
            .ok()
            .and_then(|r| r.body().deserialize::<bool>().ok())
            .unwrap_or(false)
    })
}

/// Есть ли среди процессов пользователя экран блокировки.
pub fn locker_running() -> bool {
    let uid = unsafe_getuid();
    let Ok(entries) = fs::read_dir("/proc") else { return false };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        if e.metadata().map(|m| m.uid()).ok() != Some(uid) {
            return false;
        }
        fs::read_to_string(e.path().join("comm")).is_ok_and(|c| LOCKERS.contains(&c.trim()))
    })
}
