//! Защита набранного текста: память, журнал, права, выученные слова.
//!
//! Демон видит каждое нажатие, в том числе пароли. Правила этого модуля:
//!
//! * нажатия лежат в одном заранее выделенном буфере, закреплённом в ОЗУ
//!   (`mlock`: не уходит в swap) и затираемом при каждом сбросе;
//! * процесс не дампится и не читается через ptrace/`/proc/PID/mem` другими
//!   процессами того же пользователя (`PR_SET_DUMPABLE = 0`, core = 0);
//! * у типа [`SecretText`] нет `Display`, а `Debug` печатает длину — так
//!   слово труднее случайно вывести в журнал. Копии текста бывают и в обычных
//!   строках; их затирает при освобождении аллокатор из `alloc.rs`;
//! * выученные слова («не трогать») хранятся как HMAC-SHA256 с ключом,
//!   который лежит отдельно с правами 0600, а не открытым текстом;
//! * при установке setgid-группой `input` права на устройства берутся только
//!   на время `open()`, остальное время процесс работает без них.
#![allow(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

// --- процесс ---------------------------------------------------------------

/// Запретить дампы и чтение памяти процесса другими процессами пользователя.
pub fn harden_process() {
    unsafe {
        // Без dumpable ptrace и /proc/PID/mem доступны только root
        // (при kernel.yama.ptrace_scope = 0 иначе их мог бы читать любой
        // процесс того же пользователя — и вытащить буфер нажатий).
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            crate::warn!("PR_SET_DUMPABLE не выставлен");
        }
        let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
    }
}

// --- права на устройства ------------------------------------------------------

pub struct Privs {
    real: libc::gid_t,
    input: Option<libc::gid_t>,
}

static PRIVS: Mutex<Option<Privs>> = Mutex::new(None);

/// Если бинарник установлен setgid (группа `input`), сбросить эффективную
/// группу до реальной; привилегированная остаётся в saved-gid и
/// возвращается только на время [`with_input_group`].
pub fn init_privileges() {
    let (real, effective) = unsafe { (libc::getgid(), libc::getegid()) };
    let input = if effective != real {
        let rc = unsafe { libc::setresgid(u32::MAX, real, u32::MAX) };
        if rc != 0 {
            crate::error!("не удалось сбросить setgid-права — выхожу");
            std::process::exit(1);
        }
        Some(effective)
    } else {
        None
    };
    *PRIVS.lock().unwrap() = Some(Privs { real, input });
}

/// Сбросить группу насовсем: real = effective = saved. Для всех команд,
/// кроме `run`: иначе `wayswitch-rs doctor` отдавал бы группу `input` любой
/// программе, которую находил в подставленном PATH.
pub fn drop_privileges_permanently() {
    let real = unsafe { libc::getgid() };
    let rc = unsafe { libc::setresgid(real, real, real) };
    let (mut r, mut e, mut s) = (0, 0, 0);
    let got = unsafe { libc::getresgid(&mut r, &mut e, &mut s) };
    if rc != 0 || got != 0 || r != real || e != real || s != real {
        eprintln!("не удалось сбросить setgid-права — выхожу");
        std::process::exit(1);
    }
}

/// Держать, пока запускается внешняя программа: пока guard жив, ни один
/// поток не поднимет группу `input`, и дочерний процесс её не унаследует.
pub fn privilege_window_closed() -> std::sync::MutexGuard<'static, Option<Privs>> {
    PRIVS.lock().unwrap()
}

/// Состоит ли процесс в группе с таким именем (без запуска внешних программ).
pub fn in_group(name: &str) -> bool {
    let Ok(groups) = fs::read_to_string("/etc/group") else { return false };
    let Some(gid) = groups.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next() == Some(name)).then(|| f.nth(1)?.parse::<libc::gid_t>().ok()).flatten()
    }) else {
        return false;
    };
    let mut list = [0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(256, list.as_mut_ptr()) };
    n > 0 && list[..n as usize].contains(&gid)
}

pub fn setgid_mode() -> bool {
    PRIVS.lock().unwrap().as_ref().is_some_and(|p| p.input.is_some())
}

/// Выполнить `f` с эффективной группой `input` (если демон setgid).
pub fn with_input_group<T>(f: impl FnOnce() -> T) -> T {
    let guard = PRIVS.lock().unwrap();
    let Some(p) = guard.as_ref().filter(|p| p.input.is_some()) else {
        drop(guard);
        return f();
    };
    unsafe { libc::setresgid(u32::MAX, p.input.unwrap(), u32::MAX) };
    let out = f();
    if unsafe { libc::setresgid(u32::MAX, p.real, u32::MAX) } != 0 {
        eprintln!("не удалось вернуть права обратно — выхожу");
        std::process::exit(1);
    }
    out
}

// --- закреплённый буфер ------------------------------------------------------

/// Буфер фиксированной ёмкости: не перевыделяется (старые копии не остаются
/// в куче), закреплён в ОЗУ и затирается при очистке и в `Drop`.
pub struct LockedBuf<T: Copy + Default + Zeroize, const N: usize> {
    data: Box<[T; N]>,
    len: usize,
    locked: bool,
}

