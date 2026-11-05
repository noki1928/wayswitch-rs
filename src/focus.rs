//! Что сейчас в фокусе: приложение, поле пароля, терминал — через at-spi.
//!
//! Отличия от WaySwitch, важные для паролей:
//!
//! * подписка на события `object:state-changed:focused`: смена поля видна
//!   сразу, а не через полторы секунды опроса (за это время успевали
//!   набрать и «исправить» начало пароля);
//! * поле пароля распознаётся не только по роли `password text`. Проверено на
//!   живых диалогах: GTK 4 (zenity) и KPasswordDialog (kdialog) эту роль не
//!   ставят и метаданными от обычного поля не отличаются. Поэтому ещё два
//!   признака: маскированный текст (Qt: символы есть, а текст пуст) и подписи
//!   в окне («Пароль», «Password»…). Подпись — эвристика, а не гарантия;
//! * поле пароля в самой оболочке (окна polkit и связки ключей в GNOME
//!   рисует gnome-shell) — тоже пароль; WaySwitch пропускал узлы оболочки и
//!   видел вместо них окно позади диалога;
//! * «не знаю» — отдельное состояние, а не «ограничений нет».

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::Event;

/// Снимок старше этого возраста недостоверен.
pub const MAX_AGE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(1000);
const IDLE_STOP: Duration = Duration::from_secs(30);
const MAX_NODES: usize = 400;
const MAX_DEPTH: usize = 12;
const LABEL_SCAN_NODES: usize = 200;
const STATE_ACTIVE: u32 = 1;
const STATE_FOCUSED: u32 = 12;
const ACC: &str = "org.a11y.atspi.Accessible";

const SHELL_APPS: [&str; 5] = ["gnome-shell", "plasmashell", "kwin", "kwin_wayland", "mutter"];
const PASSWORD_LABELS: [&str; 9] = [
    "password", "passphrase", "passwort", "парол", "пин-код", "pin code", "pin-код",
    "mot de passe", "contraseña",
];

type Node = (String, OwnedObjectPath);


fn has_password_word(text: &str) -> bool {
    PASSWORD_LABELS.iter().any(|p| text.contains(p))
}

#[derive(Clone, Debug, Default)]
pub struct FocusInfo {
    pub app: Option<String>,
    pub password: bool,
    pub terminal: bool,
    pub role: String,
    /// Найден ли сфокусированный узел вообще.
    pub known: bool,
    pub at: Option<Instant>,
    node: Option<Node>,
}

impl FocusInfo {
    pub fn fresh(&self) -> bool {
        self.at.is_some_and(|t| t.elapsed() <= MAX_AGE)
    }
}

pub struct FocusTracker {
    info: Mutex<FocusInfo>,
    healthy: AtomicBool,
    events: AtomicBool,
    last_poke: Mutex<Option<Instant>>,
    wake: Condvar,
    conn: OnceLock<Connection>,
    #[cfg(test)]
    fake_masked: AtomicBool,
}

impl FocusTracker {
    /// Маскирует ли поле в фокусе свой текст прямо сейчас. Вызывается перед
    /// автоисправлением: к этому моменту в поле уже есть символы.
    pub fn masked_now(&self) -> bool {
        #[cfg(test)]
        if self.fake_masked.load(Ordering::Relaxed) {
            return true;
        }
        let Some(conn) = self.conn.get() else { return false };
        let Some(node) = self.snapshot().node else { return false };
        if !(Tree { conn }).text_masked(&node) {
            return false;
        }
        self.info.lock().unwrap().password = true;
        true
    }

    pub fn snapshot(&self) -> FocusInfo {
        self.info.lock().unwrap().clone()
    }

    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn events_working(&self) -> bool {
        self.events.load(Ordering::Relaxed)
    }

    /// Человек печатает: пусть снимок обновляется.
    pub fn poke(&self) {
        let mut last = self.last_poke.lock().unwrap();
        let fresh = self.info.lock().unwrap().at.is_some_and(|t| t.elapsed() < POLL);
        let was_idle = last.is_none_or(|t| t.elapsed() > IDLE_STOP);
        *last = Some(Instant::now());
        if was_idle || !fresh {
            self.wake.notify_all();
        }
    }

    fn set(&self, info: FocusInfo) {
        *self.info.lock().unwrap() = info;
    }
}

