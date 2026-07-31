//! Colour theme. A small, named palette drives every styled element in the UI
//! (chrome and syntax alike), so the whole look can be re-skinned from one place
//! — and overridden from the user's config file.
//!
//! The default is the **VSCode "Default Dark" (Dark+)** palette — the same
//! workbench and token colours VSCode ships with — chosen so contrast is good
//! and text stays readable out of the box. Each field is a `ratatui` [`Color`];
//! the config layer can replace any of them with a `#rrggbb` hex string (see
//! [`Theme::apply_overrides`]).

use std::collections::HashMap;

use ratatui::style::{Color, Modifier, Style};

/// Popular accent colours offered in the Settings dialog. Green is the default;
/// the rest are Material-ish 500-weight hues.
pub const ACCENT_PALETTE: &[(&str, (u8, u8, u8))] = &[
    ("Green", (0x4c, 0xaf, 0x50)),
    ("Blue", (0x4f, 0xc1, 0xff)),
    ("Red", (0xe5, 0x39, 0x35)),
    ("Purple", (0xab, 0x47, 0xbc)),
    ("Orange", (0xff, 0x98, 0x00)),
    ("Teal", (0x26, 0xa6, 0x9a)),
    ("Indigo", (0x5c, 0x6b, 0xc0)),
    ("Pink", (0xec, 0x40, 0x7a)),
    ("White", (0xe0, 0xe0, 0xe0)),
];

/// Darken an accent channel to make a harmonious selection background.
fn dim_channel(c: u8) -> u8 {
    (c as f32 * 0.38) as u8
}

/// The RGB components of a `Color`, if it is an explicit `Rgb` (the only kind the
/// theme uses); named/indexed colours have no components to blend.
fn rgb_of(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

/// Linear blend from `a` to `b` by `t` in `[0, 1]`.
fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t).round() as u8
}

/// How far the status bar sits from the accent toward the background. A bar at
/// full accent strength is a solid band of saturated colour across the bottom of
/// the window — far too loud to sit under code all day — while anything much
/// darker stops reading as a bar at all.
const STATUS_BAR_MIX: f32 = 0.62;

/// The status bar's background for a given palette: a darkened shade of that
/// palette's own accent.
///
/// The default palette derives it this way, and so do [`Theme::set_accent`] and
/// an `accent` config override, so the bar always belongs to whatever colours
/// are currently on screen. Borrowing a fixed colour from somewhere else (this
/// used to be VSCode's `#007acc`) puts an unrelated hue against the accent in
/// the one place they're guaranteed to be seen together, and it goes wrong the
/// moment the accent isn't blue.
fn status_bar_bg(accent: Color, bg: Color) -> Color {
    mix(accent, bg, STATUS_BAR_MIX)
}

/// Mix `a` toward `b` by `t` in `[0, 1]`. Non-RGB colours (never used by the
/// theme) pass through unchanged rather than guessing components for them.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    match (rgb_of(a), rgb_of(b)) {
        (Some((ar, ag, ab)), Some((br, bg, bb))) => {
            Color::Rgb(lerp(ar, br, t), lerp(ag, bg, t), lerp(ab, bb, t))
        }
        _ => a,
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    /// Editor background (`editor.background`).
    pub bg: Color,
    /// Sidebar / panel / palette background (`sideBar.background`).
    pub bg_dark: Color,
    /// Subtle fill: inactive selection, scrollbar thumb, the `~` past EOF.
    pub bg_light: Color,
    /// Hairline colour for borders and separators between surfaces. A cell grid
    /// can't draw a 1px rule the way a GUI can, so the whole layout leans on
    /// tonal separation instead — this is the one slot dedicated to it, kept a
    /// step brighter than `bg_light` so a border reads as an edge rather than
    /// as a fill.
    pub border: Color,
    /// Background tint for the row the caret is on (`editor.lineHighlight`).
    /// Roughly 12-20 points above `bg` per channel — enough to locate the caret
    /// with a glance and no more. The first attempt at ~10 points was invisible
    /// in practice, the same way the original `bg`/`bg_dark` pair was: a tint
    /// nobody can see is just a slower way of having no tint.
    pub line_hl: Color,
    /// The vertical rule drawn at each indent level (`editorIndentGuide`).
    pub indent_guide: Color,
    /// Primary foreground text (`editor.foreground`).
    pub fg: Color,
    /// Dimmed text: line numbers, inactive labels.
    pub fg_dim: Color,
    /// Bright accent for thin elements — focus, hint keys, active markers,
    /// the current line number, palette border. Readable *as text* on a dark bg.
    pub accent: Color,
    /// Readable foreground on top of a filled accent (white).
    pub accent_fg: Color,
    /// Status-bar background — a darkened shade of this palette's own `accent`,
    /// see [`status_bar_bg`].
    pub status_bg: Color,
    /// Focused-selection background (`list.activeSelectionBackground`).
    pub sel_bg: Color,
    /// Background for every in-file find match (`editor.findMatchHighlight`).
    pub find_match: Color,
    /// Background for the *current* find match (`editor.findMatchBackground`).
    pub find_current: Color,

    // Token palette (VSCode Dark+ TextMate colours), reused by syntax + chrome.
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub orange: Color,
    pub blue: Color,
    pub purple: Color,
    pub cyan: Color,
    pub comment: Color,
}

