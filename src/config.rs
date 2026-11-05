//! Конфигурация: ~/.config/wayswitch-rs/config.toml и списки.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::secure::config_dir;

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub auto_correct: bool,
    /// conservative | normal | aggressive
    pub sensitivity: String,
    pub phrase_timeout_sec: f64,
    pub undo_window_sec: f64,
    pub edit_guard_sec: f64,
    pub punct_fix: bool,
    pub particles_fix: bool,
    pub split_words_fix: bool,
    pub two_capitals_fix: bool,
    pub caps_fix: bool,
    pub early_switch: bool,
    pub fix_on_tab: bool,
    pub fix_on_enter: bool,
    /// Сколько отмен одного слова, чтобы его запомнить (1..5).
    pub learn_after_undos: u32,
}

impl Default for General {
    fn default() -> Self {
        General {
            auto_correct: true,
            sensitivity: "normal".into(),
            phrase_timeout_sec: 8.0,
            undo_window_sec: 5.0,
            edit_guard_sec: 1.0,
            punct_fix: true,
            particles_fix: true,
            split_words_fix: true,
            two_capitals_fix: true,
            caps_fix: true,
            early_switch: true,
            fix_on_tab: false,
            fix_on_enter: false,
            learn_after_undos: 2,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UnknownFocus {
    /// Не знаем, что в фокусе, — не исправляем автоматически (по умолчанию).
    Skip,
    /// Исправлять и без сведений о фокусе (как в оригинальном WaySwitch).
    Allow,
}

#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TerminalMode {
    /// В терминалах только ручное исправление: приглашения sudo/ssh не видны
    /// at-spi, и пароль в них неотличим от обычного текста (по умолчанию).
    Manual,
    /// Исправлять как везде.
    Auto,
    /// В терминалах не делать ничего, даже по жесту.
    Off,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Security {
    /// Что делать, если at-spi не говорит, что в фокусе.
    pub unknown_focus: UnknownFocus,
    pub terminals: TerminalMode,
    /// Запоминать слова после отмен (хранятся как HMAC, не текстом).
    pub learn_words: bool,
    /// Ещё приложения, где не делать ничего (в дополнение к встроенным
    /// менеджерам паролей, pinentry и окнам polkit).
    pub sensitive_apps: Vec<String>,
}

impl Default for Security {
    fn default() -> Self {
        Security {
            unknown_focus: UnknownFocus::Skip,
            terminals: TerminalMode::Manual,
            learn_words: true,
            sensitive_apps: Vec::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct GestureCfg {
    /// double_shift | none
    pub manual: String,
}

impl Default for GestureCfg {
    fn default() -> Self {
        GestureCfg { manual: "double_shift".into() }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Typing {
    pub settle_ms: u64,
    pub key_delay_ms: u64,
}

impl Default for Typing {
    fn default() -> Self {
        Typing { settle_ms: 120, key_delay_ms: 0 }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct BackendCfg {
    /// auto | gnome | kde | sway | hyprland
    pub prefer: String,
    /// GNOME: печатать через расширение (keysym-ы) или uinput.
    pub gnome_typing: String,
}

impl Default for BackendCfg {
    fn default() -> Self {
        BackendCfg { prefer: "auto".into(), gnome_typing: "extension".into() }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub security: Security,
    pub gesture: GestureCfg,
    pub typing: Typing,
    pub backend: BackendCfg,
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    pub fn load() -> Result<Config, String> {
        let path = Self::path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let cfg: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        let g = &self.general;
        if !["conservative", "normal", "aggressive"].contains(&g.sensitivity.as_str()) {
            return Err("sensitivity: conservative | normal | aggressive".into());
        }
        if !(1.0..=300.0).contains(&g.phrase_timeout_sec) {
            return Err("phrase_timeout_sec: 1..300".into());
        }
        if !(0.0..=60.0).contains(&g.undo_window_sec) {
            return Err("undo_window_sec: 0..60".into());
        }
        if !(0.0..=10.0).contains(&g.edit_guard_sec) {
            return Err("edit_guard_sec: 0..10".into());
        }
        if !(1..=5).contains(&g.learn_after_undos) {
            return Err("learn_after_undos: 1..5".into());
        }
        if !["double_shift", "none"].contains(&self.gesture.manual.as_str()) {
            return Err("manual: double_shift | none".into());
        }
        if self.typing.settle_ms > 2000 || self.typing.key_delay_ms > 100 {
            return Err("settle_ms: 0..2000, key_delay_ms: 0..100".into());
        }
        if !["auto", "gnome", "kde", "sway", "hyprland"].contains(&self.backend.prefer.as_str()) {
            return Err("prefer: auto | gnome | kde | sway | hyprland".into());
        }
        if !["extension", "uinput"].contains(&self.backend.gnome_typing.as_str()) {
            return Err("gnome_typing: extension | uinput".into());
        }
        Ok(())
    }
}

fn read_list(name: &str) -> Vec<String> {
    let Ok(text) = fs::read(config_dir().join(name)) else { return Vec::new() };
    String::from_utf8_lossy(&text)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Слова, которые человек сам вписал в exceptions.txt.
pub fn load_exceptions() -> HashSet<String> {
    read_list("exceptions.txt").into_iter().map(|w| w.to_lowercase()).collect()
}

/// Программы, где не исправлять автоматически (apps.txt).
pub fn load_apps() -> Vec<String> {
    read_list("apps.txt")
}