/// Запустить отслеживание фокуса. Нет шины доступности — трекер «нездоров».
pub fn start(tx: Sender<Event>) -> Arc<FocusTracker> {
    let tracker = Arc::new(FocusTracker {
        info: Mutex::new(FocusInfo::default()),
        healthy: AtomicBool::new(false),
        events: AtomicBool::new(false),
        last_poke: Mutex::new(None),
        wake: Condvar::new(),
        conn: OnceLock::new(),
        #[cfg(test)]
        fake_masked: AtomicBool::new(false),
    });
    let t = tracker.clone();
    std::thread::Builder::new()
        .name("focus".into())
        .spawn(move || {
            let conn = match a11y_connection() {
                Ok(c) => c,
                Err(e) => {
                    crate::warn!("at-spi недоступен ({e}): поля паролей не распознаются");
                    return;
                }
            };
            t.healthy.store(true, Ordering::Relaxed);
            let _ = t.conn.set(conn.clone());
            let (c2, t2) = (conn.clone(), t.clone());
            std::thread::Builder::new()
                .name("focus-events".into())
                .spawn(move || listen_events(&c2, &t2, tx))
                .expect("поток событий фокуса");
            poll_loop(&conn, &t);
        })
        .expect("поток фокуса");
    tracker
}

fn a11y_connection() -> zbus::Result<Connection> {
    let session = Connection::session()?;
    let reply = session.call_method(Some("org.a11y.Bus"), "/org/a11y/bus", Some("org.a11y.Bus"), "GetAddress", &())?;
    let address: String = reply.body().deserialize()?;
    zbus::blocking::connection::Builder::address(address.as_str())?
        .method_timeout(Duration::from_millis(500))
        .build()
}

// --- события ----------------------------------------------------------------------

fn listen_events(conn: &Connection, t: &FocusTracker, tx: Sender<Event>) {
    let event = "object:state-changed:focused";
    let registry = "org.a11y.atspi.Registry";
    let path = "/org/a11y/atspi/registry";
    // Новые at-spi: RegisterEvent(s, as, s); старые — RegisterEvent(s).
    let registered = conn
        .call_method(Some(registry), path, Some(registry), "RegisterEvent", &(event, Vec::<String>::new(), ""))
        .or_else(|_| conn.call_method(Some(registry), path, Some(registry), "RegisterEvent", &(event,)));
    if let Err(e) = registered {
        crate::warn!("at-spi: подписка на смену фокуса не удалась ({e}), только опрос");
        return;
    }
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.a11y.atspi.Event.Object")
        .and_then(|b| b.member("StateChanged"))
        .and_then(|b| b.arg(0, "focused"))
        .map(|b| b.build());
    let iter = match rule.and_then(|r| zbus::blocking::MessageIterator::for_match_rule(r, conn, Some(256))) {
        Ok(it) => it,
        Err(e) => {
            crate::warn!("at-spi: не подписаться на сигналы ({e}), только опрос");
            return;
        }
    };
    t.events.store(true, Ordering::Relaxed);
    let tree = Tree { conn };
    for msg in iter {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        let (Some(sender), Some(path)) = (header.sender(), header.path()) else { continue };
        let node: Node = (sender.to_string(), path.clone().into());
        if !tree.has_state(&node, STATE_FOCUSED) {
            continue; // это событие «фокус ушёл»
        }
        let info = tree.describe(&node);
        let changed = {
            let old = t.info.lock().unwrap();
            old.node != info.node
        };
        t.set(info);
        if changed {
            let _ = tx.send(Event::FocusChanged);
        }
    }
    t.events.store(false, Ordering::Relaxed);
}

// --- опрос -------------------------------------------------------------------------

fn poll_loop(conn: &Connection, t: &FocusTracker) {
    let tree = Tree { conn };
    loop {
        {
            let last = t.last_poke.lock().unwrap();
            let idle = last.is_none_or(|p| p.elapsed() > IDLE_STOP);
            let guard = if idle {
                // Ввода давно нет: спим до poke(), снимок помечаем старым.
                t.info.lock().unwrap().at = None;
                t.wake.wait(last).unwrap()
            } else {
                t.wake.wait_timeout(last, POLL).unwrap().0
            };
            drop(guard);
        }
        let current = t.snapshot();
        // Узел из последнего события ещё в фокусе и его окно активно —
        // обход дерева не нужен.
        if let Some(node) = &current.node {
            if tree.has_state(node, STATE_FOCUSED) && tree.app_window_active(node) {
                let masked = tree.text_masked(node);
                let mut info = t.info.lock().unwrap();
                info.at = Some(Instant::now());
                info.password |= masked;
                continue;
            }
        }
        let info = tree.find_focus();
        t.healthy.store(info.is_some(), Ordering::Relaxed);
        t.set(info.unwrap_or_default());
    }
}