impl<T: Copy + Default + Zeroize, const N: usize> LockedBuf<T, N> {
    pub fn new() -> Self {
        let data = Box::new([T::default(); N]);
        let size = std::mem::size_of::<[T; N]>();
        let locked = unsafe { libc::mlock(data.as_ptr().cast(), size) } == 0;
        if !locked {
            crate::warn!("mlock буфера нажатий не удался — он может попасть в swap");
        }
        LockedBuf { data, len: 0, locked }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[T] {
        &self.data[..self.len]
    }

    /// Добавить; при переполнении выбрасываются самые старые элементы.
    pub fn push(&mut self, item: T) {
        if self.len == N {
            self.data.copy_within(1.., 0);
            self.len -= 1;
        }
        self.data[self.len] = item;
        self.len += 1;
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let item = self.data[self.len];
        self.data[self.len].zeroize();
        Some(item)
    }

    /// Обрезать до `len` элементов, затерев хвост.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        for item in &mut self.data[len..self.len] {
            item.zeroize();
        }
        self.len = len;
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    pub fn extend_from_slice(&mut self, items: &[T]) {
        for &item in items {
            self.push(item);
        }
    }
}

impl<T: Copy + Default + Zeroize, const N: usize> Drop for LockedBuf<T, N> {
    fn drop(&mut self) {
        for item in self.data.iter_mut() {
            item.zeroize();
        }
        if self.locked {
            let size = std::mem::size_of::<[T; N]>();
            unsafe { libc::munlock(self.data.as_ptr().cast(), size) };
        }
    }
}

// --- текст ----------------------------------------------------------------------

/// Набранный текст. Затирается при удалении; в журнал выводится только длина.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    pub fn new(s: String) -> Self {
        SecretText(Zeroizing::new(s))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn chars(&self) -> usize {
        self.0.chars().count()
    }
}

impl From<String> for SecretText {
    fn from(s: String) -> Self {
        SecretText::new(s)
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<скрыто: {} симв.>", self.chars())
    }
}

// --- файлы ------------------------------------------------------------------

pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("wayswitch-rs")
}

pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("wayswitch-rs")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback),
    }
}

/// Каталог только для владельца (0700).
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Атомарно записать файл с правами 0600.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(tmp, path)
}

// --- выученные слова -----------------------------------------------------------

/// Слова, исправление которых человек отменил: хранятся как HMAC, чтобы
/// файл не превращался в список того, что человек набирал (в том числе
/// паролей, если исправление в поле пароля когда-то отменили).
pub struct LearnedStore {
    key: Zeroizing<[u8; 32]>,
    hashes: HashSet<[u8; 32]>,
    path: PathBuf,
}

impl LearnedStore {
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        private_dir(dir)?;
        let key_path = dir.join("learned.key");
        let mut key = Zeroizing::new([0u8; 32]);
        match fs::read(&key_path) {
            Ok(mut bytes) if bytes.len() == 32 => {
                key.copy_from_slice(&bytes);
                bytes.zeroize();
            }
            _ => {
                getrandom::getrandom(&mut key[..]).map_err(|e| std::io::Error::other(e.to_string()))?;
                write_private(&key_path, &key[..])?;
            }
        }
        let path = dir.join("learned.txt");
        let mut hashes = HashSet::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                if let Some(h) = parse_hex32(line.trim()) {
                    hashes.insert(h);
                }
            }
        }
        Ok(LearnedStore { key, hashes, path })
    }

    fn digest(&self, word: &str) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key[..]).expect("ключ HMAC");
        let lower = Zeroizing::new(word.to_lowercase());
        mac.update(lower.as_bytes());
        mac.finalize().into_bytes().into()
    }

    pub fn contains(&self, word: &str) -> bool {
        !self.hashes.is_empty() && self.hashes.contains(&self.digest(word))
    }

    pub fn add(&mut self, word: &str) -> std::io::Result<()> {
        if self.hashes.insert(self.digest(word)) {
            self.save()?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn clear(&mut self) -> std::io::Result<()> {
        self.hashes.clear();
        self.save()
    }

    fn save(&self) -> std::io::Result<()> {
        let mut out = String::new();
        let mut sorted: Vec<_> = self.hashes.iter().collect();
        sorted.sort();
        for h in sorted {
            for b in h {
                out.push_str(&format!("{b:02x}"));
            }
            out.push('\n');
        }
        write_private(&self.path, out.as_bytes())
    }
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hex = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(hex, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_hides_text() {
        let s = SecretText::new("пароль123".into());
        assert_eq!(format!("{s:?}"), "<скрыто: 9 симв.>");
    }

    #[test]
    fn locked_buf_zeroizes_tail() {
        let mut b: LockedBuf<u16, 4> = LockedBuf::new();
        b.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(b.as_slice(), &[2, 3, 4, 5]);
        b.truncate(1);
        assert_eq!(b.as_slice(), &[2]);
        assert!(b.data[1..].iter().all(|&x| x == 0));
    }

    #[test]
    fn learned_store_keeps_no_plaintext() {
        let dir = std::env::temp_dir().join(format!("wsrs-test-{}", std::process::id()));
        let mut store = LearnedStore::open(&dir).unwrap();
        store.add("Ghbdtn").unwrap();
        assert!(store.contains("ghbdtn"));
        assert!(!store.contains("ghbdtm"));
        let file = fs::read_to_string(dir.join("learned.txt")).unwrap();
        assert!(!file.contains("ghbdtn"));
        let mode = fs::metadata(dir.join("learned.key")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let again = LearnedStore::open(&dir).unwrap();
        assert!(again.contains("GHBDTN"));
        fs::remove_dir_all(dir).unwrap();
    }
}
