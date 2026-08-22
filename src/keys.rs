//! **The** keyboard map. Every shortcut Oxru has is declared exactly once, in
//! the [`keys!`] table below, and everything else is derived from it:
//!
//! * [`hit`] answers "did this key event fire that binding?" — `input.rs` asks
//!   it instead of spelling the chord out in a `match` arm.
//! * [`chord`] renders the combo for the reader (`⌘S` on macOS, `Ctrl+S`
//!   elsewhere) — the dialog footers and the F1 sheet ask it instead of
//!   pasting the glyphs in by hand.
//! * [`groups`] builds the whole F1 cheat-sheet straight out of the table.
//!
//! Before this existed the same combo was written out in three places — the
//! `match` arm that implements it, the footer hint that advertises it, and the
//! F1 row that documents it — so they drifted. The terminal footer promised
//! `⌘K` / `⌘1-9` for quick-switch and jump-to-terminal while the bindings were
//! `⌥K` / `⌥1-9`; every footer hardcoded `⌘` and `⌥` glyphs that are wrong off
//! macOS; and find-and-replace was missing from the cheat-sheet entirely.
//! Adding a shortcut now means adding one row here.
//!
//! ## Adding one
//!
//! Add a line to the table: a const name, the modifier family, the key, the
//! long label (F1) and the short one (footers), the F1 group, a pair tag, and
//! whether it needs an open folder. Then use `id::YOUR_NAME` at the two call
//! sites — `keys::hit(...)` in `input.rs`, and the footer list in `ui.rs`.
//! Leave the group empty and it stays out of F1; leave the short label empty
//! and it's F1-only.

use ratatui::crossterm::event::{KeyCode, KeyEvent};

/// Which modifiers a chord wants. The `ctrl` flag callers pass in is already
/// the folded "Ctrl or ⌘" view for Oxru's own shortcuts (see
/// `input::shortcut_mods`), so [`Cmd`](Mods::Cmd) and [`Ctrl`](Mods::Ctrl)
/// *match* identically and differ only in how they read: `⌘S` is a Mac-feeling
/// action key, `⌃Tab` is a chord macOS reserves at the ⌘ level.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mods {
    /// No modifier at all (Enter, Esc, Space, a bare Tab).
    Plain,
    /// Shift only.
    Shift,
    /// ⌘ on macOS, Ctrl elsewhere.
    Cmd,
    /// ⌘⇧ / Ctrl+Shift.
    CmdShift,
    /// Literally Ctrl in both modes — for the chords ⌘ can't have.
    Ctrl,
    /// Ctrl+Shift, shown as ⌃⇧ on macOS too.
    CtrlShift,
    /// ⌥ / Alt.
    Alt,
    /// ⌥⇧ / Alt+Shift.
    AltShift,
    /// The terminal's copy/paste pair: ⌘C on macOS, Ctrl+Shift+C elsewhere.
    SuperShift,
    /// Not a chord at all — a literal string for the cheat-sheet (`← → ↑ ↓`,
    /// `type…`, `Drag · ⇧Click`). Never matches a key event.
    Text,
}

/// One shortcut: what fires it, what it's called, and where it's advertised.
pub struct Shortcut {
    pub id: &'static str,
    pub mods: Mods,
    /// The key itself — `"S"`, `"Tab"`, `"↵"`, `","`, `"1-9"` — or, when
    /// `mods` is [`Mods::Text`], the literal text to print.
    pub key: &'static str,
    /// Long form, for the F1 sheet.
    pub label: &'static str,
    /// Short form for a dialog footer; empty means "F1 only".
    pub short: &'static str,
    /// F1 section title; empty means "footer only, keep it out of F1".
    pub group: &'static str,
    /// Rows sharing a tag merge into one F1 line (`⌘S  ⌘⇧S — Save · Save all`).
    pub pair: &'static str,
    /// Only meaningful with a project open, so F1 hides it on the welcome screen.
    pub folder: bool,
}

/// F1 section titles. Declared once so the table can't typo one into a second
/// section that renders as a near-duplicate heading.
pub const G_GLOBAL: &str = "Global";
pub const G_EDITOR_FILES: &str = "Editor \u{2014} files & tabs";
pub const G_EDITOR_CURSOR: &str = "Editor \u{2014} cursor & selection";
pub const G_FILES: &str = "Files dialog";
pub const G_SEARCH: &str = "Search in Files";
pub const G_TERMINAL: &str = "Terminal";
pub const G_PICKER: &str = "Terminal quick-switch";
pub const G_COPY: &str = "Terminal copy mode";
pub const G_TODOS: &str = "To-do list";
pub const G_CLIP: &str = "Clipboard history";
pub const G_FIND: &str = "Find & replace";
pub const G_SETTINGS: &str = "Settings";
pub const G_RECENT: &str = "Recent folders";
/// Footer-only: handled and advertised in place, but not worth an F1 row.
pub const G_NONE: &str = "";