// --- дерево доступности ----------------------------------------------------------

struct Tree<'a> {
    conn: &'a Connection,
}

impl Tree<'_> {
    fn call<R>(&self, node: &Node, iface: &str, method: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType)) -> Option<R>
    where
        R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
    {
        let msg = self.conn.call_method(Some(node.0.as_str()), node.1.as_str(), Some(iface), method, body).ok()?;
        msg.body().deserialize::<R>().ok()
    }

    fn children(&self, node: &Node) -> Vec<Node> {
        self.call::<Vec<(String, OwnedObjectPath)>>(node, ACC, "GetChildren", &())
            .unwrap_or_default()
            .into_iter()
            .filter(|(n, p)| !n.is_empty() && p.as_str() != "/org/a11y/atspi/null")
            .collect()
    }

    fn has_state(&self, node: &Node, bit: u32) -> bool {
        self.call::<Vec<u32>>(node, ACC, "GetState", &())
            .and_then(|w| w.first().copied())
            .is_some_and(|w| w >> bit & 1 == 1)
    }

    fn role(&self, node: &Node) -> String {
        self.call::<String>(node, ACC, "GetRoleName", &()).unwrap_or_default()
    }

    fn name(&self, node: &Node) -> String {
        self.call::<zbus::zvariant::OwnedValue>(node, "org.freedesktop.DBus.Properties", "Get", &(ACC, "Name"))
            .and_then(|v| String::try_from(v).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn app_of(&self, node: &Node) -> Option<Node> {
        self.call::<(String, OwnedObjectPath)>(node, ACC, "GetApplication", &()).filter(|(n, _)| !n.is_empty())
    }

    fn app_window_active(&self, node: &Node) -> bool {
        let Some(app) = self.app_of(node) else { return false };
        if is_shell(&self.name(&app)) {
            return true; // диалоги оболочки окна в дереве не имеют
        }
        self.children(&app).iter().any(|w| self.has_state(w, STATE_ACTIVE))
    }

    /// Символы в поле есть, а текст пуст или замаскирован. Смотрим один символ
    /// и сразу о нём забываем.
    fn text_masked(&self, node: &Node) -> bool {
        const TEXT: &str = "org.a11y.atspi.Text";
        let count = self
            .call::<zbus::zvariant::OwnedValue>(node, "org.freedesktop.DBus.Properties", "Get", &(TEXT, "CharacterCount"))
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0);
        if count <= 0 {
            return false;
        }
        let Some(first) = self.call::<String>(node, TEXT, "GetText", &(0i32, 1i32)) else { return false };
        match first.chars().next() {
            None => true,
            Some(c) => matches!(c, '*' | '•' | '●' | '∗' | '·' | '⏺'),
        }
    }

    /// Есть ли в окне поля подпись со словом «пароль». Содержимое веб-страниц
    /// и терминалов не смотрим: там слово встречается просто в тексте.
    fn window_mentions_password(&self, node: &Node) -> bool {
        let mut top = node.clone();
        for _ in 0..MAX_DEPTH * 2 {
            let parent = self.call::<zbus::zvariant::OwnedValue>(&top, "org.freedesktop.DBus.Properties", "Get", &(ACC, "Parent"))
                .and_then(|v| <(String, OwnedObjectPath)>::try_from(v).ok());
            let Some(parent) = parent.filter(|p| !p.0.is_empty() && p.1.as_str() != "/org/a11y/atspi/null") else { break };
            if self.role(&parent) == "application" {
                break;
            }
            top = parent;
        }
        let mut queue = std::collections::VecDeque::from([(top, 0usize)]);
        let mut seen = 0;
        while let Some((n, depth)) = queue.pop_front() {
            seen += 1;
            if seen > LABEL_SCAN_NODES {
                return false;
            }
            let role = self.role(&n);
            if matches!(role.as_str(), "document web" | "document frame" | "terminal") {
                continue;
            }
            if matches!(role.as_str(), "label" | "static" | "heading" | "dialog" | "alert") && has_password_word(&self.name(&n).to_lowercase()) {
                return true;
            }
            if depth < MAX_DEPTH {
                queue.extend(self.children(&n).into_iter().map(|c| (c, depth + 1)));
            }
        }
        false
    }

    /// Описание сфокусированного узла.
    fn describe(&self, node: &Node) -> FocusInfo {
        let role = self.role(node);
        let app = self.app_of(node).map(|a| self.name(&a)).filter(|s| !s.is_empty());
        let label = self.name(node).to_lowercase();
        let entry_like = matches!(role.as_str(), "entry" | "text" | "text box" | "password text");
        let password = role == "password text"
            || (entry_like && (has_password_word(&label) || self.window_mentions_password(node) || self.text_masked(node)));
        FocusInfo {
            app,
            password,
            terminal: role == "terminal",
            role,
            known: true,
            at: Some(Instant::now()),
            node: Some(node.clone()),
        }
    }

    /// Обход: активное окно, в нём — сфокусированный узел. None — шина не отвечает.
    fn find_focus(&self) -> Option<FocusInfo> {
        let root: Node = ("org.a11y.atspi.Registry".into(), OwnedObjectPath::try_from("/org/a11y/atspi/accessible/root").ok()?);
        let msg = self.conn.call_method(Some(root.0.as_str()), root.1.as_str(), Some(ACC), "GetChildren", &()).ok()?;
        let apps: Vec<Node> = msg.body().deserialize().ok()?;
        let mut active: Option<(Node, String)> = None;
        'apps: for app in apps {
            let name = self.name(&app);
            if is_shell(&name) {
                continue;
            }
            for window in self.children(&app) {
                if self.has_state(&window, STATE_ACTIVE) {
                    active = Some((window, name));
                    break 'apps;
                }
            }
        }
        let Some((window, app_name)) = active else {
            return Some(FocusInfo { at: Some(Instant::now()), ..FocusInfo::default() });
        };
        let mut queue = std::collections::VecDeque::from([(window, 0usize)]);
        let mut seen = std::collections::HashSet::new();
        let mut deepest = None;
        while let Some((node, depth)) = queue.pop_front() {
            if seen.len() >= MAX_NODES || !seen.insert(node.clone()) {
                continue;
            }
            if self.has_state(&node, STATE_FOCUSED) {
                // Контейнеры Qt тоже бывают «в фокусе»: идём внутрь до самого поля.
                queue.clear();
                deepest = Some(node.clone());
            }
            if depth < MAX_DEPTH {
                queue.extend(self.children(&node).into_iter().map(|c| (c, depth + 1)));
            }
        }
        if let Some(node) = deepest {
            let mut info = self.describe(&node);
            if info.app.is_none() {
                info.app = Some(app_name);
            }
            return Some(info);
        }
        // Окно известно, а поле — нет: что в нём, не знаем.
        Some(FocusInfo { app: Some(app_name), at: Some(Instant::now()), ..FocusInfo::default() })
    }
}

