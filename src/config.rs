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
    /// Terminal repaint rate in windowed mode (see `FPS_OPTIONS`). Default 60.
    pub fps: Option<u32>,
    /// Dialog/terminal-modal size, as a percent of the screen (80-99).
    /// Default 99.
    pub dialog_size: Option<u32>,
}

/// Allowed windowed-mode terminal repaint rates, low to high — offered as the
/// Settings dialog's FPS picker. Only *unattended* terminal output (a build
/// log streaming, a spinner, anything the user isn't actively driving) is
/// throttled to this; typing and other real UI interaction always redraws
/// immediately regardless of this setting.
pub const FPS_OPTIONS: &[u32] = &[1, 5, 10, 20, 30, 60];

/// Lowest/highest percent-of-screen the Settings dialog's dialog-size control
/// offers (and `Config::gui_dialog_size` clamps to).
pub const DIALOG_SIZE_RANGE: std::ops::RangeInclusive<u32> = 80..=99;

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
    pub(crate) fn load_with(global: Option<&Path>, root: &Path) -> Config {
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
        if other.gui.fps.is_some() {
            self.gui.fps = other.gui.fps;
        }
        if other.gui.dialog_size.is_some() {
            self.gui.dialog_size = other.gui.dialog_size;
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

    /// Windowed-mode terminal repaint rate (default 60), snapped to the
    /// nearest `FPS_OPTIONS` entry so a hand-edited config value between the
    /// offered choices doesn't leave the Settings picker showing the wrong one.
    pub fn gui_fps(&self) -> u32 {
        let raw = self.gui.fps.unwrap_or(60);
        *FPS_OPTIONS
            .iter()
            .min_by_key(|&&f| (f as i64 - raw as i64).abs())
            .expect("FPS_OPTIONS is non-empty")
    }

    /// Dialog/terminal-modal size as a percent of the screen (default 99 —
    /// as large as the offered range allows, until the user dials it back),
    /// clamped to `DIALOG_SIZE_RANGE`.
    pub fn gui_dialog_size(&self) -> u32 {
        self.gui.dialog_size.unwrap_or(99).clamp(
            *DIALOG_SIZE_RANGE.start(),
            *DIALOG_SIZE_RANGE.end(),
        )
    }
}

/// `~/.config/oxru/config.toml`, if a home/config dir can be determined.
pub(crate) fn global_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxru/config.toml"))
}

/// Persist the user's windowed-mode preferences (font size, terminal FPS,
/// dialog size, accent colour) to `path`, merging into whatever is already
/// there so other keys are preserved. Best-effort: a write failure is
/// reported but never fatal. [`App`](crate::app::App) calls this with its
/// `config_path` (the real global config in production, a scratch file in
/// tests).
pub(crate) fn save_prefs_to(
    path: &Path,
    font_size: u32,
    fps: u32,
    dialog_size: u32,
    accent: (u8, u8, u8),
) -> std::io::Result<()> {
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
        tbl.insert("fps".to_string(), toml::Value::Integer(fps as i64));
        tbl.insert("dialog_size".to_string(), toml::Value::Integer(dialog_size as i64));
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
            fps = 30
        "##;
        let cfg: Config = toml::from_str(src).unwrap();
        assert_eq!(cfg.icon_mode(), IconMode::Unicode);
        assert_eq!(cfg.theme().accent, ratatui::style::Color::Rgb(1, 2, 3));
        assert_eq!(cfg.gui_font_size(), 22);
        assert_eq!(cfg.gui_fps(), 30);
    }

    #[test]
    fn dialog_size_defaults_and_clamps() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.gui_dialog_size(), 99, "defaults to 99");
        let cfg: Config = toml::from_str("[gui]\ndialog_size = 50\n").unwrap();
        assert_eq!(cfg.gui_dialog_size(), 80, "clamps to the 80-99 range");
        let cfg: Config = toml::from_str("[gui]\ndialog_size = 150\n").unwrap();
        assert_eq!(cfg.gui_dialog_size(), 99, "clamps to the 80-99 range");
    }

    #[test]
    fn fps_snaps_to_nearest_option() {
        let cfg: Config = toml::from_str("[gui]\nfps = 27\n").unwrap();
        assert_eq!(cfg.gui_fps(), 30, "27 is closer to 30 than 20");
        let cfg: Config = toml::from_str("[gui]\nfps = 1000\n").unwrap();
        assert_eq!(cfg.gui_fps(), 60, "clamps to the highest offered option");
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

        save_prefs_to(&path, 30, 20, 85, (0xe5, 0x39, 0x35)).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.gui_font_size(), 30);
        assert_eq!(cfg.gui_fps(), 20);
        assert_eq!(cfg.gui_dialog_size(), 85);
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
        assert_eq!(cfg.gui_fps(), 60);
    }
}