/// Declare the table and the `id::*` constants from the same line, so a
/// shortcut can't exist without an id or an id point at a shortcut that was
/// deleted. Order here is the order F1 prints.
macro_rules! keys {
    ($($c:ident, $m:ident, $k:expr, $label:expr, $short:expr, $g:expr, $pair:expr, $folder:expr;)*) => {
        /// Stable handles for the table rows. `keys::hit(id::EDITOR_SAVE, …)`.
        ///
        /// Some rows are documentation-only — the cheat-sheet renders them
        /// straight out of `KEYS`, so nothing names their const. They're kept
        /// so every row is addressable the same way.
        #[allow(dead_code)]
        pub mod id {
            $(pub const $c: &str = stringify!($c);)*
        }
        pub const KEYS: &[Shortcut] = &[$(Shortcut {
            id: stringify!($c),
            mods: Mods::$m,
            key: $k,
            label: $label,
            short: $short,
            group: $g,
            pair: $pair,
            folder: $folder,
        },)*];
    };
}

// Arrows and glyphs used in the display-only rows, named so the table reads.
const UP_DN: &str = "\u{2191}\u{2193}";
const ARROWS: &str = "\u{2190} \u{2192} \u{2191} \u{2193}";
const ENTER: &str = "\u{21b5}";
const BKSP: &str = "\u{232b}";