fn is_shell(name: &str) -> bool {
    SHELL_APPS.contains(&name.to_lowercase().as_str())
}

/// Проверка для `doctor`: отвечает ли мост доступности.
pub fn bridge_available() -> Result<usize, String> {
    let conn = a11y_connection().map_err(|e| e.to_string())?;
    let msg = conn
        .call_method(Some("org.a11y.atspi.Registry"), "/org/a11y/atspi/accessible/root", Some(ACC), "GetChildren", &())
        .map_err(|e| e.to_string())?;
    let apps: Vec<Node> = msg.body().deserialize().map_err(|e| e.to_string())?;
    Ok(apps.len())
}

#[cfg(test)]
impl FocusTracker {
    /// Трекер с заданным снимком — для тестов контроллера.
    pub fn fake(app: &str, password: bool, terminal: bool) -> Arc<Self> {
        Arc::new(FocusTracker {
            info: Mutex::new(FocusInfo {
                app: Some(app.into()),
                password,
                terminal,
                role: String::new(),
                known: true,
                at: Some(Instant::now()),
                node: None,
            }),
            healthy: AtomicBool::new(true),
            events: AtomicBool::new(true),
            last_poke: Mutex::new(None),
            wake: Condvar::new(),
            conn: OnceLock::new(),
            fake_masked: AtomicBool::new(false),
        })
    }

    pub fn set_fake_masked(&self, masked: bool) {
        self.fake_masked.store(masked, Ordering::Relaxed);
    }

    pub fn set_fake(&self, password: bool) {
        let mut i = self.info.lock().unwrap();
        i.password = password;
        i.at = Some(Instant::now());
    }
}