impl Default for Theme {
    fn default() -> Self {
        dark_plus()
    }
}

/// VSCode "Default Dark+", with the surface ramp opened up.
///
/// VSCode's own workbench greys sit within ~7/255 of each other (`#1e1e1e` /
/// `#252526`), which works there because every panel is separated by a real
/// 1-pixel border. A character grid has no sub-cell rules to draw, so those
/// same values made the editor, the tab strip and the footer read as one flat
/// sheet — the active tab in particular was indistinguishable until it was
/// given an accent-tinted background as a workaround. The ramp below keeps the
/// Dark+ *hue* (a hair cooler) while spacing the steps far enough apart that
/// each surface is legible as its own plane.
fn dark_plus() -> Theme {
    let bg = Color::Rgb(0x1a, 0x1a, 0x1c);
    let accent = Color::Rgb(0x4c, 0xaf, 0x50);
    Theme {
        bg,
        bg_dark: Color::Rgb(0x23, 0x23, 0x27),
        bg_light: Color::Rgb(0x33, 0x33, 0x3a),
        border: Color::Rgb(0x3d, 0x3d, 0x46),
        line_hl: Color::Rgb(0x28, 0x28, 0x30),
        indent_guide: Color::Rgb(0x33, 0x33, 0x3a),
        fg: Color::Rgb(0xd4, 0xd4, 0xd4),
        fg_dim: Color::Rgb(0x85, 0x85, 0x85),
        accent,
        accent_fg: Color::Rgb(0xff, 0xff, 0xff),
        status_bg: status_bar_bg(accent, bg),
        sel_bg: Color::Rgb(0x1c, 0x42, 0x1e),
        // Amber highlights for find, distinct from the green selection.
        find_match: Color::Rgb(0x4d, 0x3c, 0x14),
        find_current: Color::Rgb(0x8a, 0x60, 0x18),
        red: Color::Rgb(0xf4, 0x47, 0x47),
        green: Color::Rgb(0x6a, 0x99, 0x55),
        yellow: Color::Rgb(0xdc, 0xdc, 0xaa),
        orange: Color::Rgb(0xce, 0x91, 0x78),
        blue: Color::Rgb(0x56, 0x9c, 0xd6),
        purple: Color::Rgb(0xc5, 0x86, 0xc0),
        cyan: Color::Rgb(0x4e, 0xc9, 0xb0),
        comment: Color::Rgb(0x6a, 0x99, 0x55),
    }
}

impl Theme {
    /// A copy with every colour blended toward the background, for drawing a
    /// dialog that sits *below* the focused one in the stack. `level` is how many
    /// dialogs are above it (1 = just beneath the top); deeper = fainter. The
    /// background itself is left untouched so the faded dialog melts into it.
    pub fn dimmed(&self, level: u32) -> Theme {
        // Fraction moved toward the background; ~55% per level, capped so even
        // deep layers keep a faint outline.
        let amount = (0.55 * level as f32).min(0.85);
        let (br, bgc, bb) = rgb_of(self.bg).unwrap_or((0x1e, 0x1e, 0x1e));
        let blend = |c: Color| match rgb_of(c) {
            Some((r, g, b)) => Color::Rgb(
                lerp(r, br, amount),
                lerp(g, bgc, amount),
                lerp(b, bb, amount),
            ),
            None => c,
        };
        let mut t = self.clone();
        // Everything except `bg` (the canvas) fades toward the background.
        t.bg_dark = blend(t.bg_dark);
        t.bg_light = blend(t.bg_light);
        t.border = blend(t.border);
        t.line_hl = blend(t.line_hl);
        t.indent_guide = blend(t.indent_guide);
        t.fg = blend(t.fg);
        t.fg_dim = blend(t.fg_dim);
        t.accent = blend(t.accent);
        t.accent_fg = blend(t.accent_fg);
        t.status_bg = blend(t.status_bg);
        t.sel_bg = blend(t.sel_bg);
        t.find_match = blend(t.find_match);
        t.find_current = blend(t.find_current);
        t.red = blend(t.red);
        t.green = blend(t.green);
        t.yellow = blend(t.yellow);
        t.orange = blend(t.orange);
        t.blue = blend(t.blue);
        t.purple = blend(t.purple);
        t.cyan = blend(t.cyan);
        t.comment = blend(t.comment);
        t
    }