keys! {
    // ── Global ────────────────────────────────────────────────────────────
    FILES_OPEN,        Alt,   "F",   "Open files",                    "Files",        G_GLOBAL, "", true;
    FOLDER_OPEN,       Cmd,   "O",   "Open folder\u{2026}",           "Open other\u{2026}", G_GLOBAL, "", false;
    RECENT_OPEN,       Alt,   "O",   "Recent folders",                "",             G_GLOBAL, "", false;
    SEARCH_FILES_OPEN, CmdShift, "F", "Search in files",              "Search files", G_GLOBAL, "", true;
    TERMINAL_TOGGLE,   Alt,   "T",   "Toggle terminal",               "Term",         G_GLOBAL, "", true;
    WORD_WRAP,         Alt,   "Z",   "Toggle word wrap",              "Wrap",         G_GLOBAL, "", false;
    TODOS_OPEN,        Alt,   "D",   "To-do list",                    "To-do",        G_GLOBAL, "", false;
    CLIP_OPEN,         Alt,   "V",   "Clipboard history",             "Clipboard",    G_GLOBAL, "", false;
    SETTINGS_OPEN,     Cmd,   ",",   "Settings",                      "Settings",     G_GLOBAL, "", false;
    QUIT,              Cmd,   "Q",   "Quit",                          "Quit",         G_GLOBAL, "", false;
    HELP,              Plain, "F1",  "This shortcuts list",           "Shortcuts",    G_GLOBAL, "", false;

    // ── Editor: files & tabs ──────────────────────────────────────────────
    EDITOR_SAVE,      Cmd,       "S",  "Save",              "Save",      G_EDITOR_FILES, "save",  true;
    EDITOR_SAVE_ALL,  CmdShift,  "S",  "Save all",          "Save all",  G_EDITOR_FILES, "save",  true;
    EDITOR_CLOSE,     Cmd,       "W",  "Close tab",         "Close",     G_EDITOR_FILES, "close", true;
    EDITOR_CLOSE_ALL, CmdShift,  "W",  "Close all",         "Close all", G_EDITOR_FILES, "close", true;
    EDITOR_REOPEN,    CmdShift,  "T",  "Reopen closed tab", "Reopen",    G_EDITOR_FILES, "",      true;
    FIND_OPEN,        Cmd,       "F",  "Find in file",      "Find",      G_EDITOR_FILES, "",      true;
    EDITOR_SPLIT,     Cmd,       "\\", "Split / unsplit view", "Split",  G_EDITOR_FILES, "",      true;
    TAB_NEXT,         Ctrl,      "Tab", "Next tab",         "Switch",    G_EDITOR_FILES, "tab",   true;
    TAB_PREV,         CtrlShift, "Tab", "previous",         "",          G_EDITOR_FILES, "tab",   true;
    TAB_MOVE_LEFT,    Ctrl,      "<",  "Move tab left",     "",          G_EDITOR_FILES, "tabmv", true;
    TAB_MOVE_RIGHT,   Ctrl,      ">",  "right",             "",          G_EDITOR_FILES, "tabmv", true;
    TAB_MOVE_HINT,    Text, "\u{2303}\u{21e7}\u{2194}", "Move tab left \u{b7} right", "Move tab", G_NONE, "", true;
    EDITOR_COPY,      Cmd,       "C",  "Copy",              "Copy",      G_EDITOR_FILES, "clip",  true;
    EDITOR_CUT,       Cmd,       "X",  "Cut",               "Cut",       G_EDITOR_FILES, "clip",  true;
    EDITOR_PASTE,     Cmd,       "V",  "Paste",             "Paste",     G_EDITOR_FILES, "clip",  true;
    EDITOR_UNDO,      Cmd,       "Z",  "Undo",              "Undo",      G_EDITOR_FILES, "undo",  true;
    EDITOR_REDO,      CmdShift,  "Z",  "Redo",              "Redo",      G_EDITOR_FILES, "undo",  true;
    EDITOR_SELECT_ALL, Cmd,      "A",  "Select all",        "Select all", G_EDITOR_FILES, "",     true;
    EDITOR_COPY_PATH, CmdShift,  "C",  "Copy path",         "Copy path", G_EDITOR_FILES, "path",  true;
    EDITOR_COPY_REL,  CmdShift,  "R",  "Relative path",     "",          G_EDITOR_FILES, "path",  true;

    // ── Editor: cursor & selection ────────────────────────────────────────
    CURSOR_MOVE,       Text, ARROWS,                      "Move the cursor",                  "",       G_EDITOR_CURSOR, "",      true;
    CURSOR_EXTEND,     Text, "\u{21e7} + any move below", "Extend the selection (mark text)",  "",       G_EDITOR_CURSOR, "",      true;
    CURSOR_SELECT,     Text, "\u{21e7}\u{2190}\u{2191}\u{2193}\u{2192}", "Mark text",         "Select", G_NONE,          "",      true;
    CURSOR_HOME_END,   Text, "Home  End",                 "Line start \u{b7} end",            "",       G_EDITOR_CURSOR, "",      true;
    CURSOR_LINE_START, Cmd,  "\u{2190}",                  "Line start",                       "",       G_EDITOR_CURSOR, "line",  true;
    CURSOR_LINE_END,   Cmd,  "\u{2192}",                  "end",                         "",       G_EDITOR_CURSOR, "line",  true;
    CURSOR_WORD_LEFT,  Alt,  "\u{2190}",                  "Word left",                        "",       G_EDITOR_CURSOR, "word",  true;
    CURSOR_WORD_RIGHT, Alt,  "\u{2192}",                  "right",                       "",       G_EDITOR_CURSOR, "word",  true;
    CURSOR_DOC_START,  Cmd,  "\u{2191}",                  "Document start",                   "",       G_EDITOR_CURSOR, "doc",   true;
    CURSOR_DOC_END,    Cmd,  "\u{2193}",                  "end",                     "",       G_EDITOR_CURSOR, "doc",   true;
    DELETE_WORD,       Alt,  BKSP,                        "Delete word",                      "",       G_EDITOR_CURSOR, "del",   true;
    DELETE_LINE_START, Cmd,  BKSP,                        "to line start",             "",       G_EDITOR_CURSOR, "del",   true;
    INDENT,            Text, "Tab  \u{21e7}Tab",          "Indent \u{b7} outdent selection",  "",       G_EDITOR_CURSOR, "",      true;
    MULTI_CURSOR,      Cmd,  "D",                         "Add caret at next match (multi-cursor)", "Multi-cursor", G_EDITOR_CURSOR, "", true;
    CARET_ABOVE,       Text, "\u{2318}\u{2325}\u{2191}  \u{2318}\u{2325}\u{2193}", "Add caret above \u{b7} below", "Add caret", G_EDITOR_CURSOR, "", true;
    CARET_COLLAPSE,    Plain, "Esc",                      "Collapse to one caret",            "One caret", G_EDITOR_CURSOR, "",   true;

    // ── Files dialog ──────────────────────────────────────────────────────
    FD_NAV,        Text,     UP_DN,   "Navigate",                          "Move",         G_FILES, "",      true;
    FD_EXPAND,     Plain,    "\u{2192}", "Expand folder",                  "Expand",       G_FILES, "fdtree", true;
    FD_COLLAPSE,   Plain,    "\u{2190}", "collapse folder",                "Collapse",     G_FILES, "fdtree", true;
    FD_OPEN,       Plain,    ENTER,   "Open file / fold folder",           "Open",         G_FILES, "",      true;
    FD_TAB_IN,     Plain,    "Tab",   "Search into folder",                "Select",       G_FILES, "fdtab", true;
    FD_TAB_OUT,    Shift,    "Tab",   "out of folder",                     "Out",          G_FILES, "fdtab", true;
    FD_NEW_FILE,   Cmd,      "N",     "New file",                          "New File",     G_FILES, "",      true;
    FD_NEW_FOLDER, AltShift, "N",     "New folder",                        "New Folder",   G_FILES, "",      true;
    FD_RENAME,     Cmd,      "R",     "Rename",                            "Rename",       G_FILES, "",      true;
    FD_DELETE,     Cmd,      "D",     "Delete",                            "Delete",       G_FILES, "",      true;
    FD_COPY_PATH,  CmdShift, "C",     "Copy path",                         "Copy Path",    G_FILES, "fdpath", true;
    FD_COPY_REL,   CmdShift, "R",     "Relative path",                     "Rel Path",     G_FILES, "fdpath", true;
    FD_REVEAL,     Alt,      "R",     "Reveal in Finder",                  "Reveal in Finder", G_FILES, "",  true;
    FD_JUNK,       Alt,      "H",     "Search: show / hide node_modules, build\u{2026}", "Hidden", G_FILES, "", true;
    FD_BACKSPACE,  Text,     BKSP,    "Delete query / out of folder",      "",             G_FILES, "",      true;
    FD_TYPE,       Text,     "type\u{2026}", "Search files",               "",             G_FILES, "",      true;
    FD_CLOSE,      Plain,    "Esc",   "Close",                             "Close",        G_FILES, "",      true;

    // ── Search in files ───────────────────────────────────────────────────
    SF_TYPE,  Text,  "type\u{2026}", "Edit query \u{2014} searches automatically", "", G_SEARCH, "", true;
    SF_NAV,   Text,  UP_DN,  "Navigate results",     "Move",  G_SEARCH, "", true;
    SF_OPEN,  Plain, ENTER,  "Open selected result", "Open",  G_SEARCH, "", true;
    SF_CLOSE, Plain, "Esc",  "Close",                "Close", G_SEARCH, "", true;

    // ── Terminal ──────────────────────────────────────────────────────────
    // The embedded terminal reads raw modifiers so the shell keeps every chord
    // a real terminal app would get — these are written as they're pressed.
    TERM_NEXT,      Ctrl,       "Tab", "Next terminal",     "Next", G_TERMINAL, "tnav", true;
    TERM_PREV,      CtrlShift,  "Tab", "previous",          "Prev", G_TERMINAL, "tnav", true;
    TERM_NEW,       Alt,        "N",   "New terminal",      "New",  G_TERMINAL, "",     true;
    TERM_CLOSE,     Alt,        "W",   "Close terminal",    "Close", G_TERMINAL, "",    true;
    TERM_GRID,      Alt,        "G",   "Grid layout (click a tile to switch)", "Grid", G_TERMINAL, "", true;
    TERM_PICKER,    Alt,        "K",   "Quick-switch terminal (type to filter)", "Switch\u{2026}", G_TERMINAL, "", true;
    TERM_JUMP,      Alt,        "1-9", "Jump to terminal N", "Jump to tab", G_TERMINAL, "", true;
    TERM_MOVE,      Text, "\u{2303}\u{21e7}\u{2194}", "Move terminal left \u{b7} right", "Move", G_TERMINAL, "", true;
    TERM_WORD,      Text, "\u{2325}\u{2190}\u{2192} \u{2303}\u{2190}\u{2192}", "Shell cursor by word", "Word", G_TERMINAL, "", true;
    TERM_LINE,      Text, "\u{2318}\u{2190}  \u{2318}\u{2192}", "Cursor to line start \u{b7} end", "", G_TERMINAL, "", true;
    TERM_DEL,       Text, "\u{2325}\u{232b} \u{2318}\u{232b}", "Delete word \u{b7} to line start", "Word del", G_TERMINAL, "", true;
    TERM_COPY_MODE, Alt,  "\u{2191}", "Copy mode (free cursor + select)", "Select mode", G_TERMINAL, "", true;
    TERM_COPY,      SuperShift, "C",   "Copy selection", "Copy",  G_TERMINAL, "tclip", true;
    TERM_PASTE,     SuperShift, "V",   "Paste",          "Paste", G_TERMINAL, "tclip", true;
    TERM_SCROLL,    Text, "\u{21e7}PgUp/Dn  fn\u{2191}/\u{2193}", "Scroll history", "Scroll", G_TERMINAL, "", true;
    TERM_MARK,      Text, "\u{21e7}\u{2190} \u{2191} \u{2192} \u{2193}", "Mark text (\u{21e7}\u{2325} by word)", "Mark", G_TERMINAL, "", true;
    TERM_MOUSE,     Text, "Drag \u{b7} \u{21e7}Click", "Select w/ mouse (\u{21e7}Click extends)", "", G_TERMINAL, "", true;
    TERM_SOFT_NL,   Text, "\u{2325}\u{21b5}  \u{21e7}\u{21b5}", "Soft newline (don't submit)", "", G_TERMINAL, "", true;

    // ── Terminal quick-switch (⌥K) ────────────────────────────────────────
    PICK_NAV,   Text,  UP_DN,          "Move through the open terminals", "Move",   G_PICKER, "", true;
    PICK_OPEN,  Plain, ENTER,          "Switch to it",                    "Switch", G_PICKER, "", true;
    PICK_TYPE,  Text,  "type\u{2026}", "Filter by folder \u{b7} command", "Filter", G_PICKER, "", true;
    PICK_CLOSE, Plain, "Esc",          "Close",                           "Close",  G_PICKER, "", true;

    // ── Terminal copy mode ────────────────────────────────────────────────
    CM_MOVE,   Text, "\u{2190}\u{2191}\u{2193}\u{2192} / hjkl", "Move cursor", "Move", G_COPY, "", true;
    CM_MARK,   Text, "\u{21e7} + arrows", "Mark / extend selection", "Mark", G_COPY, "", true;
    CM_WORD,   Text, "\u{2325}\u{2190}  \u{2325}\u{2192}", "Move by word", "Word", G_COPY, "", true;
    CM_COPY,   Text, "\u{21b5} / y",  "Copy & exit",    "Copy",   G_COPY, "", true;
    CM_SCROLL, Text, "\u{21e7}PgUp/Dn", "Scroll while selecting", "Scroll", G_COPY, "", true;
    CM_EXIT,   Text, "Esc / q",       "Exit copy mode", "Exit",   G_COPY, "", true;

    // ── To-do list ────────────────────────────────────────────────────────
    TODO_ADD,    Plain,    ENTER,   "Add the typed task",              "Add",        G_TODOS, "", false;
    TODO_TOGGLE, Plain,    "Space", "Toggle the highlighted task",     "Toggle",     G_TODOS, "", false;
    TODO_NAV,    Text,     UP_DN,   "Move through the list",           "Move",       G_TODOS, "", false;
    TODO_PASTE,  Cmd,      "V",     "Paste a list \u{2014} one task per line", "Paste list", G_TODOS, "", false;
    TODO_EDIT,   Cmd,      "E",     "Edit the raw markdown file in a tab", "Edit",   G_TODOS, "", false;
    TODO_DELETE, Cmd,      "D",     "Delete the highlighted task",     "Delete",     G_TODOS, "", false;
    TODO_CLEAR,  CmdShift, "D",     "Clear every completed task",      "Clear done", G_TODOS, "", false;
    TODO_CLOSE,  Plain,    "Esc",   "Close",                           "Close",      G_TODOS, "", false;

    // ── Clipboard history ─────────────────────────────────────────────────
    CLIP_PASTE,  Plain, ENTER,   "Paste where you were",     "Paste",  G_CLIP, "", false;
    CLIP_NAV,    Text,  UP_DN,   "Move through the history", "Move",   G_CLIP, "", false;
    CLIP_TOGGLE, Plain, "Space", "Tick for a multi-entry paste", "Select", G_CLIP, "", false;
    CLIP_CLEAR, Cmd,   "D",   "Clear the history",        "Clear all", G_CLIP, "", false;
    CLIP_CLOSE, Plain, "Esc", "Close",                    "Close",     G_CLIP, "", false;

    // ── Find & replace ────────────────────────────────────────────────────
    FIND_NEXT,           Text,  "\u{21b5}  \u{2193}",          "Next match",     "next",    G_FIND, "", true;
    FIND_PREV,           Text,  "\u{21e7}\u{21b5}  \u{2191}",  "Previous match", "prev",    G_FIND, "", true;
    FIND_REPLACE_TOGGLE, Cmd,   "R",   "Show / hide the replace field",           "replace", G_FIND, "", true;
    FIND_SWITCH_FIELD,   Plain, "Tab", "Switch between the find and replace fields", "",     G_FIND, "", true;
    FIND_REPLACE_ONE,    Plain, ENTER, "Replace this match (from the replace field)", "replace", G_FIND, "", true;
    FIND_REPLACE_ALL,    Shift, ENTER, "Replace every match",                     "all",     G_FIND, "", true;
    FIND_CLOSE,          Plain, "Esc", "Close find",                              "close",   G_FIND, "", true;

    // ── Settings ──────────────────────────────────────────────────────────
    SET_SECTION, Text, "\u{2191} \u{2193}  Tab",              "Switch section", "Section", G_SETTINGS, "", false;
    SET_ADJUST,  Text, "\u{2190} \u{2192}  + \u{2212}",       "Adjust value",   "Change",  G_SETTINGS, "", false;
    SET_CLOSE,   Text, "Esc / \u{21b5}",                      "Close",          "Close",   G_SETTINGS, "", false;

    // ── Recent folders ────────────────────────────────────────────────────
    REC_NAV,    Text,  UP_DN,   "Navigate",              "Move",   G_RECENT, "", false;
    REC_TOGGLE, Plain, "Space", "Toggle selection",      "Check",  G_RECENT, "", false;
    REC_OPEN,   Plain, ENTER,   "Open",                  "Open",   G_RECENT, "", false;
    REC_REMOVE, Text,  BKSP,    "Remove from the list",  "Remove", G_RECENT, "", false;
    REC_CLOSE,  Plain, "Esc",   "Close",                 "Close",  G_RECENT, "", false;

    // ── Cheat-sheet's own footer ──────────────────────────────────────────
    HELP_SCROLL, Text,  "\u{2191}\u{2193}", "Scroll", "Scroll", G_NONE, "", false;
    HELP_PAGE,   Text,  "PgUp/Dn",          "Page",   "Page",   G_NONE, "", false;
    HELP_CLOSE,  Plain, "Esc",              "Close",  "Close",  G_NONE, "", false;
}

