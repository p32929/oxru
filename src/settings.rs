//! **The** settings map — the same idea as [`crate::keys`], for the Settings
//! dialog. Each adjustable preference is declared once in [`SETTINGS`], and
//! everything else reads it:
//!
//! * `app.rs` steps a value with the bounds from the table, and the dialog's
//!   section count *is* the table's length.
//! * `ui.rs` draws the dialog by walking the table — a label, a control drawn
//!   from its [`Kind`], and its hint line.
//! * `config.rs` clamps a hand-edited config file to the same bounds, and
//!   writes each value back under the section and key named here.
//!
//! Before this, a setting was five separate edits: a `match` arm on a bare
//! index in `app.rs`, a `settings_focus == 3` in `ui.rs` plus a hand-written
//! block to draw it, a positional argument threaded through `save_prefs_to`,
//! and an accessor in `config.rs` that re-stated the bounds. The font-size
//! range `8..=72` was written twice and had to agree; so did the section count
//! `5`, which nothing checked. Adding a preference now means adding a row here
//! and answering the two matches the compiler points at.

/// Which preference a row is. An enum rather than a string so the matches over
/// it in `app.rs` and `config.rs` are exhaustive: add a variant and the
/// compiler names every place that has to learn about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Id {
    FontSize,
    TerminalFps,
    DialogSize,
    WordWrap,
    Accent,
}

/// How a preference is stepped and drawn.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// A number ←/→ steps one at a time between bounds. Drawn as `-  N  +`.
    Number {
        min: u32,
        max: u32,
        default: u32,
        /// Printed straight after the value (`"%"`), empty for none.
        suffix: &'static str,
    },
    /// One of a fixed list. Drawn as the whole list, the picked one underlined;
    /// a config value between two entries snaps to the nearest.
    Choice {
        options: &'static [u32],
        default: u32,
    },
    /// On / off — either arrow flips it.
    Toggle { default: bool },
    /// The accent palette, drawn as swatches with a tick on the picked one.
    Palette,
}

/// One row of the Settings dialog.
pub struct Setting {
    pub id: Id,
    /// Section heading in the dialog.
    pub label: &'static str,
    /// The dim line under the control explaining what the arrows do.
    pub hint: &'static str,
    /// The `config.toml` table this is stored in — `gui` or `theme`.
    pub section: &'static str,
    /// The key within that table.
    pub key: &'static str,
    pub kind: Kind,
}

/// Order here is the order the dialog lists them, and the index *is*
/// `App::settings_focus`.
pub const SETTINGS: &[Setting] = &[
    Setting {
        id: Id::FontSize,
        label: "Font size",
        hint: "\u{2190}/\u{2192} to resize",
        section: "gui",
        key: "font_size",
        kind: Kind::Number { min: 8, max: 72, default: 24, suffix: "" },
    },
    Setting {
        id: Id::TerminalFps,
        label: "Terminal FPS",
        hint: "\u{2190}/\u{2192} to change (unattended terminal output only \u{2014} typing is always instant)",
        section: "gui",
        key: "fps",
        // Only *unattended* terminal output (a build log streaming, a spinner —
        // anything the user isn't driving) is throttled to this; typing always
        // redraws immediately regardless.
        kind: Kind::Choice { options: &[1, 5, 10, 20, 30, 60], default: 60 },
    },
    Setting {
        id: Id::DialogSize,
        label: "Dialog size",
        hint: "\u{2190}/\u{2192} to resize",
        section: "gui",
        key: "dialog_size",
        // At 99% a modal isn't a dialog, it's a page: its border hugs the
        // window edge and the surface underneath is completely hidden, so
        // there's no sense of a layer on top of the editor. The range starts
        // low enough for a dialog to read as one; the old maximum is kept for
        // anyone who preferred the full-bleed look. The default leaves a
        // margin of editor visible all the way around, which is what makes a
        // dialog look like it's *on top of* something.
        kind: Kind::Number { min: 50, max: 99, default: 85, suffix: "%" },
    },
    Setting {
        id: Id::WordWrap,
        label: "Word wrap",
        hint: "\u{2190}/\u{2192} to toggle",
        section: "gui",
        key: "word_wrap",
        kind: Kind::Toggle { default: false },
    },
    Setting {
        id: Id::Accent,
        label: "Theme Color",
        hint: "",
        // The accent is a theme colour, so it round-trips through `[theme]`
        // with the rest of the palette rather than living under `[gui]`.
        section: "theme",
        key: "accent",
        kind: Kind::Palette,
    },
];