    /// Apply `#rrggbb` overrides from a `[theme]` config table. Unknown keys and
    /// malformed colours are ignored so a typo never breaks startup.
    pub fn apply_overrides(&mut self, overrides: &HashMap<String, String>) {
        for (key, value) in overrides {
            let Some(color) = parse_hex(value) else {
                continue;
            };
            match key.as_str() {
                "bg" => self.bg = color,
                "bg_dark" => self.bg_dark = color,
                "bg_light" => self.bg_light = color,
                "border" => self.border = color,
                "line_hl" => self.line_hl = color,
                "indent_guide" => self.indent_guide = color,
                "fg" => self.fg = color,
                "fg_dim" => self.fg_dim = color,
                "accent" => self.accent = color,
                "accent_fg" => self.accent_fg = color,
                "status_bg" => self.status_bg = color,
                "sel_bg" => self.sel_bg = color,
                "find_match" => self.find_match = color,
                "find_current" => self.find_current = color,
                "red" => self.red = color,
                "green" => self.green = color,
                "yellow" => self.yellow = color,
                "orange" => self.orange = color,
                "blue" => self.blue = color,
                "purple" => self.purple = color,
                "cyan" => self.cyan = color,
                "comment" => self.comment = color,
                _ => {}
            }
        }
        // Everything derived from the accent has to be re-derived when the
        // accent itself is overridden, or the config silently produces a
        // half-and-half theme: an `accent` override used to leave `status_bg`
        // on the default palette's accent, so picking blue gave a blue UI with
        // a green status bar. Each one is skipped if the user pinned it
        // explicitly — an explicit value always beats a derived one.
        if overrides.contains_key("accent") {
            if !overrides.contains_key("sel_bg") {
                if let Color::Rgb(r, g, b) = self.accent {
                    self.sel_bg = Color::Rgb(dim_channel(r), dim_channel(g), dim_channel(b));
                }
            }
            if !overrides.contains_key("status_bg") {
                self.status_bg = status_bar_bg(self.accent, self.bg);
            }
        }
    }

    /// Style for a tree-sitter capture name, using VSCode Dark+ token colours.
    pub fn syntax_style(&self, capture: &str) -> Style {
        // A couple of token colours don't have a named palette slot.
        const NUMBER: Color = Color::Rgb(0xb5, 0xce, 0xa8); // constants / numbers
        const VARIABLE: Color = Color::Rgb(0x9c, 0xdc, 0xfe); // params / properties

        let base = Style::default();
        match capture {
            "comment" => base.fg(self.comment).add_modifier(Modifier::ITALIC),
            "keyword" => base.fg(self.blue),
            "string" => base.fg(self.orange),
            "type" | "type.builtin" => base.fg(self.cyan),
            "function" | "function.method" | "function.macro" => base.fg(self.yellow),
            "attribute" => base.fg(self.yellow),
            "constant" | "constant.builtin" => base.fg(NUMBER),
            "variable.builtin" => base.fg(self.blue),
            "variable.parameter" | "property" => base.fg(VARIABLE),
            // Plain variables, operators and punctuation stay default fg, matching
            // VSCode (it doesn't tint these in the default TextMate theme).
            _ => base.fg(self.fg),
        }
    }

    /// Re-skin the UI around a new accent colour: the accent itself drives
    /// borders/highlights/markers, and the focused-selection background becomes
    /// a darkened shade of it so the two stay in harmony.
    pub fn set_accent(&mut self, rgb: (u8, u8, u8)) {
        let (r, g, b) = rgb;
        self.accent = Color::Rgb(r, g, b);
        self.sel_bg = Color::Rgb(dim_channel(r), dim_channel(g), dim_channel(b));
        // Re-derived through the same rule the default palette uses, so the
        // bar can never be left on a hue the accent no longer matches.
        self.status_bg = status_bar_bg(self.accent, self.bg);
    }