/// Stand-in for an id with no row. Unreachable in practice — ids come from the
/// generated `id::*` constants — but returning it rather than panicking means a
/// row deleted out from under a call site costs a blank hint, not a crash mid-
/// keystroke. `every_id_resolves_and_is_unique` fails the build first.
const MISSING: Shortcut = Shortcut {
    id: "",
    mods: Mods::Text,
    key: "",
    label: "",
    short: "",
    group: G_NONE,
    pair: "",
    folder: false,
};

/// Look a row up.
pub fn get(id: &str) -> &'static Shortcut {
    match KEYS.iter().find(|s| s.id == id) {
        Some(s) => s,
        None => {
            debug_assert!(false, "no shortcut with id {id:?} in keys::KEYS");
            &MISSING
        }
    }
}

fn mac() -> bool {
    cfg!(target_os = "macos")
}

/// The chord as the reader should see it: `⌘S` / `Ctrl+S`, `⌥T` / `Alt+T`.
pub fn chord(id: &str) -> String {
    let s = get(id);
    let k = s.key;
    match s.mods {
        Mods::Text | Mods::Plain => k.to_string(),
        Mods::Shift => {
            if mac() {
                format!("\u{21e7}{k}")
            } else {
                format!("Shift+{k}")
            }
        }
        Mods::Cmd => {
            if mac() {
                format!("\u{2318}{k}")
            } else {
                format!("Ctrl+{k}")
            }
        }
        Mods::CmdShift => {
            if mac() {
                format!("\u{2318}\u{21e7}{k}")
            } else {
                format!("Ctrl+Shift+{k}")
            }
        }
        Mods::Ctrl => {
            if mac() {
                format!("\u{2303}{k}")
            } else {
                format!("Ctrl+{k}")
            }
        }
        Mods::CtrlShift => {
            if mac() {
                format!("\u{2303}\u{21e7}{k}")
            } else {
                format!("Ctrl+Shift+{k}")
            }
        }
        Mods::Alt => {
            if mac() {
                format!("\u{2325}{k}")
            } else {
                format!("Alt+{k}")
            }
        }
        Mods::AltShift => {
            if mac() {
                format!("\u{21e7}\u{2325}{k}")
            } else {
                format!("Alt+Shift+{k}")
            }
        }
        // The terminal's copy/paste: ⌘C is the Mac reflex, Ctrl+Shift+C the
        // one every Linux terminal uses (plain Ctrl+C has to stay SIGINT).
        Mods::SuperShift => {
            if mac() {
                format!("\u{2318}{k}")
            } else {
                format!("Ctrl+Shift+{k}")
            }
        }
    }
}

