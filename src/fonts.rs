//! Best-effort install of the bundled "Symbols Nerd Font" into the OS font
//! directory, so a **terminal**'s automatic glyph fallback can render the Nerd
//! icon glyphs Oxru uses (the GUI bundles and draws the font itself; a terminal
//! can't be handed a font, it can only fall back to one installed on the system).
//!
//! This is opt-in behaviour the user chose: it writes one file into the user's
//! font folder. It never overwrites an existing copy, and is undone by deleting
//! that file. Whether the host terminal actually falls back to it for the
//! private-use icon glyphs is terminal-dependent — so callers still degrade to
//! the font-independent Unicode icon set when the font isn't already present.

use std::path::PathBuf;

/// The same symbols font the GUI renderer bundles (not feature-gated, so the
/// terminal build can install it too).
const SYMBOLS_FONT: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono.ttf");

/// Filename we install under — the family name lives inside the font, so this is
/// only how the file is found on disk; a stable name keeps the install
/// idempotent.
const INSTALL_NAME: &str = "SymbolsNerdFontMono.ttf";

/// Outcome of trying to make the symbols font available to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontInstall {
    /// The font file was already in the font directory (from a previous run, or
    /// the user installed it themselves) — the terminal can fall back to it now.
    AlreadyPresent,
    /// We just wrote it this run — it won't be picked up until the next launch
    /// (and a font-cache refresh), so this session should still use safe icons.
    Installed,
    /// No known per-user font directory on this platform (e.g. Windows, which
    /// needs registry registration we don't do).
    Unsupported,
    /// We found a font directory but couldn't write the file.
    Failed,
}

/// The per-user font directory for this platform, if we know one.
fn font_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Fonts"))
    } else if cfg!(target_os = "linux") {
        Some(home.join(".local/share/fonts"))
    } else {
        None
    }
}

/// Install the bundled symbols font into the user font directory if it isn't
/// there yet. Idempotent; never overwrites an existing file.
pub fn install_symbol_font() -> FontInstall {
    let Some(dir) = font_dir() else {
        return FontInstall::Unsupported;
    };
    let target = dir.join(INSTALL_NAME);
    if target.exists() {
        return FontInstall::AlreadyPresent;
    }
    if std::fs::create_dir_all(&dir).is_err() {
        return FontInstall::Failed;
    }
    match std::fs::write(&target, SYMBOLS_FONT) {
        Ok(()) => FontInstall::Installed,
        Err(_) => FontInstall::Failed,
    }
}
