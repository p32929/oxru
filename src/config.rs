//! User configuration. Everything cosmetic is data, not code: the icon set,
//! theme colours, and the windowed font size, merged from two TOML files over
//! the built-in defaults (later wins):
//!
//! 1. global  — `~/.config/oxru/config.toml`
//! 2. project — `<root>/.oxru/config.toml`
//!
//! ```toml
//! icons = "nerd"          # "nerd" | "unicode" | "ascii"
//!
//! [theme]
//! accent = "#4caf50"
//!
//! [gui]
//! font_size = 24
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::icons::{IconMode, Icons};
use crate::theme::Theme;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Icon set: `"nerd"` (default), `"unicode"`, or `"ascii"`.
    pub icons: Option<String>,
    /// `[theme]` table of `name = "#rrggbb"` colour overrides.
    pub theme: HashMap<String, String>,
    /// `[gui]` settings (windowed mode).
    pub gui: GuiConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    /// Font size for windowed mode, in logical points. Default 18.
    pub font_size: Option<u32>,
}

impl Config {
    /// Load and merge the global and project config files. Missing or malformed
    /// files are skipped so a bad config never stops the editor from starting.
    pub fn load(root: &Path) -> Config {
        Config::load_with(global_config_path().as_deref(), root)
    }

    /// Load just the global config (no project) — for the welcome / no-folder
    /// state where there's no project directory to read overrides from.
    pub fn load_global() -> Config {
        let mut cfg = Config::default();
        if let Some(global) = global_config_path() {
            cfg.merge_file(&global);
        }
        cfg
    }

    /// The testable core of [`load`]: merge an explicit global path (if any) then
    /// the project file over the defaults. Tests pass `None` so they never read
    /// the real user config.
    fn load_with(global: Option<&Path>, root: &Path) -> Config {
        let mut cfg = Config::default();
        if let Some(global) = global {
            cfg.merge_file(global);
        }
        cfg.merge_file(&root.join(".oxru/config.toml"));
        cfg
    }

    fn merge_file(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return; // missing file is normal — nothing to merge
        };
        match toml::from_str::<Config>(&text) {
            Ok(other) => self.merge(other),
            Err(e) => tracing::warn!(path = %path.display(), error = %e, "ignoring malformed config"),
        }
    }

    fn merge(&mut self, other: Config) {
        if other.icons.is_some() {
            self.icons = other.icons;
        }
        if other.gui.font_size.is_some() {
            self.gui.font_size = other.gui.font_size;
        }
        self.theme.extend(other.theme);
    }

    pub fn icon_mode(&self) -> IconMode {
        self.icons
            .as_deref()
            .map(IconMode::from_str)
            .unwrap_or(IconMode::Nerd)
    }

    pub fn icons(&self) -> Icons {
        Icons::new(self.icon_mode())
    }

    pub fn theme(&self) -> Theme {
        let mut theme = Theme::default();
        theme.apply_overrides(&self.theme);
        theme
    }

    /// Windowed-mode font size in logical points (default 24, clamped sane).
    pub fn gui_font_size(&self) -> u32 {
        self.gui.font_size.unwrap_or(24).clamp(8, 72)
    }
}

/// `~/.config/oxru/config.toml`, if a home/config dir can be determined.
fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxru/config.toml"))
}

/// Persist the user's windowed-mode preferences (font size + accent colour) to
/// the global config, merging into whatever is already there so other keys are
/// preserved. Best-effort: a write failure is reported but never fatal.
pub fn save_prefs(font_size: u32, accent: (u8, u8, u8)) -> std::io::Result<()> {
    let Some(path) = global_config_path() else {
        return Ok(());
    };
    save_prefs_to(&path, font_size, accent)
}

/// The path-taking core of [`save_prefs`] (kept separate so tests can target a
/// temp file instead of the real user config).
fn save_prefs_to(path: &Path, font_size: u32, accent: (u8, u8, u8)) -> std::io::Result<()> {
    // Start from the existing file (to keep icons / other colours), else fresh.
    let mut root: toml::Table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let gui = root
        .entry("gui".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(tbl) = gui.as_table_mut() {
        tbl.insert("font_size".to_string(), toml::Value::Integer(font_size as i64));
    }

    let accent_hex = format!("#{:02x}{:02x}{:02x}", accent.0, accent.1, accent.2);
    let theme = root
        .entry("theme".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(tbl) = theme.as_table_mut() {
        tbl.insert("accent".to_string(), toml::Value::String(accent_hex));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(&root)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_sections() {
        let src = r##"
            icons = "unicode"
            [theme]
            accent = "#010203"
            [gui]
            font_size = 22
        "##;
        let cfg: Config = toml::from_str(src).unwrap();
        assert_eq!(cfg.icon_mode(), IconMode::Unicode);
        assert_eq!(cfg.theme().accent, ratatui::style::Color::Rgb(1, 2, 3));
        assert_eq!(cfg.gui_font_size(), 22);
    }

    #[test]
    fn project_overrides_apply() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".oxru")).unwrap();
        fs::write(
            dir.path().join(".oxru/config.toml"),
            "icons = \"ascii\"\n[theme]\naccent = \"#0a0b0c\"\n",
        )
        .unwrap();
        // No global config, so only the project file applies.
        let cfg = Config::load_with(None, dir.path());
        assert_eq!(cfg.icon_mode(), IconMode::Ascii);
        assert_eq!(cfg.theme().accent, ratatui::style::Color::Rgb(10, 11, 12));
    }

    #[test]
    fn save_prefs_roundtrips_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxru/config.toml");
        // Seed an existing key that must survive the save.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "icons = \"ascii\"\n").unwrap();

        save_prefs_to(&path, 30, (0xe5, 0x39, 0x35)).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.gui_font_size(), 30);
        assert_eq!(cfg.theme().accent, ratatui::style::Color::Rgb(0xe5, 0x39, 0x35));
        assert_eq!(cfg.icon_mode(), IconMode::Ascii, "pre-existing key preserved");
    }

    #[test]
    fn missing_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // Hermetic: no global, no project file -> built-in defaults.
        let cfg = Config::load_with(None, dir.path());
        assert_eq!(cfg.icon_mode(), IconMode::Nerd);
        assert_eq!(cfg.gui_font_size(), 24);
    }
}