/// The long label — what the F1 sheet prints.
#[allow(dead_code)]
pub fn label(id: &str) -> &'static str {
    get(id).label
}

/// Does `key` (with the caller's already-folded modifier flags) fire this
/// binding? Modifier matching is *exact*: `⌘D` does not fire when Shift is
/// down, because `⌘⇧D` is its own binding. Before, arm order in a `match`
/// decided that, which is why the same combo could mean two things depending
/// on which handler saw it first.
pub fn hit(id: &str, key: &KeyEvent, ctrl: bool, alt: bool, shift: bool) -> bool {
    let s = get(id);
    let (want_ctrl, want_alt, want_shift) = match s.mods {
        Mods::Text => return false,
        Mods::Plain => (false, false, false),
        Mods::Shift => (false, false, true),
        Mods::Cmd | Mods::Ctrl => (true, false, false),
        Mods::CmdShift | Mods::CtrlShift | Mods::SuperShift => (true, false, true),
        Mods::Alt => (false, true, false),
        Mods::AltShift => (false, true, true),
    };
    if ctrl != want_ctrl || alt != want_alt {
        return false;
    }
    // Shift is implicit in a punctuation key the shift layer already produces
    // (`<` is Shift+`,`), so those bindings don't insist on the flag.
    if shift != want_shift && !shifted_punct(s.key) {
        return false;
    }
    key_matches(s.key, key.code, want_shift)
}

