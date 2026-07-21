//! Icon sets. The UI asks this module for glyphs (file-type icons, folder
//! chevrons, panel markers) rather than hard-coding characters, so the entire
//! look can switch between three modes from config:
//!
//! - [`IconMode::Nerd`] — patched "Nerd Font" glyphs (the prettiest; needs a
//!   Nerd Font installed in the terminal).
//! - [`IconMode::Unicode`] — plain Unicode symbols that render in any modern
//!   terminal without a special font.
//! - [`IconMode::Ascii`] — pure ASCII, for the most limited environments.
//!
//! When the config doesn't pin an icon set, the default depends on where we're
//! running: the **GUI** uses `Nerd` (it bundles its own font, so the glyphs are
//! guaranteed), while the **terminal** falls back to `Unicode` (the host
//! terminal's font may lack Nerd glyphs and show tofu). See
//! `App::ensure_terminal_icons`. An explicit `icons = "nerd"` is honoured in
//! both — for users who do have a Nerd Font in their terminal.
//!
//! `file()` also returns a colour per file type so the Explorer reads at a
//! glance, the way a GUI editor's file icons do.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconMode {
    Nerd,
    Unicode,
    Ascii,
}

impl IconMode {
    /// Parse the `icons = "..."` config value. Defaults to `Nerd`.
    pub fn from_str(s: &str) -> IconMode {
        match s.trim().to_lowercase().as_str() {
            "unicode" => IconMode::Unicode,
            "ascii" => IconMode::Ascii,
            _ => IconMode::Nerd,
        }
    }
}

/// The resolved glyph set for the active [`IconMode`].
#[derive(Debug, Clone)]
pub struct Icons {
    pub mode: IconMode,
    /// Folder glyph (used for any directory rows the dialog shows).
    pub folder_closed: &'static str,
    /// Search-box glyph in the file dialog.
    pub search: &'static str,
    pub file_default: &'static str,
}

impl Icons {
    pub fn new(mode: IconMode) -> Self {
        match mode {
            IconMode::Nerd => Icons {
                mode,
                folder_closed: "\u{f07b}",
                // The Nerd magnifier (U+F002) fills the whole em cell (bbox
                // 0..2048, no side bearing) so the GPU renderer clips its edges.
                // U+2315 has proper bearings and renders cleanly in-cell.
                search: "\u{2315}",
                file_default: "\u{f15b}",
            },
            // Unicode mode: folders are marked by the tree's ▸/▾ chevron alone
            // (no folder glyph needed), files by the colour-coded ● dot (set in
            // `file()`). Everything here is in the text font, so it renders in
            // any terminal without a Nerd Font — this is the default in the
            // terminal build.
            IconMode::Unicode => Icons {
                mode,
                folder_closed: "",
                search: "⌕",
                file_default: "",
            },
            IconMode::Ascii => Icons {
                mode,
                folder_closed: "+",
                search: "/",
                file_default: " ",
            },
        }
    }

    /// Glyph + colour for a file, chosen from its name / extension.
    ///
    /// The colour carries the at-a-glance "what kind of file" signal. We use a
    /// simple filled dot (`●`) for the glyph rather than per-language Nerd
    /// logos: those glyphs fill their whole em, but the windowed renderer
    /// rasterises every glyph into one (narrower) monospace cell and clips the
    /// overflow — so the logos came out visibly cut off. `●` lives in the text
    /// font and fits the cell cleanly.
    pub fn file(&self, name: &str) -> (&'static str, Color) {
        let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
        let glyph = match self.mode {
            // ● colour-coded per type; the colour does the work, the dot fits
            // the cell (unlike the full-em Nerd logos, which clipped). It lives
            // in the text font, so it renders in any terminal — used for both
            // Nerd and Unicode modes.
            IconMode::Nerd | IconMode::Unicode => "\u{25cf}",
            // Ascii stays glyph-free for the most limited environments.
            IconMode::Ascii => self.file_default,
        };
        (glyph, file_color(&ext, name))
    }
}

