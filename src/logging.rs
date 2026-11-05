//! Журнал в stderr (под systemd он уходит в journald).
//!
//! Сознательно без форматирования произвольных значений через `{:?}` в
//! местах, где есть текст: набранное держится в `SecretText`, у которого
//! `Debug` печатает только длину.

use std::sync::atomic::{AtomicU8, Ordering};

pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;

static LEVEL: AtomicU8 = AtomicU8::new(INFO);

pub fn init_from_env() {
    let level = match std::env::var("WAYSWITCH_LOG").as_deref() {
        Ok("error") => ERROR,
        Ok("warn") => WARN,
        Ok("debug") => DEBUG,
        _ => INFO,
    };
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

pub fn write(level: u8, args: std::fmt::Arguments<'_>) {
    // Префиксы <N> понимает journald: уровень виден в journalctl -p.
    let prio = match level {
        ERROR => 3,
        WARN => 4,
        INFO => 6,
        _ => 7,
    };
    eprintln!("<{prio}>{args}");
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { if $crate::logging::enabled($crate::logging::ERROR) {
        $crate::logging::write($crate::logging::ERROR, format_args!($($t)*)) } };
}
#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { if $crate::logging::enabled($crate::logging::WARN) {
        $crate::logging::write($crate::logging::WARN, format_args!($($t)*)) } };
}
#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { if $crate::logging::enabled($crate::logging::INFO) {
        $crate::logging::write($crate::logging::INFO, format_args!($($t)*)) } };
}
#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { if $crate::logging::enabled($crate::logging::DEBUG) {
        $crate::logging::write($crate::logging::DEBUG, format_args!($($t)*)) } };
}
