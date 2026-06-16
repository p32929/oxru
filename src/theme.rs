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

#[derive(Debug, Clone)]
pub struct Theme {
    /// Editor background (`editor.background`).
    pub bg: Color,
    /// Sidebar / panel / palette background (`sideBar.background`).
    pub bg_dark: Color,
    /// Subtle fill: inactive selection, borders, separators.
    pub bg_light: Color,
    /// Primary foreground text (`editor.foreground`).
    pub fg: Color,
    /// Dimmed text: line numbers, inactive labels.
    pub fg_dim: Color,
    /// Bright accent for thin elements — focus, hint keys, active markers,
    /// the current line number, palette border. Readable *as text* on a dark bg.
    pub accent: Color,
    /// Readable foreground on top of a filled accent (white).
    pub accent_fg: Color,
    /// Status-bar background (`statusBar.background`, the VSCode blue).
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
        Theme {
            bg: Color::Rgb(0x1e, 0x1e, 0x1e),
            bg_dark: Color::Rgb(0x25, 0x25, 0x26),
            bg_light: Color::Rgb(0x37, 0x37, 0x3d),
            fg: Color::Rgb(0xd4, 0xd4, 0xd4),
            fg_dim: Color::Rgb(0x85, 0x85, 0x85),
            accent: Color::Rgb(0x4c, 0xaf, 0x50),
            accent_fg: Color::Rgb(0xff, 0xff, 0xff),
            status_bg: Color::Rgb(0x00, 0x7a, 0xcc),
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
        // Keep the selection background in harmony with a custom accent unless
        // the user pinned sel_bg explicitly.
        if overrides.contains_key("accent") && !overrides.contains_key("sel_bg") {
            if let Color::Rgb(r, g, b) = self.accent {
                self.sel_bg = Color::Rgb(dim_channel(r), dim_channel(g), dim_channel(b));
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
}