    /// Foreground pair for text drawn on the status bar: a primary that reads
    /// clearly on `status_bg`, and a muted one for secondary fields. Both are
    /// derived from the bar's own background so they stay legible whatever the
    /// accent sets it to.
    pub fn status_fg(&self) -> (Color, Color) {
        let primary = self.fg;
        (primary, mix(primary, self.status_bg, 0.42))
    }

    /// The current accent as an RGB triple (for persisting to config).
    pub fn accent_rgb(&self) -> (u8, u8, u8) {
        match self.accent {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0x4c, 0xaf, 0x50),
        }
    }

    /// Index of the palette entry matching the current accent, if any.
    pub fn accent_index(&self) -> Option<usize> {
        if let Color::Rgb(r, g, b) = self.accent {
            ACCENT_PALETTE.iter().position(|(_, c)| *c == (r, g, b))
        } else {
            None
        }
    }

    /// The standard "selected row" highlight: VSCode's blue when the owning pane
    /// has focus, a muted grey otherwise — both with readable foregrounds.
    pub fn selection(&self, focused: bool) -> Style {
        if focused {
            Style::default().bg(self.sel_bg).fg(self.accent_fg)
        } else {
            Style::default().bg(self.bg_light).fg(self.fg)
        }
    }
}

/// Parse `#rrggbb` (with or without the leading `#`) into an RGB colour.
fn parse_hex(s: &str) -> Option<Color> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_and_without_hash() {
        assert_eq!(parse_hex("#ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_hex("ff8800"), Some(Color::Rgb(255, 136, 0)));
        assert_eq!(parse_hex("nope"), None);
        assert_eq!(parse_hex("#fff"), None);
    }

    #[test]
    fn overrides_replace_named_colours() {
        let mut theme = Theme::default();
        let mut map = HashMap::new();
        map.insert("accent".to_string(), "#010203".to_string());
        map.insert("status_bg".to_string(), "#0a0b0c".to_string());
        map.insert("unknown".to_string(), "#ffffff".to_string());
        theme.apply_overrides(&map);
        assert_eq!(theme.accent, Color::Rgb(1, 2, 3));
        assert_eq!(theme.status_bg, Color::Rgb(10, 11, 12));
    }

    #[test]
    fn keyword_and_string_differ() {
        let theme = Theme::default();
        assert_ne!(theme.syntax_style("keyword"), theme.syntax_style("string"));
    }

    #[test]
    fn focused_selection_matches_default_accent() {
        let theme = Theme::default();
        // Default accent is green; the selection bg is a darkened shade of it.
        assert_eq!(theme.accent, Color::Rgb(0x4c, 0xaf, 0x50));
        let sel = theme.selection(true);
        assert_eq!(sel.bg, Some(Color::Rgb(0x1c, 0x42, 0x1e)));
        assert_eq!(sel.fg, Some(Color::Rgb(0xff, 0xff, 0xff)));
    }

    #[test]
    fn accent_override_syncs_selection_bg() {
        let mut theme = Theme::default();
        let mut map = HashMap::new();
        map.insert("accent".to_string(), "#e53935".to_string()); // red
        theme.apply_overrides(&map);
        assert_eq!(theme.accent, Color::Rgb(0xe5, 0x39, 0x35));
        // sel_bg derived from the new accent, not left green.
        assert_eq!(theme.sel_bg, Color::Rgb(0x57, 0x15, 0x14));
    }

    /// The status bar belongs to the palette it's drawn in. It used to be
    /// hardcoded to VSCode's `#007acc`, which sat as an unrelated blue band
    /// under a green accent.
    #[test]
    fn the_status_bar_is_derived_from_the_accent() {
        let t = Theme::default();
        assert_eq!(t.status_bg, status_bar_bg(t.accent, t.bg));
        assert_ne!(t.status_bg, Color::Rgb(0x00, 0x7a, 0xcc), "not VSCode's status blue");
        // Between the accent and the background, not level with either: level
        // with `bg` and it stops looking like a bar, level with the accent and
        // it's a stripe of saturated colour across the window.
        assert_ne!(t.status_bg, t.accent);
        assert_ne!(t.status_bg, t.bg);
    }

    #[test]
    fn changing_the_accent_moves_the_status_bar_with_it() {
        let mut theme = Theme::default();
        let before = theme.status_bg;
        theme.set_accent((0xec, 0x40, 0x7a)); // pink
        assert_ne!(theme.status_bg, before, "the bar follows the accent");
        assert_eq!(theme.status_bg, status_bar_bg(theme.accent, theme.bg));
    }
}