/// `<` `>` and friends arrive as their own `KeyCode::Char`, so a chord naming
/// one shouldn't also demand the Shift flag — terminals disagree about
/// whether they report it.
fn shifted_punct(spec: &str) -> bool {
    matches!(spec, "<" | ">" | "_" | "+")
}

fn key_matches(spec: &str, code: KeyCode, want_shift: bool) -> bool {
    match spec {
        // ⌥1 … ⌥9, the jump-to-terminal family.
        "1-9" => matches!(code, KeyCode::Char('1'..='9')),
        // Shift+Tab reaches us as BackTab in the terminal and as Tab+Shift in
        // the window, so a shifted Tab binding has to accept both.
        "Tab" => {
            code == KeyCode::Tab || (want_shift && code == KeyCode::BackTab)
        }
        "Esc" => code == KeyCode::Esc,
        "Space" => code == KeyCode::Char(' '),
        "F1" => code == KeyCode::F(1),
        "\u{21b5}" => code == KeyCode::Enter,      // ↵
        "\u{232b}" => code == KeyCode::Backspace,  // ⌫
        "\u{2190}" => code == KeyCode::Left,
        "\u{2192}" => code == KeyCode::Right,
        "\u{2191}" => code == KeyCode::Up,
        "\u{2193}" => code == KeyCode::Down,
        _ => {
            let mut ch = spec.chars();
            match (ch.next(), ch.next()) {
                // A single character: letters match either case, since Shift
                // (or caps) changes which one the terminal reports.
                (Some(c), None) => match code {
                    KeyCode::Char(got) => got.eq_ignore_ascii_case(&c),
                    _ => false,
                },
                _ => false,
            }
        }
    }
}

/// A footer hint: the chord plus its short label. Falls back to the long label
/// when a row has no short form, so a footer can never print an empty cell.
pub fn hint(id: &str) -> (String, String) {
    let s = get(id);
    let text = if s.short.is_empty() { s.label } else { s.short };
    (chord(id), text.to_string())
}

/// Same, with the label overridden — for the hints that flip with state
/// ("Split" / "Unsplit", "Grid" / "Tabs", "Open" / "Open in this window").
pub fn hint_as(id: &str, text: &str) -> (String, String) {
    (chord(id), text.to_string())
}

/// Same, with the *chord* extended — copy mode's "↵ / y / ⌘C" is one row that
/// names the terminal's copy chord alongside its own keys.
pub fn hint_chord(id: &str, chord_text: String) -> (String, String) {
    let s = get(id);
    let text = if s.short.is_empty() { s.label } else { s.short };
    (chord_text, text.to_string())
}

/// Build a footer row from a list of ids.
pub fn hints(ids: &[&str]) -> Vec<(String, String)> {
    ids.iter().map(|i| hint(i)).collect()
}

/// Borrow a built hint list for [`crate::ui::hint_row`] / `hint_table`.
pub fn as_refs(v: &[(String, String)]) -> Vec<(&str, &str)> {
    v.iter().map(|(k, l)| (k.as_str(), l.as_str())).collect()
}