/// Stand-in for an id with no row — see [`crate::keys::get`] for why this
/// returns a blank instead of panicking. `every_id_has_exactly_one_row` makes
/// the case unreachable.
const MISSING: Setting = Setting {
    id: Id::FontSize,
    label: "",
    hint: "",
    section: "gui",
    key: "",
    kind: Kind::Toggle { default: false },
};

/// The row for `id`. Every variant has exactly one row — asserted by
/// `every_id_has_exactly_one_row`.
pub fn get(id: Id) -> &'static Setting {
    match SETTINGS.iter().find(|s| s.id == id) {
        Some(s) => s,
        None => {
            debug_assert!(false, "no setting row for {id:?}");
            &MISSING
        }
    }
}

/// The dialog's index for `id`, i.e. the `settings_focus` that lands on it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn index_of(id: Id) -> usize {
    SETTINGS.iter().position(|s| s.id == id).unwrap_or(0)
}

/// The FPS choices, for the dialog's picker and the config file's clamp.
pub fn fps_options() -> &'static [u32] {
    match get(Id::TerminalFps).kind {
        Kind::Choice { options, .. } => options,
        _ => &[],
    }
}

/// Hold a hand-edited number inside its declared range. Anything that isn't a
/// [`Kind::Number`] is returned untouched.
pub fn clamp_number(id: Id, value: u32) -> u32 {
    match get(id).kind {
        Kind::Number { min, max, .. } => value.clamp(min, max),
        _ => value,
    }
}

/// Snap a hand-edited value to the nearest offered choice, so a number between
/// two entries doesn't leave the picker showing neither.
pub fn snap_choice(id: Id, value: u32) -> u32 {
    match get(id).kind {
        Kind::Choice { options, default } => options
            .iter()
            .min_by_key(|&&o| (o as i64 - value as i64).abs())
            .copied()
            .unwrap_or(default),
        _ => value,
    }
}

/// The declared default for a numeric or choice row.
pub fn default_number(id: Id) -> u32 {
    match get(id).kind {
        Kind::Number { default, .. } | Kind::Choice { default, .. } => default,
        _ => 0,
    }
}

/// The inclusive bounds of a numeric row, for callers that want to show or
/// test the range rather than clamp to it.
#[cfg_attr(not(test), allow(dead_code))]
pub fn number_range(id: Id) -> (u32, u32) {
    match get(id).kind {
        Kind::Number { min, max, .. } => (min, max),
        _ => (0, 0),
    }
}

/// The declared default for a toggle.
pub fn default_flag(id: Id) -> bool {
    match get(id).kind {
        Kind::Toggle { default } => default,
        _ => false,
    }
}

/// A preference's current value, in the shape the dialog needs to draw it.
/// Keeps `ui.rs` from reaching into `App`'s fields one setting at a time.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Num(u32),
    Flag(bool),
    /// Index into the accent palette.
    Pick(usize),
}

/// Everything the Settings dialog can change, gathered for writing back to
/// `config.toml`. One struct rather than five positional arguments, so adding
/// a preference can't silently skip being persisted.
#[derive(Clone, Copy, Debug)]
pub struct Prefs {
    pub font_size: u32,
    pub fps: u32,
    pub dialog_size: u32,
    pub word_wrap: bool,
    pub accent: (u8, u8, u8),
}