/// File-type accent colour (kept independent of the theme so icons stay
/// recognisable across re-skins, matching how GUI editors colour file icons).
fn file_color(ext: &str, name: &str) -> Color {
    if name == ".gitignore" || ext == "git" || ext == "gitignore" {
        return Color::Rgb(240, 80, 50);
    }
    match ext {
        "rs" => Color::Rgb(222, 165, 132),
        "py" => Color::Rgb(255, 212, 59),
        "js" | "mjs" | "cjs" => Color::Rgb(240, 219, 79),
        "ts" | "tsx" => Color::Rgb(49, 120, 198),
        "jsx" => Color::Rgb(97, 218, 251),
        "go" => Color::Rgb(0, 173, 216),
        "json" => Color::Rgb(203, 203, 65),
        "toml" | "ini" | "cfg" | "conf" | "yaml" | "yml" => Color::Rgb(160, 160, 160),
        "md" | "markdown" => Color::Rgb(97, 175, 239),
        "html" | "htm" => Color::Rgb(228, 77, 38),
        "css" => Color::Rgb(86, 156, 214),
        "lock" => Color::Rgb(130, 130, 130),
        _ => Color::Rgb(150, 160, 175),
    }
}

/// Whether a file is binary — by extension — and so can't be opened in the text
/// editor. The file browser shows these disabled (greyed out). Conservative: a
/// file with no extension, or an unknown one, is treated as openable text (and
/// if it turns out not to be, opening it simply reports a read error).
pub fn is_binary(name: &str) -> bool {
    if !name.contains('.') {
        return false;
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        // images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff"
            | "heic" | "heif" | "avif" | "psd"
        // documents / pdf
            | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        // archives
            | "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "jar" | "war"
            | "dmg" | "iso"
        // audio / video
            | "mp3" | "wav" | "flac" | "ogg" | "oga" | "m4a" | "aac" | "opus"
            | "mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "wmv" | "flv"
        // fonts
            | "ttf" | "otf" | "woff" | "woff2" | "eot"
        // compiled / binaries
            | "exe" | "dll" | "so" | "dylib" | "o" | "obj" | "a" | "lib" | "class" | "pyc"
            | "pyo" | "wasm" | "bin" | "node"
        // databases
            | "db" | "sqlite" | "sqlite3"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection_by_extension() {
        assert!(is_binary("photo.jpeg"));
        assert!(is_binary("doc.PDF")); // case-insensitive
        assert!(is_binary("archive.zip"));
        assert!(!is_binary("main.rs"));
        assert!(!is_binary("notes.md"));
        assert!(!is_binary("icon.svg")); // SVG is text
        assert!(!is_binary("Makefile")); // no extension → openable
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(IconMode::from_str("nerd"), IconMode::Nerd);
        assert_eq!(IconMode::from_str("UNICODE"), IconMode::Unicode);
        assert_eq!(IconMode::from_str("ascii"), IconMode::Ascii);
        assert_eq!(IconMode::from_str("???"), IconMode::Nerd);
    }

    #[test]
    fn rust_file_has_distinct_color() {
        let icons = Icons::new(IconMode::Nerd);
        let (glyph, color) = icons.file("main.rs");
        // A cell-fitting dot (the colour, not the glyph, signals the type).
        assert_eq!(glyph, "\u{25cf}");
        assert_eq!(color, Color::Rgb(222, 165, 132));
    }

    #[test]
    fn file_types_share_glyph_differ_by_color() {
        let icons = Icons::new(IconMode::Nerd);
        let (rs_g, rs_c) = icons.file("a.rs");
        let (ts_g, ts_c) = icons.file("b.ts");
        assert_eq!(rs_g, ts_g, "same dot glyph");
        assert_ne!(rs_c, ts_c, "distinct colours per type");
    }

    #[test]
    fn ascii_mode_uses_neutral_glyph() {
        let icons = Icons::new(IconMode::Ascii);
        let (glyph, _) = icons.file("main.rs");
        assert_eq!(glyph, " ");
    }

    #[test]
    fn unicode_mode_shows_the_colored_dot() {
        // The colour-coded ● is in the text font, so terminal (Unicode) mode
        // shows it just like Nerd mode — and keeps the per-type colour.
        let icons = Icons::new(IconMode::Unicode);
        let (glyph, color) = icons.file("main.rs");
        assert_eq!(glyph, "\u{25cf}");
        assert_eq!(color, Color::Rgb(222, 165, 132));
    }
}