/// The whole F1 cheat-sheet, straight from the table: sections in declaration
/// order, rows sharing a `pair` tag merged onto one line, and the folder-only
/// sections dropped on the welcome screen.
pub fn groups(has_folder: bool) -> Vec<(&'static str, Vec<(String, String)>)> {
    let mut out: Vec<(&'static str, Vec<(String, String)>)> = Vec::new();
    let mut i = 0;
    while i < KEYS.len() {
        let s = &KEYS[i];
        if s.group.is_empty() || (s.folder && !has_folder) {
            i += 1;
            continue;
        }
        // Merge the run of rows sharing this one's pair tag.
        let mut keys_txt = chord(s.id);
        let mut label_txt = s.label.to_string();
        let mut j = i + 1;
        if !s.pair.is_empty() {
            while j < KEYS.len() && KEYS[j].pair == s.pair && KEYS[j].group == s.group {
                if !(KEYS[j].folder && !has_folder) {
                    keys_txt = format!("{keys_txt}  {}", chord(KEYS[j].id));
                    label_txt = format!("{label_txt} \u{b7} {}", KEYS[j].label);
                }
                j += 1;
            }
        }
        match out.last_mut() {
            Some((g, rows)) if *g == s.group => rows.push((keys_txt, label_txt)),
            _ => out.push((s.group, vec![(keys_txt, label_txt)])),
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn ev(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Every id in the table resolves, and no two rows share one — `get`
    /// panics on a miss, so a stale call site would be a runtime crash.
    #[test]
    fn every_id_resolves_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for s in KEYS {
            assert!(seen.insert(s.id), "duplicate shortcut id {:?}", s.id);
            assert_eq!(get(s.id).id, s.id);
        }
    }

    /// A row that's in a group needs a long label; a row with a short label
    /// needs it to be short enough for a footer cell.
    #[test]
    fn every_row_is_labelled() {
        for s in KEYS {
            assert!(!s.label.is_empty(), "{} has no label", s.id);
            assert!(!s.key.is_empty(), "{} has no key", s.id);
            assert!(
                s.short.chars().count() <= 18,
                "{} short label {:?} is too long for a footer",
                s.id,
                s.short
            );
        }
    }

    /// The point of the table: modifier families are exclusive, so ⌘D and ⌘⇧D
    /// can't both fire on the same press.
    #[test]
    fn shift_variants_do_not_collide() {
        let d = ev(KeyCode::Char('d'));
        assert!(hit(id::TODO_DELETE, &d, true, false, false));
        assert!(!hit(id::TODO_CLEAR, &d, true, false, false));
        assert!(hit(id::TODO_CLEAR, &d, true, false, true));
        assert!(!hit(id::TODO_DELETE, &d, true, false, true));
    }

    /// Ctrl and Alt are different families — neither leaks into the other.
    #[test]
    fn families_do_not_leak() {
        let v = ev(KeyCode::Char('v'));
        assert!(hit(id::TODO_PASTE, &v, true, false, false));
        assert!(!hit(id::TODO_PASTE, &v, false, true, false));
        assert!(hit(id::CLIP_OPEN, &v, false, true, false));
        assert!(!hit(id::CLIP_OPEN, &v, true, false, false));
    }

    /// Letters match whichever case the terminal reports.
    #[test]
    fn letters_are_case_insensitive() {
        assert!(hit(id::EDITOR_SAVE, &ev(KeyCode::Char('s')), true, false, false));
        assert!(hit(id::EDITOR_SAVE, &ev(KeyCode::Char('S')), true, false, false));
    }

    /// Shift+Tab is BackTab in the terminal and Tab+Shift in the window.
    #[test]
    fn shifted_tab_arrives_two_ways() {
        assert!(hit(id::TAB_PREV, &ev(KeyCode::BackTab), true, false, true));
        assert!(hit(id::TAB_PREV, &ev(KeyCode::Tab), true, false, true));
        assert!(hit(id::TAB_NEXT, &ev(KeyCode::Tab), true, false, false));
        assert!(!hit(id::TAB_NEXT, &ev(KeyCode::Tab), true, false, true));
    }

    /// ⌥1 … ⌥9 all fire jump-to-terminal; ⌥0 doesn't.
    #[test]
    fn digit_range_matches_one_to_nine() {
        for c in '1'..='9' {
            assert!(hit(id::TERM_JUMP, &ev(KeyCode::Char(c)), false, true, false));
        }
        assert!(!hit(id::TERM_JUMP, &ev(KeyCode::Char('0')), false, true, false));
    }

    /// Display-only rows exist for the cheat-sheet and must never swallow a key.
    #[test]
    fn text_rows_never_match() {
        assert!(!hit(id::CURSOR_MOVE, &ev(KeyCode::Up), false, false, false));
        assert!(!hit(id::TERM_MARK, &ev(KeyCode::Left), false, false, true));
    }

    /// The cheat-sheet is generated, so the sections it shows track the table.
    #[test]
    fn groups_cover_every_section_when_a_folder_is_open() {
        let g = groups(true);
        let titles: Vec<_> = g.iter().map(|(t, _)| *t).collect();
        for want in [
            G_GLOBAL,
            G_EDITOR_FILES,
            G_EDITOR_CURSOR,
            G_FILES,
            G_SEARCH,
            G_TERMINAL,
            G_PICKER,
            G_COPY,
            G_TODOS,
            G_CLIP,
            G_FIND,
            G_SETTINGS,
            G_RECENT,
        ] {
            assert!(titles.contains(&want), "F1 is missing the {want:?} section");
        }
        // Footer-only rows stay out of it.
        assert!(!titles.contains(&G_NONE));
    }

    /// The welcome screen has no project, so the sections that need one go.
    #[test]
    fn groups_drop_folder_sections_without_a_folder() {
        let titles: Vec<_> = groups(false).iter().map(|(t, _)| *t).collect();
        assert!(titles.contains(&G_GLOBAL));
        assert!(!titles.contains(&G_TERMINAL));
        assert!(!titles.contains(&G_FILES));
        // The global to-do list and clipboard work with no folder open.
        assert!(titles.contains(&G_TODOS));
    }

    /// Paired rows merge into one line rather than printing twice.
    #[test]
    fn paired_rows_merge() {
        let g = groups(true);
        let files = &g.iter().find(|(t, _)| *t == G_EDITOR_FILES).unwrap().1;
        let save = files.iter().find(|(_, l)| l.starts_with("Save")).unwrap();
        assert_eq!(save.1, "Save \u{b7} Save all");
        assert!(save.0.contains(' '), "the two chords should share a cell: {:?}", save.0);
    }

    /// Find-and-replace was absent from the sheet entirely; it's the reason
    /// this table exists, so it stays asserted.
    #[test]
    fn find_group_documents_replace() {
        let g = groups(true);
        let find = &g.iter().find(|(t, _)| *t == G_FIND).unwrap().1;
        let all = find.iter().map(|(k, l)| format!("{k} {l}")).collect::<Vec<_>>().join(" | ");
        assert!(all.contains("Show / hide the replace field"), "{all}");
        assert!(all.contains("Replace every match"), "{all}");
        assert!(all.contains(&chord(id::FIND_REPLACE_TOGGLE)), "{all}");
    }

    /// The terminal footer used to promise ⌘K / ⌘1-9 for bindings that were
    /// ⌥K / ⌥1-9. One table means the hint is the binding.
    #[test]
    fn quick_switch_hint_matches_its_binding() {
        assert!(hit(id::TERM_PICKER, &ev(KeyCode::Char('k')), false, true, false));
        assert!(!hit(id::TERM_PICKER, &ev(KeyCode::Char('k')), true, false, false));
        let shown = chord(id::TERM_PICKER);
        assert!(
            shown.starts_with('\u{2325}') || shown.starts_with("Alt+"),
            "quick-switch is an Alt chord but reads {shown:?}"
        );
    }

    /// Off macOS there is no ⌘ or ⌥ key, so no hint may print one.
    #[test]
    fn no_mac_glyphs_off_mac() {
        if mac() {
            return;
        }
        for s in KEYS {
            if s.mods == Mods::Text {
                continue; // literal cheat-sheet text, written for the Mac reader
            }
            let c = chord(s.id);
            assert!(
                !c.contains('\u{2318}') && !c.contains('\u{2325}') && !c.contains('\u{2303}'),
                "{} renders Mac glyphs off macOS: {c:?}",
                s.id
            );
        }
    }

    /// No source file outside this one may write a ⌘, ⌥ or ⌃ into a string the
    /// user can see. Those keys don't exist off macOS, so a hardcoded glyph is
    /// an instruction to press a key the reader hasn't got — which is what the
    /// terminal footer's ⌘K was. `chord()` renders the right one per platform,
    /// so every mention has to come from the table.
    ///
    /// The sheet's own "⌘ (Command) works anywhere Ctrl does" note is exempt:
    /// it's a sentence *about* the Command key, not an instruction to press a
    /// chord, and it's only shown where naming ⌘ is the point.
    #[test]
    fn no_source_file_hardcodes_a_chord_glyph() {
        const GLYPHS: [char; 3] = ['\u{2318}', '\u{2325}', '\u{2303}'];
        const EXEMPT_SUBSTRINGS: &[&str] = &[
            "(Command) works anywhere Ctrl does", // the F1 sheet's explanatory note
        ];
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut offences: Vec<String> = Vec::new();

        let Ok(entries) = std::fs::read_dir(dir) else {
            panic!("the source directory should be readable");
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if name == "keys.rs" {
                continue; // the table is where these belong
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut in_tests = false;
            for (n, line) in text.lines().enumerate() {
                // Test modules assert *about* chords; their messages are for us.
                if line.trim_start().starts_with("#[cfg(test)]") {
                    in_tests = true;
                }
                if in_tests {
                    continue;
                }
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") {
                    continue; // comments explain, they aren't shown
                }
                if EXEMPT_SUBSTRINGS.iter().any(|e| line.contains(e)) {
                    continue;
                }
                // Only string literals matter — and only the escaped form or a
                // literal glyph, both of which end up on screen verbatim.
                let has_glyph = GLYPHS.iter().any(|g| line.contains(*g))
                    || ["u{2318}", "u{2325}", "u{2303}"].iter().any(|e| line.contains(e));
                if has_glyph && line.contains('"') {
                    offences.push(format!("{name}:{}: {}", n + 1, trimmed));
                }
            }
        }
        assert!(
            offences.is_empty(),
            "these hardcode a modifier glyph instead of asking keys::chord():\n  {}",
            offences.join("\n  ")
        );
    }

}