impl Prefs {
    /// The value to write for `id`. `Accent` is a colour, not a number, so it
    /// has no `Value` and is written separately by the config writer.
    pub fn value(&self, id: Id) -> Option<Value> {
        Some(match id {
            Id::FontSize => Value::Num(self.font_size),
            Id::TerminalFps => Value::Num(self.fps),
            Id::DialogSize => Value::Num(self.dialog_size),
            Id::WordWrap => Value::Flag(self.word_wrap),
            Id::Accent => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_has_exactly_one_row() {
        for id in [Id::FontSize, Id::TerminalFps, Id::DialogSize, Id::WordWrap, Id::Accent] {
            let n = SETTINGS.iter().filter(|s| s.id == id).count();
            assert_eq!(n, 1, "{id:?} should appear once, found {n}");
        }
    }

    /// Every row is labelled, and stored somewhere a config file can express.
    #[test]
    fn every_row_is_labelled_and_storable() {
        for s in SETTINGS {
            assert!(!s.label.is_empty(), "{:?} has no label", s.id);
            assert!(!s.key.is_empty(), "{:?} has no config key", s.id);
            assert!(
                matches!(s.section, "gui" | "theme"),
                "{:?} names an unknown config section {:?}",
                s.id,
                s.section
            );
        }
    }

    /// Two rows writing the same key would have them overwrite each other on
    /// save, with whichever ran last winning.
    #[test]
    fn no_two_rows_share_a_config_key() {
        let mut seen = std::collections::HashSet::new();
        for s in SETTINGS {
            assert!(
                seen.insert((s.section, s.key)),
                "{}.{} is claimed by two settings",
                s.section,
                s.key
            );
        }
    }

    /// A default outside its own range would be un-restorable: the config
    /// clamp would move it the moment it was read back.
    #[test]
    fn defaults_are_inside_their_own_bounds() {
        for s in SETTINGS {
            match s.kind {
                Kind::Number { min, max, default, .. } => {
                    assert!(min < max, "{:?} has an empty range", s.id);
                    assert!(
                        (min..=max).contains(&default),
                        "{:?} defaults to {default}, outside {min}..={max}",
                        s.id
                    );
                }
                Kind::Choice { options, default } => {
                    assert!(!options.is_empty(), "{:?} offers no choices", s.id);
                    assert!(
                        options.contains(&default),
                        "{:?} defaults to {default}, which isn't one of the choices",
                        s.id
                    );
                }
                Kind::Toggle { .. } | Kind::Palette => {}
            }
        }
    }

    #[test]
    fn clamping_and_snapping_use_the_declared_ranges() {
        assert_eq!(clamp_number(Id::FontSize, 4), 8);
        assert_eq!(clamp_number(Id::FontSize, 900), 72);
        assert_eq!(clamp_number(Id::DialogSize, 10), 50);
        // 17 is nearer 20 than 10, so the picker shows 20 rather than nothing.
        assert_eq!(snap_choice(Id::TerminalFps, 17), 20);
        assert_eq!(snap_choice(Id::TerminalFps, 1000), 60);
    }

    /// The dialog indexes settings by position, so the order has to be stable.
    #[test]
    fn focus_indices_match_the_table_order() {
        assert_eq!(index_of(Id::FontSize), 0);
        assert_eq!(index_of(Id::Accent), SETTINGS.len() - 1);
    }

    /// The shipped example config documents the `[gui]` keys, and it lives
    /// outside the code that reads them. `padding` is the one key there that
    /// isn't an adjustable preference — it has no dialog row, only a config
    /// line — so it's named here rather than being silently tolerated.
    #[test]
    fn the_example_config_documents_the_settings_it_claims() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/config.toml");
        let text = std::fs::read_to_string(path).expect("examples/config.toml is shipped");
        let doc: toml::Table = text.parse().expect("the example config has to be valid TOML");
        let gui = doc
            .get("gui")
            .and_then(|v| v.as_table())
            .expect("the example config should demonstrate a [gui] table");

        const CONFIG_ONLY: &[&str] = &["padding"];
        let from_table: Vec<&str> = SETTINGS
            .iter()
            .filter(|s| s.section == "gui")
            .map(|s| s.key)
            .collect();
        for key in gui.keys() {
            assert!(
                from_table.contains(&key.as_str()) || CONFIG_ONLY.contains(&key.as_str()),
                "examples/config.toml documents [gui] {key:?}, which is neither a setting nor a known config-only key",
            );
        }
        // The other direction: a preference the dialog can change has to be
        // documented, or nobody editing the file by hand would know it exists.
        for key in from_table {
            assert!(
                gui.contains_key(key),
                "[gui] {key} is an adjustable setting but examples/config.toml never mentions it",
            );
        }
    }

}
