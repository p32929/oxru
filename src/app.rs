//! Central application state.
//!
//! The current app is deliberately small: a set of open editor buffers and a
//! file dialog (the single entry point to the filesystem). Everything is driven
//! through a handful of methods called from [`crate::input`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::buffer::{Buffer, DiskStatus};
use crate::config::Config;
use crate::filedialog::FileDialog;
use crate::fstree::{self, FileTree};
use crate::icons::Icons;
use crate::prompt::{Prompt, PromptKind};
use crate::syntax::{self, Span};
use crate::termbridge;
use crate::terminalpane::TerminalPane;
use crate::theme::Theme;

/// How long a toast stays fully on screen before it disappears.
const TOAST_TTL: Duration = Duration::from_millis(2200);

/// The flavour of a toast, which picks its accent colour and icon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// The kinds of modal dialog. They live in a stack ([`App::dialogs`]) so any one
/// can be opened from anywhere — even on top of another — and each appears
/// slightly smaller than the one beneath it. Re-opening a kind that's already in
/// the stack just raises it (no duplicates), so the stack depth is capped at the
/// number of kinds here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialog {
    Files,
    Recent,
    Settings,
    Terminal,
    Help,
}

/// In-file find state (Ctrl+F). Matching is always case-insensitive (ASCII).
/// Each match is a `[start, end)` character range into the active buffer; the
/// "current" one is shown selected so the existing selection highlight and
/// cursor-follow scrolling carry it into view.
#[derive(Default)]
pub struct Find {
    pub active: bool,
    pub query: String,
    /// Cursor and selection anchor within `query` (char indices) — the find box
    /// is a full single-line input, edited via [`crate::editline`].
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub matches: Vec<(usize, usize)>,
    pub current: usize,
    /// Cursor position when find opened — refining the query keeps selecting the
    /// nearest match to *here* rather than walking forward as matches get picked.
    origin: usize,
}

/// Flatten pasted text to a single line — the small inputs (find / dialog query
/// / name prompt) are one line, so newlines become spaces rather than smuggling
/// in line breaks.
fn one_line(s: String) -> String {
    if !s.contains(['\n', '\r']) {
        return s;
    }
    s.split(['\n', '\r']).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Apply a double/triple-click selection to an editor buffer after the cursor
/// was placed at the click: 2 = select the word, 3+ = select the line.
fn apply_click_select(buf: &mut Buffer, clicks: u8) {
    match clicks {
        2 => buf.select_word_at(buf.cursor),
        n if n >= 3 => buf.select_line_at(buf.cursor),
        _ => {}
    }
}

/// One thing the quit flow must confirm before exiting: an unsaved editor or a
/// terminal with a running command. Identified by index into the live lists.
#[derive(Clone, Copy)]
enum QuitBlocker {
    UnsavedFile(usize),
    RunningTerminal(usize),
}

/// A transient corner notification ("Copied", "Saved", …).
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    created: Instant,
}

impl Toast {
    /// Still within its time-to-live (i.e. should be drawn).
    pub fn is_visible(&self) -> bool {
        self.created.elapsed() < TOAST_TTL
    }
}

pub struct App {
    /// The open project folder, or `None` in the welcome (no-folder) state —
    /// e.g. when the app is launched without a folder.
    pub root: Option<PathBuf>,

    /// The open modal dialogs, bottom→top. The last one has focus. See
    /// [`Dialog`]. Per-dialog state lives in the dedicated fields below; this is
    /// the ordering/stacking/focus layer over them.
    pub dialogs: Vec<Dialog>,
    pub tree: FileTree,
    pub editors: Vec<Buffer>,
    pub active_editor: Option<usize>,
    /// In-file find (Ctrl+F): a case-insensitive word search over the active tab.
    pub find: Find,

    // Embedded terminals, shown in a full-screen modal as tabs (or a grid).
    pub terminals: Vec<TerminalPane>,
    pub active_terminal: usize,
    pub terminal_modal: bool,
    pub terminal_grid: bool,
    /// Wakes the GUI event loop when terminal output arrives (set by the GUI).
    terminal_waker: Option<crate::terminalpane::Waker>,
    /// Bytes already consumed from the terminal-bridge request file.
    request_offset: u64,
    /// The active terminal's body rect in global cell coords (x, y, w, h),
    /// recorded during render so mouse events can map to terminal cells.
    pub terminal_view: Option<(u16, u16, u16, u16)>,
    /// True while a mouse drag is selecting terminal text.
    terminal_dragging: bool,
    /// Last drag position (terminal-local row, col) so a drag held against the
    /// top/bottom edge keeps auto-scrolling even when the mouse stops moving.
    terminal_drag_last: Option<(u16, u16)>,
    /// True while the current mouse press is being forwarded to a mouse-reporting
    /// program (so its drag/release route to the program, not oxru's selection).
    /// Lives here, not in the GUI/TUI adapter, so both share one behavior.
    mouse_to_app: bool,
    /// Click-streak tracking for double/triple-click (time + cell of the last
    /// left press, and how many fast clicks have landed on that cell). Drives
    /// word-select (2) and line-select (3) in both the editor and the terminal.
    last_click: Option<(Instant, u16, u16)>,
    click_count: u8,
    /// Set by ⌘/Ctrl+O; the event loop consumes it to pop the native folder
    /// picker (which must run on the main thread, not mid-keypress).
    open_folder_requested: bool,
    /// Last time we checked open files for external changes (throttle).
    last_file_check: Instant,
    /// Cursor-blink clock: `blink_base` is reset to "now" whenever the caret
    /// moves or edits (so the bar is solid the instant you act), and the on/off
    /// phase is derived from the elapsed time since. `blink_key` is the last
    /// `(editor, cursor)` we saw, used to detect that movement.
    blink_base: Instant,
    blink_key: (Option<usize>, usize),

    pub status: String,
    pub should_quit: bool,
    /// Recently-opened files, most-recent first — boosts them to the top of file
    /// search (like VSCode quick open, where the file you just had open is first).
    recent_files: Vec<PathBuf>,
    /// Paths of recently-closed tabs, for "reopen closed tab" (Ctrl+Shift+T).
    closed_tabs: Vec<PathBuf>,
    /// Set when the open-tab set changed; the run loop persists the session.
    pub session_dirty: bool,
    /// Quit-confirmation flow: the blockers still to ask about, and our position
    /// in that queue. Non-empty `quit_queue` means a quit is pending the user's
    /// answers (see [`App::request_quit`]).
    quitting: bool,
    quit_queue: Vec<QuitBlocker>,
    quit_pos: usize,
    pub files_cache: Vec<PathBuf>,

    // The file dialog and its entry list (files + folders, relative display).
    pub prompt: Prompt,
    pub file_dialog: FileDialog,
    pub dialog_entries: Vec<(PathBuf, bool)>,
    pub dialog_display: Vec<String>,
    /// Parallel to `dialog_entries`: `true` for gitignored files, so search
    /// results can be faded the same way the explorer fades ignored entries.
    pub dialog_ignored: Vec<bool>,
    /// Whether search includes the heavy build / dependency dirs (`node_modules`,
    /// `build`, …). Off by default (they only clutter Quick Open); toggled with
    /// ⌥H in the dialog. They're always browsable in the tree regardless.
    pub dialog_show_junk: bool,
    /// When set, the file dialog is scoped to this folder: both the browse tree
    /// and the search corpus are limited to its subtree. `None` = whole project.
    pub dialog_scope: Option<PathBuf>,
    /// First visible row of the shortcuts cheat-sheet (F1), for scrolling.
    pub help_scroll: usize,

    // Look & feel.
    pub theme: Theme,
    pub icons: Icons,
    /// Font size for windowed mode (logical points); read by the GUI backend.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub gui_font_size: u32,

    /// Cached syntax highlight: (editor index, revision, lines).
    hl_cache: std::collections::HashMap<u64, (u64, Vec<Vec<Span>>)>,
    /// Next id to hand to a newly-opened buffer.
    next_buffer_id: u64,
    /// Tile all open files in a grid (split view), like the terminal grid.
    pub editor_grid: bool,
    /// In grid view, each pane's content rect (editor index, (x,y,w,h)) in global
    /// cells — recorded at render time so mouse clicks can map to a pane.
    pub editor_panes: Vec<(usize, (u16, u16, u16, u16))>,
    /// Last rendered editor viewport height, for scroll math.
    pub editor_height: u16,
    /// (editor index, cursor) at the last render — to detect cursor movement so
    /// wheel-scroll can detach the view from the cursor.
    last_cursor: Option<(usize, usize)>,

    // "Recent folders" dialog state.
    pub recent_open: bool,
    /// The recent folders shown in the dialog (newest first).
    pub recent_folders: Vec<PathBuf>,
    /// Cursor row in the recent list.
    pub recent_cursor: usize,
    /// Which rows are checked for multi-open.
    pub recent_checked: Vec<bool>,
    /// Which rows are already open in a window (can't be checked / reopened).
    pub recent_disabled: Vec<bool>,

    // Settings dialog state.
    pub settings_open: bool,
    /// Focused section: 0 = font size, 1 = theme colour.
    pub settings_focus: usize,
    /// Selected index into [`crate::theme::ACCENT_PALETTE`].
    pub settings_color: usize,

    /// The current toast notification, if any (cleared once it expires).
    pub toast: Option<Toast>,

    /// System clipboard handle (lazy; `None` if unavailable on this platform).
    clipboard: Option<arboard::Clipboard>,
    /// In-process clipboard, used as a mirror and a fallback when the system
    /// clipboard can't be reached.
    clipboard_text: String,
}

impl App {
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let config = match &root {
            Some(r) => Config::load(r),
            None => Config::load_global(),
        };
        let theme = config.theme();
        let icons = config.icons();

        let (tree, files_cache) = match &root {
            Some(r) => (FileTree::new(r), fstree::collect_files(r, false)),
            None => (FileTree::empty(), Vec::new()),
        };

        Ok(App {
            root,
            dialogs: Vec::new(),
            tree,
            editors: Vec::new(),
            active_editor: None,
            find: Find::default(),
            terminals: Vec::new(),
            active_terminal: 0,
            terminal_modal: false,
            terminal_grid: false,
            terminal_waker: None,
            request_offset: 0,
            terminal_view: None,
            terminal_dragging: false,
            terminal_drag_last: None,
            mouse_to_app: false,
            last_click: None,
            click_count: 0,
            open_folder_requested: false,
            last_file_check: Instant::now(),
            blink_base: Instant::now(),
            blink_key: (None, 0),
            status: String::new(),
            should_quit: false,
            recent_files: Vec::new(),
            closed_tabs: Vec::new(),
            session_dirty: false,
            quitting: false,
            quit_queue: Vec::new(),
            quit_pos: 0,
            files_cache,
            prompt: Prompt::default(),
            file_dialog: FileDialog::default(),
            dialog_entries: Vec::new(),
            dialog_display: Vec::new(),
            dialog_ignored: Vec::new(),
            dialog_show_junk: false,
            dialog_scope: None,
            help_scroll: 0,
            theme,
            icons,
            gui_font_size: config.gui_font_size(),
            hl_cache: std::collections::HashMap::new(),
            next_buffer_id: 1,
            editor_grid: false,
            editor_panes: Vec::new(),
            editor_height: 20,
            last_cursor: None,
            recent_open: false,
            recent_folders: Vec::new(),
            recent_cursor: 0,
            recent_checked: Vec::new(),
            recent_disabled: Vec::new(),
            settings_open: false,
            settings_focus: 0,
            settings_color: 0,
            toast: None,
            clipboard: arboard::Clipboard::new().ok(),
            clipboard_text: String::new(),
        })
    }

    /// Whether a project folder is open (vs. the welcome / no-folder state).
    pub fn has_folder(&self) -> bool {
        self.root.is_some()
    }

    /// A directory to spawn terminals in: the open folder, else the home dir.
    fn terminal_cwd(&self) -> PathBuf {
        self.root
            .clone()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Replace the open folder with `new_root`, **reusing this window**: clears
    /// editors/terminals, reloads the tree, and re-registers the window so the
    /// recents/"already open" tracking stays correct. The GUI picks up the new
    /// title on its next frame.
    pub fn switch_root(&mut self, new_root: PathBuf) {
        let new_root = new_root.canonicalize().unwrap_or(new_root);
        crate::recent::record(&new_root);
        crate::instances::register(&new_root); // overwrite this PID's marker

        // Persist the outgoing project's open tabs before we drop them.
        self.save_session();

        // Reset per-project state.
        self.dialogs.clear();
        self.editors.clear();
        self.active_editor = None;
        self.terminals.clear();
        self.active_terminal = 0;
        self.terminal_modal = false;
        self.terminal_grid = false;
        self.hl_cache.clear();
        self.closed_tabs.clear();
        self.last_cursor = None;
        self.file_dialog.close();
        self.prompt.close();
        self.recent_open = false;
        self.settings_open = false;

        self.tree = FileTree::new(&new_root);
        self.files_cache = fstree::collect_files(&new_root, false);
        self.root = Some(new_root);
        // Bring back the incoming project's tabs.
        self.restore_session();
    }

    // ---- dialog stack --------------------------------------------------

    /// The dialog on top of the stack (the one receiving input), if any.
    pub fn top_dialog(&self) -> Option<Dialog> {
        self.dialogs.last().copied()
    }

    /// Whether the terminal is the focused (top) dialog — it then owns the mouse
    /// (wheel scroll, drag-select). Buried under another dialog it does not.
    pub fn terminal_focused(&self) -> bool {
        self.top_dialog() == Some(Dialog::Terminal)
    }

    /// Open `d` and give it focus. If it's already open, just raise it to the top
    /// (no duplicate); otherwise initialise its state.
    pub fn open_dialog(&mut self, d: Dialog) {
        if let Some(i) = self.dialogs.iter().position(|x| *x == d) {
            let x = self.dialogs.remove(i);
            self.dialogs.push(x);
            return;
        }
        self.dialogs.push(d);
        match d {
            Dialog::Files => self.init_file_dialog(),
            Dialog::Recent => self.init_recent_dialog(),
            Dialog::Settings => self.init_settings(),
            Dialog::Terminal => {
                if self.terminals.is_empty() {
                    self.new_terminal();
                }
                self.terminal_modal = true;
            }
            Dialog::Help => self.help_scroll = 0,
        }
    }

    /// Toggle `d`: if it's already on top, close it; otherwise open/raise it.
    pub fn toggle_dialog(&mut self, d: Dialog) {
        if self.top_dialog() == Some(d) {
            self.close_top_dialog();
        } else {
            self.open_dialog(d);
        }
    }

    /// Close the focused (top) dialog, revealing whatever is beneath it.
    pub fn close_top_dialog(&mut self) {
        if let Some(d) = self.dialogs.pop() {
            self.teardown_dialog(d);
        }
    }

    /// Close every open dialog/modal. Used when focus moves to a file: opening it
    /// dismisses the file picker *and* anything it was stacked over (a terminal,
    /// say), so the editor is what you see — you can reopen them when needed.
    pub fn close_all_dialogs(&mut self) {
        while let Some(d) = self.dialogs.pop() {
            self.teardown_dialog(d);
        }
    }

    /// Remove `d` from the stack wherever it is (e.g. once its action completes,
    /// like opening a file from the Files dialog).
    pub fn dismiss_dialog(&mut self, d: Dialog) {
        if let Some(i) = self.dialogs.iter().position(|x| *x == d) {
            self.dialogs.remove(i);
            self.teardown_dialog(d);
        }
    }

    /// Per-dialog cleanup when a dialog leaves the stack.
    fn teardown_dialog(&mut self, d: Dialog) {
        match d {
            Dialog::Files => {
                self.file_dialog.close();
                self.dialog_scope = None;
            }
            Dialog::Recent => self.recent_open = false,
            Dialog::Settings => {
                self.persist_settings();
                self.settings_open = false;
            }
            Dialog::Terminal => self.terminal_modal = false,
            Dialog::Help => self.help_scroll = 0,
        }
    }

    // ---- recent folders ------------------------------------------------

    /// Open the "Recent folders" dialog (raising it if already open).
    pub fn open_recent_dialog(&mut self) {
        self.open_dialog(Dialog::Recent);
    }

    /// Load the recent-folders list from disk and mark the dialog active.
    fn init_recent_dialog(&mut self) {
        self.recent_folders = crate::recent::list();
        // Folders already open in some window can't be re-opened.
        let open: Vec<PathBuf> = crate::instances::open_folders()
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        self.recent_disabled = self
            .recent_folders
            .iter()
            .map(|f| {
                let c = f.canonicalize().unwrap_or_else(|_| f.clone());
                open.iter().any(|o| *o == c)
            })
            .collect();
        self.recent_cursor = 0;
        self.recent_checked = vec![false; self.recent_folders.len()];
        self.recent_open = true;
    }


    fn recent_is_disabled(&self, i: usize) -> bool {
        self.recent_disabled.get(i).copied().unwrap_or(false)
    }

    pub fn recent_move(&mut self, delta: i32) {
        let n = self.recent_folders.len();
        if n == 0 {
            return;
        }
        let i = (self.recent_cursor as i32 + delta).rem_euclid(n as i32) as usize;
        self.recent_cursor = i;
    }

    /// Toggle the checkbox on the row under the cursor (no-op if it's already
    /// open in another window).
    pub fn recent_toggle(&mut self) {
        if self.recent_is_disabled(self.recent_cursor) {
            self.notify("Already open in a window", ToastKind::Info);
            return;
        }
        if let Some(c) = self.recent_checked.get_mut(self.recent_cursor) {
            *c = !*c;
        }
    }

    /// Open the checked folders — or, if none are checked, the one under the
    /// cursor — each in its own new window. Returns to caller after spawning.
    /// ⌘/Ctrl+O: ask the event loop to pop the native folder picker. Deferred via
    /// a flag because the picker is a modal that must run on the main thread.
    pub fn request_open_folder(&mut self) {
        self.open_folder_requested = true;
    }

    /// Consume a pending folder-open request (returns true once per request).
    pub fn take_open_folder_request(&mut self) -> bool {
        std::mem::take(&mut self.open_folder_requested)
    }

    /// Open a folder the user picked from the native panel: reuse this window if
    /// it's empty (welcome state), otherwise launch it in a new window — the same
    /// rule the recents picker uses.
    pub fn open_picked_folder(&mut self, path: PathBuf) {
        self.dismiss_dialog(Dialog::Recent);
        if self.has_folder() {
            if spawn_window(&path) {
                self.notify("Opened in a new window", ToastKind::Success);
            }
        } else {
            self.switch_root(path);
        }
    }

    pub fn recent_open_selected(&mut self) {
        // Checked rows that aren't already open.
        let mut targets: Vec<PathBuf> = self
            .recent_folders
            .iter()
            .enumerate()
            .filter(|(i, _)| {
                self.recent_checked.get(*i).copied().unwrap_or(false)
                    && !self.recent_is_disabled(*i)
            })
            .map(|(_, p)| p.clone())
            .collect();
        // Nothing checked: open the folder under the cursor (unless it's open).
        if targets.is_empty() {
            if self.recent_is_disabled(self.recent_cursor) {
                self.notify("That folder is already open", ToastKind::Info);
                return;
            }
            if let Some(p) = self.recent_folders.get(self.recent_cursor) {
                targets.push(p.clone());
            }
        }
        if targets.is_empty() {
            return;
        }
        self.dismiss_dialog(Dialog::Recent);

        if self.has_folder() {
            // A project is already open in this window — keep it untouched and
            // open every pick in its own new window.
            let n = targets.len();
            for path in &targets {
                spawn_window(path);
            }
            self.notify(format!("Opened {n} window(s)"), ToastKind::Success);
        } else {
            // Welcome (no-folder) window: reuse it for the first folder, and open
            // any extras in new windows.
            let mut iter = targets.into_iter();
            let first = iter.next().unwrap();
            let extras: Vec<PathBuf> = iter.collect();
            for path in &extras {
                spawn_window(path);
            }
            if !extras.is_empty() {
                self.notify(format!("Opened +{} window(s)", extras.len()), ToastKind::Success);
            }
            self.switch_root(first);
        }
    }

    // ---- settings ------------------------------------------------------

    pub fn open_settings(&mut self) {
        self.open_dialog(Dialog::Settings);
    }

    /// Open the keyboard-shortcuts cheat-sheet (F1).
    pub fn open_help(&mut self) {
        self.open_dialog(Dialog::Help);
    }

    /// Scroll the cheat-sheet by `delta` rows (clamped at the top; the renderer
    /// clamps the bottom against the actual content height).
    pub fn help_scroll(&mut self, delta: i32) {
        self.help_scroll = (self.help_scroll as i64 + delta as i64).max(0) as usize;
    }

    fn init_settings(&mut self) {
        self.settings_open = true;
        self.settings_focus = 0;
        self.settings_color = self.theme.accent_index().unwrap_or(0);
    }

    /// Persist the chosen size + colour to the global config so they survive a
    /// restart. Called when the dialog is dismissed.
    pub fn persist_settings(&mut self) {
        let result = crate::config::save_prefs(self.gui_font_size, self.theme.accent_rgb());
        match result {
            Ok(()) => self.notify("Settings saved", ToastKind::Success),
            Err(e) => self.notify(format!("Couldn't save settings: {e}"), ToastKind::Error),
        }
    }

    /// The number of focusable sections in the Settings dialog.
    pub const SETTINGS_SECTIONS: usize = 2;

    /// Move between the dialog's sections (font / colour).
    pub fn settings_toggle_focus(&mut self) {
        self.settings_focus = (self.settings_focus + 1) % Self::SETTINGS_SECTIONS;
    }

    /// Left/right within the focused section: resize the font or pick a colour.
    pub fn settings_adjust(&mut self, delta: i32) {
        match self.settings_focus {
            0 => {
                let n = (self.gui_font_size as i32 + delta).clamp(8, 72);
                self.gui_font_size = n as u32;
            }
            _ => {
                let len = crate::theme::ACCENT_PALETTE.len() as i32;
                let idx = (self.settings_color as i32 + delta).rem_euclid(len) as usize;
                self.settings_color = idx;
                self.theme.set_accent(crate::theme::ACCENT_PALETTE[idx].1);
            }
        }
    }

    /// Raise a transient toast notification.
    pub fn notify(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            message: message.into(),
            kind,
            created: Instant::now(),
        });
    }

    /// Drop the toast once it has outlived its time-to-live.
    pub fn expire_toast(&mut self) {
        if self.toast.as_ref().is_some_and(|t| !t.is_visible()) {
            self.toast = None;
        }
    }

    // ---- clipboard / selection ----------------------------------------

    /// Store `text` on the system clipboard (mirrored in-process).
    fn set_clipboard(&mut self, text: String) {
        if let Some(cb) = &mut self.clipboard {
            let _ = cb.set_text(&text);
        }
        self.clipboard_text = text;
    }

    /// Read the clipboard, preferring the system clipboard.
    fn get_clipboard(&mut self) -> String {
        if let Some(cb) = &mut self.clipboard {
            if let Ok(t) = cb.get_text() {
                return t;
            }
        }
        self.clipboard_text.clone()
    }

    /// Copy a file's path to the clipboard — absolute, or relative to the
    /// project root.
    fn copy_path(&mut self, path: &Path, relative: bool) {
        let text = if relative {
            self.root
                .as_deref()
                .and_then(|r| path.strip_prefix(r).ok())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned())
        } else {
            path.to_string_lossy().into_owned()
        };
        let kind = if relative { "relative path" } else { "path" };
        self.notify(format!("Copied {kind}"), ToastKind::Success);
        self.set_clipboard(text);
    }

    /// Copy the selected search result's path (from the file dialog).
    pub fn copy_dialog_path(&mut self, relative: bool) {
        if let Some((p, _)) = self.dialog_selected() {
            self.copy_path(&p, relative);
        }
    }

    /// Copy the active editor file's path.
    pub fn copy_active_path(&mut self, relative: bool) {
        let path = self
            .active_editor
            .and_then(|i| self.editors.get(i))
            .and_then(|b| b.path.clone());
        match path {
            Some(p) => self.copy_path(&p, relative),
            None => self.notify("No file open", ToastKind::Info),
        }
    }

    /// Select the whole active buffer (Ctrl+A).
    pub fn select_all(&mut self) {
        if let Some(buf) = self.active_buffer() {
            buf.select_all();
        }
    }

    /// Cmd+D — select the next occurrence of the current word/selection, adding
    /// a caret there (the headline multi-cursor gesture).
    pub fn add_next_occurrence(&mut self) {
        let added = self.active_buffer().map(|b| b.add_next_occurrence()).unwrap_or(false);
        if !added {
            self.notify("No more matches", ToastKind::Info);
        }
    }

    /// Cmd+Alt+↓ — add a caret on the line below the lowest one.
    pub fn add_caret_below(&mut self) {
        if let Some(b) = self.active_buffer() {
            b.add_caret_below();
        }
    }

    /// Cmd+Alt+↑ — add a caret on the line above the highest one.
    pub fn add_caret_above(&mut self) {
        if let Some(b) = self.active_buffer() {
            b.add_caret_above();
        }
    }

    /// Esc — collapse multiple carets back to one. Returns whether it did.
    pub fn clear_editor_carets(&mut self) -> bool {
        self.active_buffer().map(|b| b.clear_extra_carets()).unwrap_or(false)
    }

    /// VSCode-style caret blink half-period (on for this long, then off).
    const BLINK: Duration = Duration::from_millis(500);

    /// Whether the editor caret should be drawn this frame. Snaps to solid the
    /// instant the caret moves or edits (so it's always visible right when you
    /// act), then blinks on a steady clock.
    pub fn cursor_blink_on(&mut self) -> bool {
        let cur = self
            .active_editor
            .and_then(|i| self.editors.get(i))
            .map(|b| b.cursor)
            .unwrap_or(0);
        let key = (self.active_editor, cur);
        if key != self.blink_key {
            self.blink_key = key;
            self.blink_base = Instant::now();
            return true;
        }
        let half = Self::BLINK.as_millis().max(1);
        (self.blink_base.elapsed().as_millis() / half) % 2 == 0
    }

    /// Copy the active selection to the clipboard (Ctrl+C).
    pub fn copy_selection(&mut self) {
        if let Some(text) = self.active_buffer().and_then(|b| b.selected_text()) {
            let n = text.chars().count();
            self.set_clipboard(text);
            self.notify(format!("Copied {n} chars"), ToastKind::Success);
        }
    }

    /// Undo the last edit in the active buffer (Ctrl+Z).
    pub fn undo(&mut self) {
        if let Some(buf) = self.active_buffer() {
            if !buf.undo() {
                self.notify("Nothing to undo", ToastKind::Info);
            } else {
                self.hl_cache.clear();
            }
        }
    }

    /// Redo the last undone edit (Ctrl+Y / Ctrl+Shift+Z).
    pub fn redo(&mut self) {
        if let Some(buf) = self.active_buffer() {
            if !buf.redo() {
                self.notify("Nothing to redo", ToastKind::Info);
            } else {
                self.hl_cache.clear();
            }
        }
    }

    /// Cut the active selection to the clipboard (Ctrl+X).
    pub fn cut_selection(&mut self) {
        if let Some(text) = self.active_buffer().and_then(|b| b.selected_text()) {
            let n = text.chars().count();
            if let Some(buf) = self.active_buffer() {
                buf.delete_selection();
            }
            self.set_clipboard(text);
            self.notify(format!("Cut {n} chars"), ToastKind::Success);
        }
    }

    /// Insert the clipboard contents at the cursor, replacing any selection
    /// (Ctrl+V).
    pub fn paste(&mut self) {
        let text = self.get_clipboard();
        if text.is_empty() {
            self.notify("Clipboard is empty", ToastKind::Info);
            return;
        }
        let n = text.chars().count();
        if let Some(buf) = self.active_buffer() {
            buf.insert_str(&text);
            self.notify(format!("Pasted {n} chars"), ToastKind::Success);
        }
    }

    /// Delete the active selection, if any, with feedback (Delete/Backspace).
    /// Returns whether a selection was removed.
    pub fn delete_selection(&mut self) -> bool {
        let removed = self
            .active_buffer()
            .map(|b| {
                b.selected_text()
                    .map(|t| t.chars().count())
                    .filter(|_| b.delete_selection())
            })
            .flatten();
        if let Some(n) = removed {
            self.notify(format!("Deleted {n} chars"), ToastKind::Info);
            true
        } else {
            false
        }
    }

    pub fn active_buffer(&mut self) -> Option<&mut Buffer> {
        self.active_editor.and_then(move |i| self.editors.get_mut(i))
    }

    /// Open `path` in an editor, focusing an existing tab if already open.
    pub fn open_path(&mut self, path: &Path) {
        if let Some(i) = self.editors.iter().position(|b| b.path.as_deref() == Some(path)) {
            self.active_editor = Some(i);
            self.touch_recent(path);
            self.close_all_dialogs(); // focus moves to the file
            return;
        }
        // Binary files (images, PDFs, …) aren't text and can't be edited here —
        // leave any open dialog up so another file can be picked.
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if crate::icons::is_binary(&name) {
            self.notify(format!("Can't open {name} — not a text file"), ToastKind::Info);
            return;
        }
        match Buffer::from_path(path) {
            Ok(mut buf) => {
                buf.id = self.next_buffer_id;
                self.next_buffer_id += 1;
                self.editors.push(buf);
                self.active_editor = Some(self.editors.len() - 1);
                self.status = format!("Opened {}", path.display());
                self.touch_recent(path);
                self.close_all_dialogs(); // focus moves to the file
            }
            Err(e) => self.status = format!("Could not open {}: {e}", path.display()),
        }
    }

    /// Most files the search will boost (most-recent first).
    const RECENT_CAP: usize = 64;

    /// Record `path` as the most-recently-used file (front of the MRU list).
    fn touch_recent(&mut self, path: &Path) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(Self::RECENT_CAP);
        self.session_dirty = true;
    }

    /// Toggle the split/grid view — tile all open files like the terminal grid
    /// (left, right, then bottom). No effect with fewer than two files.
    pub fn toggle_editor_grid(&mut self) {
        self.editor_grid = !self.editor_grid;
    }

    /// Reopen the most-recently-closed tab (Ctrl+Shift+T).
    pub fn reopen_closed_tab(&mut self) {
        while let Some(path) = self.closed_tabs.pop() {
            if path.is_file() {
                self.open_path(&path);
                return;
            }
        }
        self.notify("No closed tab to reopen", ToastKind::Info);
    }

    /// Remember a closed file's path so it can be reopened later.
    fn remember_closed(&mut self, path: Option<&Path>) {
        if let Some(p) = path {
            self.closed_tabs.retain(|c| c != p);
            self.closed_tabs.push(p.to_path_buf());
            if self.closed_tabs.len() > 32 {
                self.closed_tabs.remove(0);
            }
        }
        self.session_dirty = true;
    }

    /// Persist the open tabs for the current project (called from the run loop
    /// when `session_dirty`). No-op without a folder.
    pub fn save_session(&mut self) {
        self.session_dirty = false;
        if let Some(root) = &self.root {
            let files: Vec<PathBuf> = self.editors.iter().filter_map(|b| b.path.clone()).collect();
            crate::session::save(root, &files, self.active_editor.unwrap_or(0));
        }
    }

    /// Reopen the files that were open last time this folder was used.
    pub fn restore_session(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let (files, active) = crate::session::load(&root);
        for f in &files {
            self.open_path(f);
        }
        if !self.editors.is_empty() {
            self.active_editor = Some(active.min(self.editors.len() - 1));
        }
        self.session_dirty = false; // restoring isn't a change to persist
    }

    /// MRU position of `path` (0 = most recent), or `None` if not recently used.
    fn recent_rank(&self, path: &Path) -> Option<usize> {
        self.recent_files.iter().position(|p| p == path)
    }

    /// Syntax-highlighted lines for editor `idx` (cached by its buffer id +
    /// revision).
    pub fn highlighted_for(&mut self, idx: usize) -> &[Vec<Span>] {
        let Some(buf) = self.editors.get(idx) else {
            return &[];
        };
        let id = buf.id;
        let rev = buf.revision();
        let stale = match self.hl_cache.get(&id) {
            Some((r, _)) => *r != rev,
            None => true,
        };
        if stale {
            let buf = &self.editors[idx];
            let text = buf.rope.to_string();
            let lang = buf.lang;
            let lines = syntax::highlight(&text, lang, &self.theme);
            self.hl_cache.insert(id, (rev, lines));
        }
        &self.hl_cache.get(&id).unwrap().1
    }

    pub fn ensure_cursor_visible(&mut self, height: u16) {
        self.editor_height = height.max(1);
        let Some(idx) = self.active_editor else {
            return;
        };
        let h = self.editor_height as usize;
        let (row, cursor, line_count, scroll) = {
            let b = &self.editors[idx];
            (b.cursor_row(), b.cursor, b.line_count(), b.scroll_row)
        };
        // Only re-centre on the cursor when it actually moved — so a mouse-wheel
        // scroll (which doesn't move the cursor) detaches the view and stays put.
        let key = (idx, cursor);
        let cursor_moved = self.last_cursor != Some(key);
        self.last_cursor = Some(key);

        let mut next = scroll.min(line_count.saturating_sub(1));
        if cursor_moved {
            if row < next {
                next = row;
            } else if row >= next + h {
                next = row + 1 - h;
            }
        }
        self.editors[idx].scroll_row = next;
    }

    /// Scroll the active editor by `delta` rows (mouse wheel). Positive = down.
    /// The cursor stays put; the view can scroll until the last line reaches the
    /// top.
    pub fn editor_scroll(&mut self, delta: i32) {
        let Some(idx) = self.active_editor else {
            return;
        };
        let buf = &mut self.editors[idx];
        let max = buf.line_count().saturating_sub(1) as i32;
        buf.scroll_row = (buf.scroll_row as i32 + delta).clamp(0, max) as usize;
    }

    // ---- tabs ----------------------------------------------------------

    /// Save the active file.
    pub fn save_active(&mut self) {
        let result = self.active_buffer().map(|buf| buf.save());
        match result {
            Some(Ok(())) => self.notify("Saved", ToastKind::Success),
            Some(Err(e)) => self.notify(format!("Save failed: {e}"), ToastKind::Error),
            None => {}
        }
    }

    /// Save every open file that has unsaved changes (Ctrl+Shift+S).
    pub fn save_all(&mut self) {
        if self.editors.is_empty() {
            return;
        }
        let mut saved = 0usize;
        let mut failed = 0usize;
        for buf in self.editors.iter_mut() {
            if buf.modified {
                match buf.save() {
                    Ok(()) => saved += 1,
                    Err(_) => failed += 1,
                }
            }
        }
        if failed > 0 {
            self.notify(format!("Saved {saved}, {failed} failed"), ToastKind::Error);
        } else if saved == 0 {
            self.notify("All files already saved", ToastKind::Info);
        } else {
            self.notify(format!("Saved {saved} file(s)"), ToastKind::Success);
        }
    }

    /// Close every open file (Ctrl+Shift+W). Files with unsaved changes raise a
    /// save / discard / cancel prompt one at a time before closing.
    pub fn close_all(&mut self) {
        // Handle the first file with unsaved changes by asking the user.
        if let Some(i) = self.editors.iter().position(|b| b.modified) {
            self.active_editor = Some(i);
            self.hl_cache.clear();
            let target = self.editors[i].path.clone().unwrap_or_default();
            self.prompt.open(PromptKind::CloseUnsaved, target, String::new());
            return;
        }
        // Nothing left unsaved — drop everything.
        if self.editors.is_empty() {
            return;
        }
        let closed: Vec<PathBuf> = self.editors.iter().filter_map(|b| b.path.clone()).collect();
        for p in closed {
            self.remember_closed(Some(&p));
        }
        self.editors.clear();
        self.active_editor = None;
        self.hl_cache.clear();
        self.notify("Closed all files", ToastKind::Info);
    }

    /// Answer the per-file "save before close?" prompt: `true` saves, `false`
    /// discards. Either way the file is closed and the close-all flow resumes.
    pub fn close_unsaved_answer(&mut self, save: bool) {
        if let Some(i) = self.editors.iter().position(|b| b.modified) {
            if save {
                if let Err(e) = self.editors[i].save() {
                    // Stop on a save error so changes aren't silently lost.
                    self.prompt.close();
                    self.notify(format!("Save failed: {e}"), ToastKind::Error);
                    return;
                }
            }
            let removed = self.editors.remove(i);
            self.remember_closed(removed.path.as_deref());
            self.active_editor = if self.editors.is_empty() {
                None
            } else {
                Some(self.active_editor.unwrap_or(0).min(self.editors.len() - 1))
            };
            self.hl_cache.clear();
        }
        self.prompt.close();
        // Continue with the next file (or finish closing the rest).
        self.close_all();
    }

    /// Abort the close-all flow, leaving the remaining files open.
    pub fn cancel_close_all(&mut self) {
        self.prompt.close();
        self.notify("Close all cancelled", ToastKind::Info);
    }

    // ---- quit flow -----------------------------------------------------

    /// Begin quitting (Ctrl+Q or the window close button). If any file has
    /// unsaved changes or any terminal is running a command, the user is asked
    /// about each one in turn; saying "don't close" / "cancel" to *any* aborts
    /// the quit. With nothing to confirm, quits immediately.
    pub fn request_quit(&mut self) {
        // Already asking — don't stack a second quit flow on top.
        if self.quitting {
            return;
        }
        let mut queue = Vec::new();
        for (i, b) in self.editors.iter().enumerate() {
            if b.modified {
                queue.push(QuitBlocker::UnsavedFile(i));
            }
        }
        for (i, t) in self.terminals.iter().enumerate() {
            if t.is_running() {
                queue.push(QuitBlocker::RunningTerminal(i));
            }
        }
        if queue.is_empty() {
            self.should_quit = true;
            return;
        }
        self.quitting = true;
        self.quit_queue = queue;
        self.quit_pos = 0;
        self.prompt_next_quit_blocker();
    }

    /// Open a prompt for the next unresolved blocker, or finish quitting when the
    /// queue is exhausted. Nothing is actually closed until the very end, so a
    /// cancel at any point leaves every file and terminal untouched.
    fn prompt_next_quit_blocker(&mut self) {
        while let Some(blocker) = self.quit_queue.get(self.quit_pos).copied() {
            match blocker {
                QuitBlocker::UnsavedFile(i) => {
                    let name = self
                        .editors
                        .get(i)
                        .map(|b| b.name())
                        .unwrap_or_else(|| "file".to_string());
                    self.prompt.open_confirm(
                        PromptKind::QuitUnsaved,
                        format!(
                            "\"{name}\" has unsaved changes.   S = save & quit,  D = discard,  Esc = cancel"
                        ),
                    );
                    return;
                }
                QuitBlocker::RunningTerminal(i) => {
                    let label = self
                        .terminals
                        .get(i)
                        .map(|t| t.display_name())
                        .unwrap_or_else(|| "terminal".to_string());
                    self.prompt.open_confirm(
                        PromptKind::QuitTerminal,
                        format!(
                            "\"{label}\" is still running.   Y = close & quit,  N/Esc = cancel"
                        ),
                    );
                    return;
                }
            }
        }
        // Every blocker approved — exit (terminals are killed and buffers dropped
        // as the process tears down).
        self.quitting = false;
        self.quit_queue.clear();
        self.quit_pos = 0;
        self.should_quit = true;
    }

    /// QuitUnsaved → "save": write the current file, then move to the next.
    pub fn quit_save_current(&mut self) {
        if let Some(QuitBlocker::UnsavedFile(i)) = self.quit_queue.get(self.quit_pos).copied() {
            if let Some(buf) = self.editors.get_mut(i) {
                if let Err(e) = buf.save() {
                    // Don't quit if the save failed — surface it and abort.
                    self.notify(format!("Save failed: {e}"), ToastKind::Error);
                    self.cancel_quit();
                    return;
                }
            }
        }
        self.advance_quit();
    }

    /// "Discard this file" / "close this terminal": approve and move on without
    /// touching anything (the actual teardown happens on exit).
    pub fn quit_skip_current(&mut self) {
        self.advance_quit();
    }

    fn advance_quit(&mut self) {
        self.prompt.close();
        self.quit_pos += 1;
        self.prompt_next_quit_blocker();
    }

    /// The user wants to keep something open — abort the quit entirely, leaving
    /// everything as it was.
    pub fn cancel_quit(&mut self) {
        self.prompt.close();
        self.quitting = false;
        self.quit_queue.clear();
        self.quit_pos = 0;
        self.notify("Quit cancelled", ToastKind::Info);
    }

    /// Cycle to the next / previous open tab (wraps).
    pub fn next_tab(&mut self) {
        if let Some(i) = self.active_editor {
            if !self.editors.is_empty() {
                self.active_editor = Some((i + 1) % self.editors.len());
            }
        }
    }

    pub fn prev_tab(&mut self) {
        if let Some(i) = self.active_editor {
            let n = self.editors.len();
            if n > 0 {
                self.active_editor = Some((i + n - 1) % n);
            }
        }
    }

    // ---- in-file find (Ctrl+F) ----------------------------------------

    /// Open the find bar for the active tab. Prefills with the current selection
    /// (when it's a single word) and jumps to the first match from the cursor.
    pub fn find_open(&mut self) {
        let Some(i) = self.active_editor else {
            return;
        };
        self.find.active = true;
        self.find.origin = self.editors[i].cursor;
        if let Some(text) = self.editors[i].selected_text() {
            if !text.is_empty() && !text.contains('\n') {
                self.find.query = text;
            }
        }
        self.find.cursor = self.find.query.chars().count();
        self.find.anchor = None;
        self.recompute_find();
        self.find_jump_from_origin();
    }

    /// Close the find bar (leaves the last match selected).
    pub fn find_close(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.cursor = 0;
        self.find.anchor = None;
        self.find.matches.clear();
        self.find.current = 0;
    }

    /// Re-run the search and re-jump after the query text changed. Call after any
    /// [`crate::editline`] edit of `self.find.query`.
    pub fn find_changed(&mut self) {
        crate::editline::clamp(&self.find.query, &mut self.find.cursor, &mut self.find.anchor);
        self.recompute_find();
        self.find_jump_from_origin();
    }

    pub fn find_input(&mut self, c: char) {
        crate::editline::insert(
            &mut self.find.query,
            &mut self.find.cursor,
            &mut self.find.anchor,
            c,
        );
        self.find_changed();
    }

    pub fn find_backspace(&mut self) {
        crate::editline::backspace(
            &mut self.find.query,
            &mut self.find.cursor,
            &mut self.find.anchor,
        );
        self.find_changed();
    }

    /// Copy the find box's selection to the clipboard.
    pub fn find_copy(&mut self) {
        if let Some(t) = crate::editline::selected_text(&self.find.query, self.find.cursor, self.find.anchor) {
            self.set_clipboard(t);
        }
    }

    /// Cut the find box's selection to the clipboard.
    pub fn find_cut(&mut self) {
        self.find_copy();
        if crate::editline::delete_selection(&mut self.find.query, &mut self.find.cursor, &mut self.find.anchor) {
            self.find_changed();
        }
    }

    /// Paste the clipboard into the find box (flattened to one line).
    pub fn find_paste(&mut self) {
        let t = one_line(self.get_clipboard());
        if !t.is_empty() {
            crate::editline::insert_str(&mut self.find.query, &mut self.find.cursor, &mut self.find.anchor, &t);
            self.find_changed();
        }
    }

    // ---- file-dialog query editing (single-line input) ----------------

    /// Re-run the fuzzy filter after the query text changed. Call after any
    /// [`crate::editline`] edit of `self.file_dialog.query`.
    pub fn dialog_query_changed(&mut self) {
        crate::editline::clamp(&self.file_dialog.query, &mut self.file_dialog.cursor, &mut self.file_dialog.anchor);
        self.dialog_refilter();
    }

    pub fn dialog_copy_text(&mut self) {
        if let Some(t) = crate::editline::selected_text(&self.file_dialog.query, self.file_dialog.cursor, self.file_dialog.anchor) {
            self.set_clipboard(t);
        }
    }

    pub fn dialog_cut_text(&mut self) {
        self.dialog_copy_text();
        if crate::editline::delete_selection(&mut self.file_dialog.query, &mut self.file_dialog.cursor, &mut self.file_dialog.anchor) {
            self.dialog_query_changed();
        }
    }

    pub fn dialog_paste_text(&mut self) {
        let t = one_line(self.get_clipboard());
        if !t.is_empty() {
            crate::editline::insert_str(&mut self.file_dialog.query, &mut self.file_dialog.cursor, &mut self.file_dialog.anchor, &t);
            self.dialog_query_changed();
        }
    }

    // ---- name-prompt input editing (single-line input) ----------------

    pub fn prompt_copy(&mut self) {
        if let Some(t) = crate::editline::selected_text(&self.prompt.input, self.prompt.cursor, self.prompt.anchor) {
            self.set_clipboard(t);
        }
    }

    pub fn prompt_cut(&mut self) {
        self.prompt_copy();
        crate::editline::delete_selection(&mut self.prompt.input, &mut self.prompt.cursor, &mut self.prompt.anchor);
    }

    pub fn prompt_paste(&mut self) {
        let t = one_line(self.get_clipboard());
        if !t.is_empty() {
            crate::editline::insert_str(&mut self.prompt.input, &mut self.prompt.cursor, &mut self.prompt.anchor, &t);
        }
    }

    /// Recompute all (non-overlapping, case-insensitive) matches of the query in
    /// the active buffer.
    fn recompute_find(&mut self) {
        self.find.matches.clear();
        self.find.current = 0;
        let Some(i) = self.active_editor else {
            return;
        };
        let needle: Vec<char> = self.find.query.chars().map(|c| c.to_ascii_lowercase()).collect();
        let n = needle.len();
        if n == 0 {
            return;
        }
        let hay: Vec<char> = self.editors[i].rope.chars().map(|c| c.to_ascii_lowercase()).collect();
        if hay.len() < n {
            return;
        }
        let mut idx = 0;
        while idx + n <= hay.len() {
            if hay[idx..idx + n] == needle[..] {
                self.find.matches.push((idx, idx + n));
                idx += n; // non-overlapping
            } else {
                idx += 1;
            }
        }
    }

    /// Select the first match at or after where find opened (wrapping to the
    /// first), so refining the query doesn't walk the selection down the file.
    fn find_jump_from_origin(&mut self) {
        if self.find.matches.is_empty() {
            return;
        }
        let origin = self.find.origin;
        self.find.current = self
            .find
            .matches
            .iter()
            .position(|(s, _)| *s >= origin)
            .unwrap_or(0);
        self.select_current_match();
    }

    fn select_current_match(&mut self) {
        let Some(i) = self.active_editor else {
            return;
        };
        if let Some(&(s, e)) = self.find.matches.get(self.find.current) {
            self.editors[i].select_range(s, e);
        }
    }

    /// Jump to the next match (wraps around).
    pub fn find_next(&mut self) {
        if self.find.matches.is_empty() {
            return;
        }
        self.find.current = (self.find.current + 1) % self.find.matches.len();
        self.select_current_match();
    }

    /// Jump to the previous match (wraps around).
    pub fn find_prev(&mut self) {
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.current = (self.find.current + n - 1) % n;
        self.select_current_match();
    }

    /// Close the active tab (Ctrl+W). If the file has unsaved changes, ask first
    /// (save / close without saving / cancel).
    pub fn close_tab(&mut self) {
        let Some(i) = self.active_editor else {
            return;
        };
        if i >= self.editors.len() {
            return;
        }
        if self.editors[i].modified {
            let target = self.editors[i].path.clone().unwrap_or_default();
            self.prompt.open(PromptKind::CloseTab, target, String::new());
            return;
        }
        self.drop_tab(i);
    }

    /// Answer the "unsaved changes" prompt for a single tab close: `true` saves
    /// first, `false` discards. Esc (cancel) is handled by closing the prompt.
    pub fn close_tab_answer(&mut self, save: bool) {
        self.prompt.close();
        let Some(i) = self.active_editor else {
            return;
        };
        if save {
            if let Err(e) = self.editors[i].save() {
                self.notify(format!("Save failed: {e}"), ToastKind::Error);
                return; // keep the tab open if the save failed
            }
        }
        self.drop_tab(i);
    }

    /// Remove tab `i` from the editor list and pick a new active tab.
    fn drop_tab(&mut self, i: usize) {
        let removed = self.editors.remove(i);
        self.remember_closed(removed.path.as_deref());
        self.hl_cache.clear();
        self.active_editor = if self.editors.is_empty() {
            None
        } else {
            Some(i.min(self.editors.len() - 1))
        };
    }

    /// Move the active tab one position left / right.
    pub fn move_tab_left(&mut self) {
        if let Some(i) = self.active_editor {
            if i > 0 {
                self.editors.swap(i, i - 1);
                self.active_editor = Some(i - 1);
                self.hl_cache.clear();
            }
        }
    }

    pub fn move_tab_right(&mut self) {
        if let Some(i) = self.active_editor {
            if i + 1 < self.editors.len() {
                self.editors.swap(i, i + 1);
                self.active_editor = Some(i + 1);
                self.hl_cache.clear();
            }
        }
    }

    // ---- mouse ---------------------------------------------------------

    /// Handle a left-click at character cell (`col`, `row`). Row 0 is the tab
    /// strip; rows below are the editor body. Clicks are ignored while an
    /// overlay (prompt, file dialog, terminal modal) is open.
    /// Select the tab whose label spans `col` in the tab strip. The layout here
    /// mirrors `ui::render_tabs`.
    fn click_editor_tab(&mut self, col: u16) {
        let mut x: u16 = 0;
        for (i, buf) in self.editors.iter().enumerate() {
            let name_w = buf.name().chars().count() as u16;
            let modified_w = if buf.modified { 2 } else { 0 };
            // " " + name + (" \u{25cf}")? + " "  — the trailing "\u{2502}" is a separator.
            let tab_w = 1 + name_w + modified_w + 1;
            if col >= x && col < x + tab_w {
                self.active_editor = Some(i);
                return;
            }
            x += tab_w + 1; // skip the separator column
        }
    }

    /// Move the cursor to the body cell (`col`, `row`). With `keep_anchor`, the
    /// selection anchor is preserved (so a drag extends the selection); without
    /// it, any selection is dropped.
    fn place_cursor_at(&mut self, col: u16, row: u16, keep_anchor: bool) {
        let Some(idx) = self.active_editor else {
            return;
        };
        let buf = &mut self.editors[idx];
        // The editor body starts one row below the tab strip.
        let view_row = row.saturating_sub(1) as usize;
        let gutter_w = (buf.line_count().max(1).to_string().len() as u16).max(3) + 1;
        let target = (buf.scroll_row + view_row).min(buf.line_count().saturating_sub(1));
        let click_col = col.saturating_sub(gutter_w) as usize;
        let c = click_col.min(buf.line_len_chars(target));
        if !keep_anchor {
            buf.clear_selection();
            buf.clear_extra_carets();
        }
        buf.cursor = buf.rope.line_to_char(target) + c;
        buf.goal_col = c;
    }

    /// True when the editor is showing the tiled grid (and there's more than one
    /// file to tile).
    fn in_grid(&self) -> bool {
        self.editor_grid && self.editors.len() > 1
    }

    /// The grid pane `(index, content rect)` under (`col`, `row`), if any.
    fn pane_at(&self, col: u16, row: u16) -> Option<(usize, (u16, u16, u16, u16))> {
        self.editor_panes
            .iter()
            .copied()
            .find(|(_, (x, y, w, h))| col >= *x && col < x + w && row >= *y && row < y + h)
    }

    /// Move the cursor in grid pane `idx` (its content starts at (`px`,`py`)).
    fn place_cursor_in_pane(&mut self, idx: usize, px: u16, py: u16, col: u16, row: u16, keep: bool) {
        let buf = &mut self.editors[idx];
        let view_row = row.saturating_sub(py) as usize;
        let gutter_w = (buf.line_count().max(1).to_string().len() as u16).max(3) + 1;
        let target = (buf.scroll_row + view_row).min(buf.line_count().saturating_sub(1));
        let click_col = col.saturating_sub(px + gutter_w) as usize;
        let c = click_col.min(buf.line_len_chars(target));
        if !keep {
            buf.clear_selection();
            buf.clear_extra_carets();
        }
        buf.cursor = buf.rope.line_to_char(target) + c;
        buf.goal_col = c;
    }

    /// Mouse pressed in the editor: focus the pane (grid view), position the
    /// cursor, and start a (potential) drag-selection anchored there.
    pub fn editor_mouse_down(&mut self, col: u16, row: u16) {
        if self.prompt.active || !self.dialogs.is_empty() || self.active_editor.is_none() {
            return;
        }
        let clicks = self.click_count;
        if self.in_grid() {
            if let Some((idx, (px, py, _, _))) = self.pane_at(col, row) {
                self.active_editor = Some(idx);
                self.place_cursor_in_pane(idx, px, py, col, row, false);
                let buf = &mut self.editors[idx];
                buf.anchor = Some(buf.cursor);
                apply_click_select(buf, clicks);
            }
            return;
        }
        if row == 0 {
            self.click_editor_tab(col);
            return;
        }
        self.place_cursor_at(col, row, false);
        if let Some(buf) = self.active_buffer() {
            buf.anchor = Some(buf.cursor); // anchor the drag here
            apply_click_select(buf, clicks);
        }
    }

    /// Mouse dragged with the button down: extend the selection to (`col`,`row`).
    pub fn editor_mouse_drag(&mut self, col: u16, row: u16) {
        if self.prompt.active || !self.dialogs.is_empty() || self.active_editor.is_none() {
            return;
        }
        if self.in_grid() {
            // Extend within the focused pane, clamping the drag to its bounds.
            if let Some(idx) = self.active_editor {
                if let Some((_, (px, py, pw, ph))) =
                    self.editor_panes.iter().copied().find(|(i, _)| *i == idx)
                {
                    let r = row.clamp(py, py + ph.saturating_sub(1));
                    let c = col.clamp(px, px + pw.saturating_sub(1));
                    self.place_cursor_in_pane(idx, px, py, c, r, true);
                }
            }
            return;
        }
        if row == 0 {
            return;
        }
        self.place_cursor_at(col, row, true);
    }

    /// Mouse released: a click with no movement leaves an empty selection — drop
    /// it so it doesn't register as a selection.
    pub fn editor_mouse_up(&mut self) {
        if let Some(buf) = self.active_buffer() {
            if buf.anchor == Some(buf.cursor) {
                buf.anchor = None;
            }
        }
    }

    // ---- terminals -----------------------------------------------------

    /// Toggle the terminal dialog (Alt+T). Opening it with no terminals spawns
    /// one; if it's already showing but buried under another dialog, this raises
    /// it instead of closing.
    pub fn toggle_terminal_modal(&mut self) {
        // A terminal needs a working directory, so — like the file dialog — it
        // requires an open folder.
        if self.root.is_none() {
            self.notify("Open a folder first (\u{2325}O)", ToastKind::Info);
            return;
        }
        self.toggle_dialog(Dialog::Terminal);
    }

    /// Install the callback that wakes the GUI loop on terminal output. Unused by
    /// the current GUI (the manual `pump_app_events` loop polls every frame), kept
    /// for the waker plumbing and any future event-driven driver.
    #[allow(dead_code)]
    pub fn set_terminal_waker(&mut self, waker: crate::terminalpane::Waker) {
        self.terminal_waker = Some(waker);
    }

    /// Spawn a new terminal pane and focus it, ensuring the terminal dialog is
    /// open. The new pane opens right *beside* the current one (not at the end).
    pub fn new_terminal(&mut self) {
        let cwd = self.terminal_cwd();
        match TerminalPane::new(folder_name(&cwd), 24, 80, &cwd, self.terminal_waker.clone()) {
            Ok(pane) => {
                let at = if self.terminals.is_empty() {
                    0
                } else {
                    self.active_terminal.min(self.terminals.len() - 1) + 1
                };
                self.terminals.insert(at, pane);
                self.active_terminal = at;
                self.terminal_modal = true;
                // Make sure the terminal is in (and on top of) the dialog stack.
                if self.top_dialog() != Some(Dialog::Terminal) {
                    self.open_dialog(Dialog::Terminal);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, cwd = ?cwd, "failed to spawn terminal");
                self.status = format!("Failed to spawn terminal: {e}");
            }
        }
    }

    /// Move the active terminal one position left / right in the tab strip.
    pub fn move_terminal_left(&mut self) {
        let i = self.active_terminal;
        if i > 0 && i < self.terminals.len() {
            self.terminals.swap(i, i - 1);
            self.active_terminal = i - 1;
        }
    }

    pub fn move_terminal_right(&mut self) {
        let i = self.active_terminal;
        if i + 1 < self.terminals.len() {
            self.terminals.swap(i, i + 1);
            self.active_terminal = i + 1;
        }
    }

    /// Close the active terminal; closing the last one hides the dialog.
    pub fn close_terminal(&mut self) {
        if self.terminals.is_empty() {
            return;
        }
        let i = self.active_terminal.min(self.terminals.len() - 1);
        self.terminals.remove(i);
        if self.terminals.is_empty() {
            self.active_terminal = 0;
            self.dismiss_dialog(Dialog::Terminal);
            self.terminal_modal = false;
        } else {
            self.active_terminal = i.min(self.terminals.len() - 1);
        }
    }

    pub fn next_terminal(&mut self) {
        if !self.terminals.is_empty() {
            self.active_terminal = (self.active_terminal + 1) % self.terminals.len();
        }
    }

    pub fn prev_terminal(&mut self) {
        let n = self.terminals.len();
        if n > 0 {
            self.active_terminal = (self.active_terminal + n - 1) % n;
        }
    }

    /// Toggle the auto-arranged grid view (all terminals at once) vs tabs.
    pub fn toggle_terminal_grid(&mut self) {
        self.terminal_grid = !self.terminal_grid;
    }

    pub fn active_terminal_mut(&mut self) -> Option<&mut TerminalPane> {
        self.terminals.get_mut(self.active_terminal)
    }

    // ---- terminal scrolling / selection (mouse + keyboard) -------------

    /// Scroll the active terminal by `delta` rows (positive = up into history).
    pub fn terminal_scroll(&mut self, delta: i32) {
        if let Some(term) = self.active_terminal_mut() {
            term.scroll_lines(delta);
        }
    }

    /// Extend the active terminal's keyboard text selection (Shift+arrows).
    pub fn terminal_select(&mut self, drow: i32, dcol: i32) {
        if let Some(term) = self.active_terminal_mut() {
            term.select_key(drow, dcol);
        }
    }

    /// Extend the terminal's keyboard selection by a word (Shift+Option+arrow).
    pub fn terminal_select_word(&mut self, dir: i32) {
        if let Some(term) = self.active_terminal_mut() {
            term.select_word(dir);
        }
    }

    // ---- terminal copy mode -------------------------------------------

    /// Whether the active terminal is in copy mode (free cursor + select).
    pub fn terminal_copy_mode(&self) -> bool {
        self.terminals
            .get(self.active_terminal)
            .is_some_and(|t| t.copy_mode())
    }

    pub fn terminal_enter_copy_mode(&mut self) {
        if let Some(term) = self.active_terminal_mut() {
            term.enter_copy_mode();
        }
    }

    pub fn terminal_exit_copy_mode(&mut self) {
        if let Some(term) = self.active_terminal_mut() {
            term.exit_copy_mode();
        }
    }

    pub fn terminal_copy_move(&mut self, drow: i32, dcol: i32, select: bool) {
        if let Some(term) = self.active_terminal_mut() {
            term.copy_move(drow, dcol, select);
        }
    }

    pub fn terminal_copy_move_word(&mut self, dir: i32, select: bool) {
        if let Some(term) = self.active_terminal_mut() {
            term.copy_move_word(dir, select);
        }
    }

    /// Copy the copy-mode selection to the clipboard and leave copy mode.
    pub fn terminal_copy_and_exit(&mut self) {
        self.copy_terminal_selection();
        if let Some(term) = self.active_terminal_mut() {
            term.exit_copy_mode();
        }
    }

    /// Scroll the active terminal by a screenful; `dir` +1 = up, -1 = down.
    pub fn terminal_scroll_page(&mut self, dir: i32) {
        if let Some(term) = self.active_terminal_mut() {
            term.scroll_page(dir);
        }
    }

    /// Map a global cell `(col, row)` to active-terminal-local coords, if it
    /// falls inside the recorded terminal body.
    fn terminal_local(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        let (x, y, w, h) = self.terminal_view?;
        if col >= x && col < x + w && row >= y && row < y + h {
            Some((row - y, col - x)) // (term_row, term_col)
        } else {
            None
        }
    }

    // ---- unified mouse entry points (shared by GUI and TUI) -----------
    //
    // Both event loops translate their native mouse events into a global cell
    // `(col, row)` plus a `shift` flag and call these. All routing — terminal vs
    // editor, forward-to-program vs local-selection — lives here so the two
    // modes behave identically and the logic is unit-testable without a window.

    /// Update the click streak for a left press at `(col, row)` and return how
    /// many consecutive fast clicks have landed on that same cell — 1, 2, or 3,
    /// then wrapping back to 1. Drives double-click (word) and triple-click
    /// (line) selection in both the editor and the terminal.
    fn bump_click(&mut self, col: u16, row: u16) -> u8 {
        const DOUBLE_MS: u128 = 400;
        let now = Instant::now();
        let streak = match self.last_click {
            Some((t, c, r))
                if c == col && r == row && now.duration_since(t).as_millis() <= DOUBLE_MS =>
            {
                (self.click_count % 3) + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, col, row));
        self.click_count = streak;
        streak
    }

    /// Left-button press at global cell `(col, row)`. `shift` forces local text
    /// selection even when a program has grabbed the mouse.
    pub fn mouse_down(&mut self, col: u16, row: u16, shift: bool) {
        self.bump_click(col, row);
        if self.terminal_focused() {
            if self.terminal_wants_mouse() && !shift {
                self.mouse_to_app = true;
                self.terminal_mouse_down_report(col, row);
            } else if shift {
                self.mouse_to_app = false;
                self.terminal_mouse_extend(col, row);
            } else {
                self.mouse_to_app = false;
                self.terminal_mouse_down(col, row);
            }
        } else {
            self.mouse_to_app = false;
            self.editor_mouse_down(col, row);
        }
    }

    /// Pointer moved with the left button held.
    pub fn mouse_drag(&mut self, col: u16, row: u16) {
        if self.terminal_focused() {
            if self.mouse_to_app {
                self.terminal_mouse_drag_report(col, row);
            } else {
                self.terminal_mouse_drag(col, row);
            }
        } else {
            self.editor_mouse_drag(col, row);
        }
    }

    /// Left-button release at global cell `(col, row)`.
    pub fn mouse_up(&mut self, col: u16, row: u16) {
        if self.terminal_focused() {
            if self.mouse_to_app {
                self.terminal_mouse_up_report(col, row);
                self.mouse_to_app = false;
            } else {
                self.terminal_mouse_up();
            }
        } else {
            self.editor_mouse_up();
        }
    }

    /// Mouse wheel: `lines` > 0 is wheel-up (toward history / page top).
    pub fn mouse_wheel(&mut self, lines: i32, col: u16, row: u16, shift: bool) {
        if self.terminal_focused() {
            self.terminal_wheel(lines, col, row, shift);
        } else {
            // Editor: wheel-up moves the view toward the top (negative delta).
            self.editor_scroll(-lines);
        }
    }

    /// Per-frame hook: keep a local-selection drag held against an edge
    /// auto-scrolling. No-op during a program-forwarded drag.
    pub fn mouse_drag_tick(&mut self) {
        if !self.mouse_to_app {
            self.terminal_drag_autoscroll();
        }
    }

    /// Whether the active terminal's program has mouse reporting enabled.
    pub fn terminal_wants_mouse(&self) -> bool {
        self.terminals
            .get(self.active_terminal)
            .is_some_and(|t| t.wants_mouse())
    }

    /// Map a global cell to the terminal body and report a mouse event to the
    /// program. `button`: 0 left, 64/65 wheel. `motion` sets the drag bit.
    fn terminal_report(&mut self, col: u16, row: u16, button: u8, release: bool, motion: bool) {
        if let Some((x, y, w, h)) = self.terminal_view {
            let c = col.clamp(x, x + w.saturating_sub(1)) - x;
            let r = row.clamp(y, y + h.saturating_sub(1)) - y;
            let cb = if motion { button | 32 } else { button };
            if let Some(term) = self.active_terminal_mut() {
                term.send_mouse(cb, c, r, release);
            }
        }
    }

    /// Forward a left-button press / drag / release to the program (mouse mode).
    pub fn terminal_mouse_down_report(&mut self, col: u16, row: u16) {
        self.terminal_report(col, row, 0, false, false);
    }
    pub fn terminal_mouse_drag_report(&mut self, col: u16, row: u16) {
        let motion = self
            .terminals
            .get(self.active_terminal)
            .is_some_and(|t| t.wants_mouse_motion());
        if motion {
            self.terminal_report(col, row, 0, false, true);
        }
    }
    pub fn terminal_mouse_up_report(&mut self, col: u16, row: u16) {
        self.terminal_report(col, row, 0, true, false);
    }

    /// Mouse-wheel dispatch. With the program in mouse mode, forward wheel
    /// events to it; on the alternate screen without mouse mode (pagers like
    /// less/man), send arrow keys ("alternate scroll"); otherwise scroll oxru's
    /// own scrollback. Shift always forces local scrollback.
    pub fn terminal_wheel(&mut self, lines: i32, col: u16, row: u16, shift: bool) {
        if lines == 0 {
            return;
        }
        let up = lines > 0;
        let n = (lines.unsigned_abs() as usize).min(10);
        let wants = self.terminal_wants_mouse();
        let on_alt = self
            .terminals
            .get(self.active_terminal)
            .is_some_and(|t| t.on_alternate_screen());
        if !shift && wants {
            for _ in 0..n {
                self.terminal_report(col, row, if up { 64 } else { 65 }, false, false);
            }
        } else if !shift && on_alt {
            let seq: &[u8] = if up { &[0x1b, b'[', b'A'] } else { &[0x1b, b'[', b'B'] };
            if let Some(term) = self.active_terminal_mut() {
                for _ in 0..n {
                    term.send_input(seq);
                }
            }
        } else {
            self.terminal_scroll(lines);
        }
    }

    /// Begin a terminal text selection at the given global cell. A double-click
    /// selects the word under the cell, a triple-click the whole line.
    pub fn terminal_mouse_down(&mut self, col: u16, row: u16) {
        let clicks = self.click_count;
        if let Some((r, c)) = self.terminal_local(col, row) {
            self.terminal_dragging = true;
            if let Some(term) = self.active_terminal_mut() {
                match clicks {
                    2 => term.select_word_cell(r, c),
                    n if n >= 3 => term.select_line_cell(r),
                    _ => term.begin_selection(r, c),
                }
            }
        } else if let Some(term) = self.active_terminal_mut() {
            term.clear_selection();
        }
    }

    /// Shift+Click: extend the current selection to the clicked cell, keeping the
    /// original anchor. Combined with wheel-scroll (which preserves the
    /// selection), this is the reliable way to grab long text: click the start,
    /// scroll until the end is visible, then Shift+Click the end.
    pub fn terminal_mouse_extend(&mut self, col: u16, row: u16) {
        if let Some((r, c)) = self.terminal_local(col, row) {
            self.terminal_dragging = true;
            self.terminal_drag_last = Some((r, c));
            if let Some(term) = self.active_terminal_mut() {
                term.extend_selection(r, c);
            }
        }
    }

    /// Extend the in-progress terminal selection.
    pub fn terminal_mouse_drag(&mut self, col: u16, row: u16) {
        if !self.terminal_dragging {
            return;
        }
        // Clamp to the terminal body so dragging outside still selects sensibly.
        if let Some((x, y, w, h)) = self.terminal_view {
            let c = col.clamp(x, x + w.saturating_sub(1)) - x;
            let r = row.clamp(y, y + h.saturating_sub(1)) - y;
            self.terminal_drag_last = Some((r, c));
            if let Some(term) = self.active_terminal_mut() {
                term.update_selection(r, c);
            }
        }
    }

    /// While a drag is held against the top/bottom edge, keep scrolling and
    /// extending the selection — even when the mouse isn't moving. Called every
    /// frame; returns whether it scrolled (so the caller can mark the UI dirty).
    pub fn terminal_drag_autoscroll(&mut self) -> bool {
        if !self.terminal_dragging {
            return false;
        }
        let Some((r, c)) = self.terminal_drag_last else {
            return false;
        };
        let Some((_, _, _, h)) = self.terminal_view else {
            return false;
        };
        // Only the very top / bottom rows trigger the conveyor.
        if r == 0 || r + 1 >= h {
            if let Some(term) = self.active_terminal_mut() {
                // update_selection re-applies the edge auto-scroll for this row.
                term.update_selection(r, c);
            }
            return true;
        }
        false
    }

    /// Finish a terminal selection drag.
    pub fn terminal_mouse_up(&mut self) {
        self.terminal_dragging = false;
        self.terminal_drag_last = None;
    }

    /// Copy the active terminal's current selection to the clipboard.
    pub fn copy_terminal_selection(&mut self) {
        let text = self.active_terminal_mut().and_then(|t| t.selection_text());
        if let Some(text) = text {
            if !text.trim().is_empty() {
                let n = text.chars().count();
                self.set_clipboard(text);
                self.notify(format!("Copied {n} chars"), ToastKind::Success);
            }
        }
    }

    /// Paste the clipboard into the active terminal (Cmd+V / Ctrl+Shift+V),
    /// honouring **bracketed paste** mode when the running program enabled it so
    /// shells/editors treat it as a literal paste rather than typed input.
    pub fn paste_terminal(&mut self) {
        let text = self.get_clipboard();
        if text.is_empty() {
            return;
        }
        if let Some(term) = self.active_terminal_mut() {
            term.paste(&text);
        }
    }

    /// Tab labels for the terminals, with duplicate names disambiguated by an
    /// index (e.g. three idle "tests" shells become "tests", "tests 2",
    /// "tests 3") so they're still tellable apart.
    pub fn terminal_labels(&self) -> Vec<String> {
        use std::collections::HashMap;
        let names: Vec<String> = self.terminals.iter().map(|t| t.display_name()).collect();
        let mut total: HashMap<&str, usize> = HashMap::new();
        for n in &names {
            *total.entry(n.as_str()).or_default() += 1;
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        names
            .iter()
            .map(|n| {
                if total[n.as_str()] > 1 {
                    let c = seen.entry(n.as_str()).or_insert(0);
                    *c += 1;
                    if *c == 1 {
                        n.clone()
                    } else {
                        format!("{n} {c}")
                    }
                } else {
                    n.clone()
                }
            })
            .collect()
    }

    /// Open a new embedded terminal that runs `cmd` — used when a script in a
    /// terminal asks to launch a command in its own window.
    fn open_terminal_with_command(&mut self, cmd: &str) {
        self.new_terminal();
        if let Some(term) = self.active_terminal_mut() {
            term.folder = short_term_title(cmd);
            term.send_input(format!("{cmd}\n").as_bytes());
        }
        self.terminal_modal = true;
        self.notify("Opened embedded terminal", ToastKind::Info);
    }

    /// Drain the terminal-bridge request file and open an embedded terminal for
    // ---- external file changes ----------------------------------------

    /// Detect files that changed on disk underneath us (e.g. a command in the
    /// terminal rewrote an open file) and react: silently reload an unmodified
    /// buffer, prompt on a real conflict, flag a deletion. Called every frame
    /// from both run loops; throttled internally so it's cheap.
    pub fn poll_file_changes(&mut self) {
        // A prompt is already up (this one, or a close/quit choice) — don't stack.
        if self.prompt.active {
            return;
        }
        if self.last_file_check.elapsed() < Duration::from_millis(800) {
            return;
        }
        self.last_file_check = Instant::now();

        for i in 0..self.editors.len() {
            match self.editors[i].disk_status() {
                DiskStatus::Unchanged => {}
                DiskStatus::Deleted => {
                    let name = self.editors[i].name();
                    // Keep the user's content; mark dirty so a save restores it.
                    self.editors[i].modified = true;
                    self.editors[i].forget_disk(); // report once, then stop
                    self.notify(format!("{name} was deleted on disk"), ToastKind::Error);
                }
                DiskStatus::Modified => {
                    if self.editors[i].modified {
                        // Unsaved edits + external change = conflict: ask, one at a
                        // time (the prompt blocks further polling until answered).
                        self.open_external_change_prompt(i);
                        break;
                    }
                    // Clean buffer: just adopt the new contents.
                    let name = self.editors[i].name();
                    match self.editors[i].reload_from_disk() {
                        Ok(()) => {
                            self.notify(format!("Reloaded {name} (changed on disk)"), ToastKind::Info)
                        }
                        Err(e) => {
                            self.notify(format!("Couldn't reload {name}: {e}"), ToastKind::Error)
                        }
                    }
                }
            }
        }
    }

    fn open_external_change_prompt(&mut self, editor: usize) {
        let path = self.editors[editor].path.clone().unwrap_or_default();
        self.prompt
            .open(crate::prompt::PromptKind::ExternalChange, path, String::new());
    }

    /// Answer the external-change conflict: `reload` = take the disk version
    /// (discard edits); otherwise keep the in-memory edits and stop nagging.
    pub fn external_change_answer(&mut self, reload: bool) {
        let path = self.prompt.target.clone();
        self.prompt.close();
        let Some(i) = self
            .editors
            .iter()
            .position(|b| b.path.as_deref() == Some(path.as_path()))
        else {
            return;
        };
        if reload {
            let name = self.editors[i].name();
            match self.editors[i].reload_from_disk() {
                Ok(()) => self.notify(format!("Reloaded {name}"), ToastKind::Info),
                Err(e) => self.notify(format!("Couldn't reload: {e}"), ToastKind::Error),
            }
        } else {
            // Keep mine: accept the disk state as the baseline so we don't re-ask.
            self.editors[i].rebaseline_disk();
        }
    }

    /// Force the next `poll_file_changes` to run immediately (e.g. on regaining
    /// window focus, where a check is worth doing right away).
    pub fn recheck_files_soon(&mut self) {
        self.last_file_check = Instant::now() - Duration::from_secs(1);
    }

    /// each queued command. Called once per frame from the run loop.
    pub fn poll_terminal_requests(&mut self) {
        let Some(path) = termbridge::request_file() else {
            return;
        };
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if len < self.request_offset {
            self.request_offset = 0; // file was rotated/truncated
        }
        if len <= self.request_offset {
            return;
        }
        let mut file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return,
        };
        use std::io::{Read, Seek, SeekFrom};
        if file.seek(SeekFrom::Start(self.request_offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return;
        }
        // Only consume whole lines so a half-written request waits for next time.
        let Some(end) = buf.rfind('\n') else {
            return;
        };
        self.request_offset += (end + 1) as u64;
        let cmds: Vec<String> = buf[..=end]
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        for cmd in cmds {
            self.open_terminal_with_command(&cmd);
        }
    }

    // ---- file dialog --------------------------------------------------

    /// Rebuild the search list: **files only**, by relative path — matching
    /// VSCode's quick-open, which lists files (folders are browsed in the tree,
    /// shown when the query is empty). When a [`dialog_scope`](Self::dialog_scope)
    /// is set the corpus is limited to that folder's subtree; paths still display
    /// relative to the project root so they read in full context.
    fn rebuild_dialog_entries(&mut self) {
        let Some(root) = self.root.clone() else {
            self.dialog_entries.clear();
            self.dialog_display.clear();
            self.dialog_ignored.clear();
            return;
        };
        let walk_root = self.dialog_scope.clone().unwrap_or_else(|| root.clone());
        let marked = fstree::collect_files_marked(&walk_root, self.dialog_show_junk);
        self.dialog_ignored = marked.iter().map(|(_, ig)| *ig).collect();
        // The tuple's bool is "is a file" (always true here — collect_* yields
        // only files); the gitignored flag lives in `dialog_ignored`.
        self.dialog_entries = marked.into_iter().map(|(p, _)| (p, false)).collect();
        self.dialog_display = self
            .dialog_entries
            .iter()
            .map(|(p, _)| {
                p.strip_prefix(&root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
    }

    /// Open the file dialog (the app's single entry point to files). With an
    /// empty query it shows a browsable **tree**; typing switches to a flat
    /// fuzzy-search list.
    pub fn open_file_dialog(&mut self) {
        // No folder open: point the user at the recents picker instead.
        if self.root.is_none() {
            self.notify("Open a folder first (\u{2325}O)", ToastKind::Info);
            return;
        }
        self.open_dialog(Dialog::Files);
    }

    fn init_file_dialog(&mut self) {
        // Start unscoped, browsing the whole project from a fresh tree.
        self.dialog_scope = None;
        if let Some(root) = self.root.clone() {
            self.tree = FileTree::new(&root);
        }
        self.rebuild_dialog_entries();
        self.file_dialog.open();
        self.dialog_refilter();
    }

    pub fn dialog_refilter(&mut self) {
        let display = self.dialog_display.clone();
        // MRU position of each entry, so recently-opened files rank to the top.
        let recent: Vec<Option<usize>> = self
            .dialog_entries
            .iter()
            .map(|(p, _)| self.recent_rank(p))
            .collect();
        self.file_dialog.refilter(&display, &recent);
    }

    /// Toggle whether search includes the heavy build / dependency dirs
    /// (`node_modules`, `build`, `.dart_tool`, …). They're always browsable in the
    /// tree; this only changes the flat search corpus. Bound to ⌥H in the dialog.
    pub fn dialog_toggle_junk(&mut self) {
        self.dialog_show_junk = !self.dialog_show_junk;
        self.rebuild_dialog_entries();
        self.dialog_refilter();
        let msg = if self.dialog_show_junk {
            "Search now includes node_modules / build dirs (\u{2325}H)"
        } else {
            "Search hides node_modules / build dirs (\u{2325}H)"
        };
        self.notify(msg, ToastKind::Info);
    }

    /// Whether the dialog is showing the browsable **tree** (empty query) rather
    /// than the flat fuzzy-search list (VSCode quick-open style). Typing switches
    /// to search; clearing the query switches back to the tree.
    pub fn dialog_tree_mode(&self) -> bool {
        self.file_dialog.query.is_empty()
    }

    /// The selected `(path, is_dir)` in whichever view is showing.
    fn dialog_selected(&self) -> Option<(PathBuf, bool)> {
        if self.dialog_tree_mode() {
            self.tree.selected()
        } else {
            self.file_dialog
                .selected_source()
                .and_then(|i| self.dialog_entries.get(i).cloned())
        }
    }

    /// Where a new file/folder should be created: the selected directory, the
    /// parent of the selected file, or the project root.
    fn dialog_base_dir(&self) -> PathBuf {
        let root = self.root.clone().unwrap_or_else(|| PathBuf::from("."));
        match self.dialog_selected() {
            Some((p, true)) => p,
            Some((p, false)) => p.parent().map(|x| x.to_path_buf()).unwrap_or(root),
            None => root,
        }
    }

    /// Enter. In tree mode: open the highlighted file, or expand/collapse the
    /// highlighted folder. In search mode: open every ticked file, or — if none
    /// are ticked — the highlighted one.
    pub fn dialog_open_selected(&mut self) {
        if self.dialog_tree_mode() {
            match self.tree.selected() {
                // open_path opens it and dismisses the dialog (binary files toast
                // and leave the dialog up).
                Some((path, false)) => self.open_path(&path),
                Some((_, true)) => self.toggle_tree_selected(),
                None => {}
            }
            return;
        }
        if !self.file_dialog.checked.is_empty() {
            // Open all ticked files, in entry order; the last opened is focused.
            let paths: Vec<PathBuf> = (0..self.dialog_entries.len())
                .filter(|i| self.file_dialog.checked.contains(i))
                .filter_map(|i| match &self.dialog_entries[i] {
                    (p, false) => Some(p.clone()),
                    _ => None,
                })
                .collect();
            self.dismiss_dialog(Dialog::Files);
            for p in &paths {
                self.open_path(p);
            }
            return;
        }
        if let Some((path, false)) = self.dialog_selected() {
            self.open_path(&path);
        }
    }

    /// Tick/untick the highlighted file for multi-open (search mode only).
    pub fn dialog_toggle_check(&mut self) {
        if let Some(src) = self.file_dialog.selected_source() {
            if matches!(self.dialog_entries.get(src), Some((_, false))) {
                self.file_dialog.toggle_check(src);
            }
        }
    }

    /// Tab. In the tree: drill the dialog *into* the highlighted folder, so the
    /// search box now only finds files within it (a breadcrumb shows the scope).
    /// In search mode: keep the fzf-style tick-and-advance behaviour.
    pub fn dialog_tab(&mut self) {
        if self.dialog_tree_mode() {
            self.dialog_scope_into();
        } else {
            self.dialog_toggle_check();
            self.dialog_down();
        }
    }

    /// Shift+Tab. In the tree: drill back *out* of the scoped folder (the mirror
    /// of Tab drilling in). In search mode: tick-and-retreat (fzf-style).
    pub fn dialog_backtab(&mut self) {
        if self.dialog_tree_mode() {
            self.dialog_scope_pop();
        } else {
            self.dialog_toggle_check();
            self.dialog_up();
        }
    }

    /// Backspace. Deletes a query character; once the query is empty it instead
    /// pops the folder scope one level (drilling back out), so a single key walks
    /// all the way back to the whole project.
    pub fn dialog_backspace(&mut self) {
        if !self.file_dialog.query.is_empty() {
            crate::editline::backspace(
                &mut self.file_dialog.query,
                &mut self.file_dialog.cursor,
                &mut self.file_dialog.anchor,
            );
            self.dialog_query_changed();
        } else if self.dialog_scope.is_some() {
            self.dialog_scope_pop();
        }
    }

    /// Type a character into the file-dialog search box at the cursor.
    pub fn dialog_input(&mut self, c: char) {
        crate::editline::insert(
            &mut self.file_dialog.query,
            &mut self.file_dialog.cursor,
            &mut self.file_dialog.anchor,
            c,
        );
        self.dialog_query_changed();
    }

    /// Expand or collapse the highlighted tree folder (Enter on a directory).
    fn toggle_tree_selected(&mut self) {
        let i = self.tree.selected;
        match self.tree.entries.get(i) {
            Some(e) if e.is_dir && e.expanded => self.tree.collapse_selected(),
            Some(e) if e.is_dir => self.tree.expand_selected(),
            _ => {}
        }
    }

    /// Limit the dialog to the highlighted folder: re-root the browse tree there
    /// and rebuild the search corpus from its subtree. Scoping to the current
    /// root is a no-op.
    fn dialog_scope_into(&mut self) {
        if let Some((path, true)) = self.tree.selected() {
            let already = self.dialog_scope.as_deref() == Some(path.as_path())
                || (self.dialog_scope.is_none() && self.root.as_deref() == Some(path.as_path()));
            if already {
                return;
            }
            self.dialog_scope = Some(path);
            self.reroot_dialog();
        }
    }

    /// Drill back out one level: scope to the parent folder, or clear the scope
    /// entirely once it reaches (or passes) the project root.
    fn dialog_scope_pop(&mut self) {
        let Some(scope) = self.dialog_scope.clone() else {
            return;
        };
        self.dialog_scope = match (scope.parent(), self.root.as_deref()) {
            (Some(parent), Some(root)) if parent != root && parent.starts_with(root) => {
                Some(parent.to_path_buf())
            }
            _ => None,
        };
        self.reroot_dialog();
    }

    /// Rebuild the browse tree and search corpus for the current scope (or the
    /// project root when unscoped).
    fn reroot_dialog(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let eff = self.dialog_scope.clone().unwrap_or(root);
        self.tree = FileTree::new(&eff);
        self.rebuild_dialog_entries();
        self.dialog_refilter();
    }

    pub fn dialog_tree_expand(&mut self) {
        if self.dialog_tree_mode() {
            self.tree.expand_selected();
        }
    }

    pub fn dialog_tree_collapse(&mut self) {
        if self.dialog_tree_mode() {
            self.tree.collapse_selected();
        }
    }

    pub fn dialog_up(&mut self) {
        if self.dialog_tree_mode() {
            self.tree.move_up();
        } else {
            self.file_dialog.move_up();
        }
    }

    pub fn dialog_down(&mut self) {
        if self.dialog_tree_mode() {
            self.tree.move_down();
        } else {
            self.file_dialog.move_down();
        }
    }

    pub fn dialog_new_file(&mut self) {
        let base = self.dialog_base_dir();
        self.prompt.open(PromptKind::NewFile, base, String::new());
    }

    pub fn dialog_new_folder(&mut self) {
        let base = self.dialog_base_dir();
        self.prompt.open(PromptKind::NewFolder, base, String::new());
    }

    pub fn dialog_rename(&mut self) {
        if let Some((p, _)) = self.dialog_selected() {
            if self.root.as_deref() == Some(p.as_path()) {
                self.status = "Can't rename the root folder".to_string();
                return;
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.prompt.open(PromptKind::Rename, p, name);
        }
    }

    pub fn dialog_delete(&mut self) {
        if let Some((p, _)) = self.dialog_selected() {
            if self.root.as_deref() == Some(p.as_path()) {
                self.status = "Can't delete the root folder".to_string();
                return;
            }
            self.prompt.open(PromptKind::Delete, p, String::new());
        }
    }

    /// Apply the active prompt (Enter), then refresh the tree + dialog.
    pub fn confirm_prompt(&mut self) {
        let Some(kind) = self.prompt.kind() else {
            self.prompt.close();
            return;
        };
        let target = self.prompt.target.clone();
        let input = self.prompt.input.trim().to_string();
        self.prompt.close();

        let mut to_open: Option<PathBuf> = None;
        let result: Result<String, String> = match kind {
            PromptKind::NewFile if input.is_empty() => return,
            PromptKind::NewFile => {
                // Allow a nested path like "src/foo.txt" — create parents.
                let path = target.join(&input);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, b"")
                    .map(|_| {
                        to_open = Some(path);
                        format!("Created {input}")
                    })
                    .map_err(|e| e.to_string())
            }
            PromptKind::NewFolder if input.is_empty() => return,
            PromptKind::NewFolder => std::fs::create_dir_all(target.join(&input))
                .map(|_| format!("Created folder {input}"))
                .map_err(|e| e.to_string()),
            PromptKind::Rename if input.is_empty() => return,
            PromptKind::Rename => {
                let dest = target
                    .parent()
                    .map(|p| p.join(&input))
                    .unwrap_or_else(|| PathBuf::from(&input));
                std::fs::rename(&target, &dest)
                    .map(|_| format!("Renamed to {input}"))
                    .map_err(|e| e.to_string())
            }
            PromptKind::Delete => {
                let r = if target.is_dir() {
                    std::fs::remove_dir_all(&target)
                } else {
                    std::fs::remove_file(&target)
                };
                r.map(|_| "Deleted".to_string()).map_err(|e| e.to_string())
            }
            // These choice prompts are handled by their own answer methods, not
            // through the text-prompt confirm path.
            PromptKind::CloseUnsaved
            | PromptKind::CloseTab
            | PromptKind::QuitUnsaved
            | PromptKind::QuitTerminal
            | PromptKind::ExternalChange => return,
        };

        self.status = match result {
            Ok(msg) => msg,
            Err(e) => format!("Error: {e}"),
        };
        self.tree.refresh();
        if let Some(root) = self.root.clone() {
            self.files_cache = fstree::collect_files(&root, false);
        }
        self.rebuild_dialog_entries();
        match to_open {
            Some(path) => {
                self.dismiss_dialog(Dialog::Files);
                self.open_path(&path);
            }
            None => {
                if self.file_dialog.active {
                    self.dialog_refilter();
                }
            }
        }
    }
}

/// Launch a new Oxru window for `folder` as a detached child process. Returns
/// whether the spawn succeeded.
fn spawn_window(folder: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .arg("--gui")
        .arg(folder)
        .spawn()
        .is_ok()
}

/// The folder name used to label a plain terminal (the project root's basename,
/// or "shell" for a root with no name like "/").
fn folder_name(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "shell".to_string())
}

/// A short tab title for a launched command, preferring the `cd '<dir>'` target's
/// basename, else the first word of the command.
fn short_term_title(cmd: &str) -> String {
    if let Some((_, after)) = cmd.split_once("cd ") {
        let s = after.trim_start_matches(['\'', '"']);
        if let Some(end) = s.find(['\'', '"']) {
            if let Some(name) = s[..end].rsplit('/').next().filter(|n| !n.is_empty()) {
                return name.to_string();
            }
        }
    }
    cmd.split_whitespace().next().unwrap_or("term").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "hello\n").unwrap();
        dir
    }

    #[test]
    fn save_all_writes_every_modified_file() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.open_path(&dir.path().join("src/main.rs"));
        app.active_buffer().unwrap().insert_str("Y");
        assert!(app.editors.iter().all(|b| b.modified));

        app.save_all();
        assert!(app.editors.iter().all(|b| !b.modified), "all saved, none modified");
        assert!(fs::read_to_string(dir.path().join("notes.txt")).unwrap().starts_with('X'));
    }

    #[test]
    fn find_is_case_insensitive_and_selects_current_match() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "Foo bar foo BAR Foo").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        for c in "foo".chars() {
            app.find_input(c);
        }
        assert_eq!(app.find.matches.len(), 3, "Foo / foo / Foo all match");
        // The current match is shown as a selection.
        let cur = app.find.matches[app.find.current];
        assert_eq!(app.editors[0].selection(), Some(cur));

        // Next / prev wrap around the match list.
        app.find_next();
        assert_eq!(app.find.current, 1);
        app.find_prev();
        app.find_prev();
        assert_eq!(app.find.current, 2, "prev from first wraps to last");
    }

    #[test]
    fn new_terminal_opens_beside_current_and_moves() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.new_terminal(); // [t0]                      active 0
        app.new_terminal(); // [t0, t1]                  active 1
        app.new_terminal(); // [t0, t1, t2]              active 2
        assert_eq!(app.terminals.len(), 3);
        assert_eq!(app.active_terminal, 2);

        // From the first tab, a new terminal opens right beside it (index 1).
        app.active_terminal = 0;
        app.new_terminal();
        assert_eq!(app.terminals.len(), 4);
        assert_eq!(app.active_terminal, 1, "inserted beside, not at the end");

        // Move the active terminal left/right, clamped at the ends.
        app.move_terminal_left();
        assert_eq!(app.active_terminal, 0);
        app.move_terminal_left();
        assert_eq!(app.active_terminal, 0, "already leftmost");
        app.move_terminal_right();
        assert_eq!(app.active_terminal, 1);
    }

    #[test]
    fn find_no_results_then_close() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "hello world").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        for c in "zzz".chars() {
            app.find_input(c);
        }
        assert!(app.find.matches.is_empty());
        app.find_close();
        assert!(!app.find.active && app.find.query.is_empty());
    }

    #[test]
    fn close_tab_unmodified_closes_immediately() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.close_tab();
        assert!(app.editors.is_empty());
        assert!(!app.prompt.active, "no prompt for an unmodified file");
    }

    #[test]
    fn close_tab_unsaved_prompts_then_save_or_discard() {
        let dir = workspace();
        let notes = dir.path().join("notes.txt");
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&notes);
        app.active_buffer().unwrap().insert_str("EDIT");

        // Ctrl+W on a modified file prompts and does NOT close yet.
        app.close_tab();
        assert_eq!(app.prompt.kind(), Some(PromptKind::CloseTab));
        assert_eq!(app.editors.len(), 1, "stays open until answered");

        // Save & close writes the file.
        app.close_tab_answer(true);
        assert!(app.editors.is_empty());
        assert!(fs::read_to_string(&notes).unwrap().starts_with("EDIT"), "saved on close");
    }

    #[test]
    fn close_tab_discard_does_not_save() {
        let dir = workspace();
        let notes = dir.path().join("notes.txt");
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&notes);
        app.active_buffer().unwrap().insert_str("X");

        app.close_tab();
        app.close_tab_answer(false); // close without saving
        assert!(app.editors.is_empty());
        assert_eq!(fs::read_to_string(&notes).unwrap(), "hello\n", "changes discarded");
    }

    #[test]
    fn quit_with_no_blockers_exits_immediately() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt")); // opened but unmodified
        app.request_quit();
        assert!(app.should_quit);
    }

    #[test]
    fn quit_prompts_for_unsaved_and_cancel_aborts() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.request_quit();
        assert!(!app.should_quit, "must not quit with unsaved changes");
        assert_eq!(app.prompt.kind(), Some(PromptKind::QuitUnsaved));
        // Cancel keeps everything: no quit, file still modified and untouched.
        app.cancel_quit();
        assert!(!app.should_quit);
        assert!(!app.prompt.active);
        assert!(app.editors[0].modified);
        assert_eq!(fs::read_to_string(dir.path().join("notes.txt")).unwrap(), "hello\n");
    }

    #[test]
    fn quit_save_writes_then_exits() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.request_quit();
        app.quit_save_current();
        assert!(app.should_quit, "quits once the only blocker is resolved");
        assert!(
            fs::read_to_string(dir.path().join("notes.txt")).unwrap().starts_with('X'),
            "the file was saved"
        );
    }

    #[test]
    fn quit_discard_exits_without_saving() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.request_quit();
        app.quit_skip_current(); // discard
        assert!(app.should_quit);
        assert_eq!(
            fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
            "hello\n",
            "discarded changes were not written"
        );
    }

    #[test]
    fn close_all_with_no_unsaved_drops_everything() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.open_path(&dir.path().join("src/main.rs"));
        app.close_all();
        assert!(app.editors.is_empty());
        assert_eq!(app.active_editor, None);
        assert!(!app.prompt.active, "no prompt when nothing is unsaved");
    }

    #[test]
    fn close_all_prompts_per_unsaved_file_then_closes() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let notes = dir.path().join("notes.txt");
        app.open_path(&notes);
        app.active_buffer().unwrap().insert_str("EDIT"); // notes is now modified
        app.open_path(&dir.path().join("src/main.rs")); // unmodified

        // First close-all raises the save prompt for the unsaved file.
        app.close_all();
        assert!(app.prompt.active);
        assert_eq!(app.prompt.kind(), Some(PromptKind::CloseUnsaved));
        assert_eq!(app.editors.len(), 2, "nothing closed until answered");

        // Answer "save": file is written, and all files close.
        app.close_unsaved_answer(true);
        assert!(app.editors.is_empty(), "everything closed after answering");
        assert!(!app.prompt.active);
        assert!(fs::read_to_string(&notes).unwrap().starts_with("EDIT"), "saved on the way out");
    }

    #[test]
    fn close_all_discard_does_not_save() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let notes = dir.path().join("notes.txt");
        app.open_path(&notes);
        app.active_buffer().unwrap().insert_str("DISCARD_ME");

        app.close_all();
        app.close_unsaved_answer(false); // discard
        assert!(app.editors.is_empty());
        assert_eq!(fs::read_to_string(&notes).unwrap(), "hello\n", "discarded, file unchanged");
    }

    #[test]
    fn close_all_handles_multiple_unsaved_sequentially() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let notes = dir.path().join("notes.txt");
        let main = dir.path().join("src/main.rs");
        app.open_path(&notes);
        app.active_buffer().unwrap().insert_str("N");
        app.open_path(&main);
        app.active_buffer().unwrap().insert_str("M");

        // First unsaved file prompts.
        app.close_all();
        assert!(app.prompt.active);
        app.close_unsaved_answer(true); // save first
        // Still a second unsaved file -> another prompt.
        assert!(app.prompt.active, "second unsaved file prompts");
        assert_eq!(app.editors.len(), 1);
        app.close_unsaved_answer(false); // discard second
        assert!(app.editors.is_empty());
        assert!(!app.prompt.active);
        assert!(fs::read_to_string(&notes).unwrap().starts_with('N'), "first was saved");
        assert_eq!(fs::read_to_string(&main).unwrap(), "fn main() {}\n", "second discarded");
    }

    #[test]
    fn term_title_from_cd_command() {
        assert_eq!(
            short_term_title("cd '/a/b/beenapp_backend_2023' && ./run.sh "),
            "beenapp_backend_2023"
        );
        assert_eq!(short_term_title("npm run start:dev"), "npm");
    }

    #[test]
    fn terminal_labels_disambiguate_duplicates() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // spawns terminal 1 (folder = workspace name)
        app.new_terminal();
        app.new_terminal();
        let labels = app.terminal_labels();
        assert_eq!(labels.len(), 3);
        // All three are idle shells in the same folder -> base, "base 2", "base 3".
        assert_eq!(labels[1], format!("{} 2", labels[0]));
        assert_eq!(labels[2], format!("{} 3", labels[0]));
    }

    #[test]
    fn recent_dialog_multiselect() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // Inject a couple of folders directly (avoids touching the real recents).
        app.recent_open = true;
        app.recent_folders = vec![
            dir.path().join("src"),
            dir.path().to_path_buf(),
        ];
        app.recent_cursor = 0;
        app.recent_checked = vec![false; 2];

        // Move + toggle build up a multi-selection.
        app.recent_toggle(); // check row 0
        app.recent_move(1);
        app.recent_toggle(); // check row 1
        assert_eq!(app.recent_checked, vec![true, true]);

        // Cursor wraps.
        app.recent_move(1);
        assert_eq!(app.recent_cursor, 0);
    }

    #[test]
    fn search_includes_gitignored_files() {
        // "Show everywhere, dimmed": gitignored files ARE findable in search (so
        // you can open a .env), just flagged ignored for fading.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("shown.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.rebuild_dialog_entries();

        let pos = |n: &str| app.dialog_entries.iter().position(|(p, _)| p.ends_with(n));
        assert!(pos("shown.rs").is_some());
        let ig = pos("ignored.rs").expect("gitignored file is searchable");
        assert!(app.dialog_ignored[ig], "gitignored file flagged for fading");
        assert!(!app.dialog_ignored[pos("shown.rs").unwrap()]);
    }

    #[test]
    fn junk_dirs_hidden_from_search_until_toggled() {
        // node_modules is browsable in the tree but pruned from search by
        // default; ⌥H (dialog_toggle_junk) flips it back, and again to hide it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("app.rs"), "").unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules/lib.js"), "").unwrap();
        let mut app = App::new(Some(root.to_path_buf())).unwrap();
        app.rebuild_dialog_entries();

        let has = |app: &App, n: &str| app.dialog_entries.iter().any(|(p, _)| p.ends_with(n));
        assert!(has(&app, "app.rs"), "real source is searchable");
        assert!(!has(&app, "node_modules/lib.js"), "node_modules hidden from search by default");

        app.dialog_toggle_junk();
        assert!(app.dialog_show_junk);
        assert!(has(&app, "node_modules/lib.js"), "toggle reveals node_modules in search");
        assert!(has(&app, "app.rs"), "real source still there");

        app.dialog_toggle_junk();
        assert!(!app.dialog_show_junk);
        assert!(!has(&app, "node_modules/lib.js"), "toggle hides it again");

        // The tree, meanwhile, always lists node_modules (browsable like VSCode).
        let tree = crate::fstree::FileTree::new(root);
        assert!(
            tree.entries.iter().any(|e| e.name == "node_modules"),
            "node_modules is shown in the tree regardless of the search toggle"
        );
    }

    #[test]
    fn settings_dialog_has_font_and_colour_sections() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_settings();
        assert_eq!(app.settings_focus, 0); // font
        app.settings_toggle_focus();
        assert_eq!(app.settings_focus, 1); // colour
        app.settings_toggle_focus();
        assert_eq!(app.settings_focus, 0, "wraps back to font (only two sections)");
    }

    #[test]
    fn settings_adjusts_font_and_accent() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_settings();
        assert!(app.settings_open);
        assert_eq!(app.settings_focus, 0); // font size first

        let before = app.gui_font_size;
        app.settings_adjust(1);
        assert_eq!(app.gui_font_size, before + 1);
        app.settings_adjust(-1);
        assert_eq!(app.gui_font_size, before);

        // Switch to the colour section and pick the next colour.
        app.settings_toggle_focus();
        assert_eq!(app.settings_focus, 1);
        let c0 = app.settings_color;
        app.settings_adjust(1);
        assert_ne!(app.settings_color, c0);
        // The live theme accent now matches the picked palette entry.
        assert_eq!(app.theme.accent_index(), Some(app.settings_color));

        // Settings is the focused dialog in the stack (closing it persists to the
        // real config, so we don't exercise that side-effecting path here).
        assert_eq!(app.top_dialog(), Some(Dialog::Settings));
    }

    #[test]
    fn folder_name_uses_basename() {
        assert_eq!(folder_name(Path::new("/a/b/myproj")), "myproj");
        assert_eq!(folder_name(Path::new("/")), "shell");
    }

    #[test]
    fn dialog_stack_raises_dedups_and_pops() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        // Open two dialogs — they stack, newest on top.
        app.open_file_dialog();
        assert_eq!(app.top_dialog(), Some(Dialog::Files));
        app.open_recent_dialog();
        assert_eq!(app.top_dialog(), Some(Dialog::Recent));
        assert_eq!(app.dialogs.len(), 2);

        // Re-opening Files (already open) raises it without duplicating.
        app.open_file_dialog();
        assert_eq!(app.top_dialog(), Some(Dialog::Files));
        assert_eq!(app.dialogs, vec![Dialog::Recent, Dialog::Files]);

        // Closing the top reveals the one beneath, then the editor.
        app.close_top_dialog();
        assert_eq!(app.top_dialog(), Some(Dialog::Recent));
        app.close_top_dialog();
        assert_eq!(app.top_dialog(), None);
        assert!(!app.file_dialog.active && !app.recent_open);
    }

    #[test]
    fn no_folder_state() {
        // Launched with no folder: welcome state, empty tree, file dialog is a
        // no-op until a folder is opened.
        let mut app = App::new(None).unwrap();
        assert!(!app.has_folder());
        assert!(app.root.is_none());
        assert!(app.tree.entries.is_empty());
        assert!(app.files_cache.is_empty());
        app.open_file_dialog();
        assert!(!app.file_dialog.active, "file dialog stays closed without a folder");

        // A folder-backed app has the normal populated state.
        let dir = workspace();
        let app2 = App::new(Some(dir.path().to_path_buf())).unwrap();
        assert!(app2.has_folder());
        assert!(!app2.tree.entries.is_empty());
    }

    #[test]
    fn opens_and_dedups_tabs() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let p = dir.path().join("notes.txt");
        app.open_path(&p);
        app.open_path(&p);
        assert_eq!(app.editors.len(), 1);
        assert_eq!(app.active_editor, Some(0));
    }

    #[test]
    fn tabs_open_navigate_move_close() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.open_path(&dir.path().join("src/main.rs"));
        assert_eq!(app.editors.len(), 2);
        assert_eq!(app.active_editor, Some(1));

        app.next_tab();
        assert_eq!(app.active_editor, Some(0)); // wrapped 1 -> 0
        app.prev_tab();
        assert_eq!(app.active_editor, Some(1)); // wrapped 0 -> 1

        // Move the active tab (main.rs at index 1) left.
        app.move_tab_left();
        assert_eq!(app.active_editor, Some(0));
        assert!(app.editors[0].name().contains("main.rs"));

        app.close_tab();
        assert_eq!(app.editors.len(), 1);
        assert_eq!(app.active_editor, Some(0));
    }

    #[test]
    fn click_switches_tabs_and_positions_cursor() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt")); // tab 0, name len 9
        app.open_path(&dir.path().join("src/main.rs")); // tab 1, name len 7
        assert_eq!(app.active_editor, Some(1));

        // Tab 0 label occupies cols 0..11 (" notes.txt "), tab 1 starts at col 12.
        app.editor_mouse_down(4, 0);
        assert_eq!(app.active_editor, Some(0), "clicking tab 0 selects it");
        app.editor_mouse_down(15, 0);
        assert_eq!(app.active_editor, Some(1), "clicking tab 1 selects it");

        // Click into the body: row 2 (-> editor view row 1 -> line 1), past the
        // gutter, lands the cursor on that line.
        app.editor_mouse_down(20, 2);
        app.editor_mouse_up();
        let b = &app.editors[1];
        assert_eq!(b.cursor_row(), 1, "cursor moved to the clicked line");
        assert!(b.selection().is_none(), "a plain click leaves no selection");
    }

    #[test]
    fn mouse_drag_selects_text() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs")); // "fn main() {}\n"

        // Press at the start of line 0, drag a few columns right.
        app.editor_mouse_down(4, 1); // gutter is ~4 cols, so col 0 of the text
        app.editor_mouse_drag(8, 1);
        app.editor_mouse_up();
        let b = &app.editors[0];
        let sel = b.selected_text().expect("drag created a selection");
        assert!(!sel.is_empty(), "selection has text, got {sel:?}");
    }

    // ---- terminal mouse: the unified dispatch shared by GUI and TUI ----

    /// App with one terminal open + focused and a known body rect, so mouse
    /// coords map to terminal cells without a real render pass.
    fn terminal_mouse_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        app.terminal_view = Some((0, 0, 80, 24)); // x, y, w, h
        (dir, app)
    }

    /// Pump the active terminal until `pred` holds or a timeout elapses.
    fn pump_until(app: &mut App, pred: impl Fn(&crate::terminalpane::TerminalPane) -> bool) -> bool {
        let i = app.active_terminal;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            app.terminals[i].pump();
            if pred(&app.terminals[i]) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        false
    }

    fn screen_has(t: &crate::terminalpane::TerminalPane, needle: &str) -> bool {
        let s: String = (0..24)
            .flat_map(|r| (0..80).map(move |c| (r, c)))
            .filter_map(|(r, c)| t.screen().cell(r, c).map(|x| x.contents().to_string()))
            .collect();
        s.contains(needle)
    }

    #[test]
    fn wheel_scrolls_local_scrollback_at_shell() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        // `printf 'L%s\n' …` so the marker "L300" appears only in the OUTPUT, not
        // in the echoed command line (which would race the wait).
        app.terminals[i].send_input(b"printf 'L%s\\n' $(seq 1 300)\n");
        assert!(pump_until(&mut app, |t| screen_has(t, "L300")), "printf output");
        let _ = app.terminals[i].take_sent();
        // No mouse mode, normal screen -> wheel scrolls oxru's own scrollback.
        app.mouse_wheel(3, 10, 5, false);
        assert!(
            app.terminals[i].scroll_offset() > 0,
            "wheel should scroll scrollback up"
        );
        assert!(
            app.terminals[i].take_sent().is_empty(),
            "scrollback wheel must not reach the shell"
        );
    }

    #[test]
    fn wheel_sends_arrows_on_alt_screen_pager() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        // Enter the alternate screen WITHOUT mouse tracking (like less/man).
        app.terminals[i].send_input(b"printf '\\033[?1049h'\n");
        assert!(pump_until(&mut app, |t| t.on_alternate_screen()), "alt screen");
        let _ = app.terminals[i].take_sent();
        // Wheel up twice -> two Up-arrow presses (alternate scroll).
        app.mouse_wheel(2, 0, 0, false);
        assert_eq!(
            app.terminals[i].take_sent(),
            vec![0x1b, b'[', b'A', 0x1b, b'[', b'A']
        );
    }

    #[test]
    fn wheel_forwards_to_program_in_mouse_mode() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        app.terminals[i].send_input(b"printf '\\033[?1000h\\033[?1006h'\n");
        assert!(pump_until(&mut app, |t| t.wants_mouse()), "mouse mode");
        let _ = app.terminals[i].take_sent();
        // Wheel up at col 10/row 5 -> SGR button 64 at 1-based (11, 6).
        app.mouse_wheel(1, 10, 5, false);
        let sent = String::from_utf8_lossy(&app.terminals[i].take_sent()).into_owned();
        assert!(
            sent.contains("\u{1b}[<64;11;6M"),
            "wheel-up should forward SGR button 64, got {sent:?}"
        );
    }

    #[test]
    fn click_forwards_in_mouse_mode_but_shift_selects_locally() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        app.terminals[i].send_input(b"printf '\\033[?1000h\\033[?1006h'\n");
        assert!(pump_until(&mut app, |t| t.wants_mouse()), "mouse mode");
        let _ = app.terminals[i].take_sent();

        // Plain click -> forwarded to the program as an SGR press at (4,3).
        app.mouse_down(3, 2, false);
        let press = String::from_utf8_lossy(&app.terminals[i].take_sent()).into_owned();
        assert!(
            press.contains("\u{1b}[<0;4;3M"),
            "plain click forwards SGR press, got {press:?}"
        );
        app.mouse_up(3, 2);
        let _ = app.terminals[i].take_sent();

        // Shift+click -> local selection; nothing reaches the program.
        app.mouse_down(3, 2, true);
        assert!(
            app.terminals[i].take_sent().is_empty(),
            "shift+click must select locally, not reach the program"
        );
    }

    #[test]
    fn drag_selects_terminal_text_without_mouse_mode() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        // Marker appears only in output (not the echoed command), so the wait is
        // reliable and the text is actually on screen before we select it.
        app.terminals[i].send_input(b"printf 'PICKME-%s\\n' TARGET\n");
        assert!(
            pump_until(&mut app, |t| screen_has(t, "PICKME-TARGET")),
            "printf output"
        );
        // Drag a wide, multi-row rectangle so it definitely covers the output
        // line regardless of how tall the shell prompt is.
        app.mouse_down(0, 0, false);
        app.mouse_drag(79, 6);
        app.mouse_up(79, 6);
        let txt = app.terminals[i].selection_text().unwrap_or_default();
        assert!(
            txt.contains("PICKME-TARGET"),
            "drag should select the visible output, got {txt:?}"
        );
    }

    // ---- external file changes ----------------------------------------

    fn open_one(content: &str) -> (tempfile::TempDir, std::path::PathBuf, App) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        std::fs::write(&p, content).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&p);
        (dir, p, app)
    }

    #[test]
    fn external_change_reloads_clean_buffer() {
        let (_d, p, mut app) = open_one("original\n");
        assert!(!app.editors[0].modified);
        // A command rewrites the file (different length so it's detected even
        // within the same mtime-second).
        std::fs::write(&p, "rewritten by a command\n").unwrap();
        app.recheck_files_soon();
        app.poll_file_changes();
        assert_eq!(app.editors[0].rope.to_string(), "rewritten by a command\n");
        assert!(!app.editors[0].modified, "auto-reloaded buffer stays clean");
        assert!(!app.prompt.active, "no prompt for a clean buffer");
    }

    #[test]
    fn external_change_with_unsaved_edits_prompts_and_keeps() {
        let (_d, p, mut app) = open_one("original\n");
        app.active_buffer().unwrap().insert_str("MY EDIT\n");
        assert!(app.editors[0].modified);
        std::fs::write(&p, "different on disk\n").unwrap();
        app.recheck_files_soon();
        app.poll_file_changes();
        // Conflict -> prompt; the buffer is NOT silently overwritten.
        assert_eq!(
            app.prompt.kind(),
            Some(crate::prompt::PromptKind::ExternalChange)
        );
        assert!(app.editors[0].rope.to_string().contains("MY EDIT"));
        // "Keep mine" closes the prompt and preserves edits.
        app.external_change_answer(false);
        assert!(!app.prompt.active);
        assert!(app.editors[0].rope.to_string().contains("MY EDIT"));
        // And we don't re-prompt for the same change.
        app.recheck_files_soon();
        app.poll_file_changes();
        assert!(!app.prompt.active, "no repeat prompt after keep-mine");
    }

    #[test]
    fn external_change_reload_answer_takes_disk_version() {
        let (_d, p, mut app) = open_one("original\n");
        app.active_buffer().unwrap().insert_str("MY EDIT\n");
        std::fs::write(&p, "DISK VERSION\n").unwrap();
        app.recheck_files_soon();
        app.poll_file_changes();
        app.external_change_answer(true); // reload, discard edits
        assert_eq!(app.editors[0].rope.to_string(), "DISK VERSION\n");
        assert!(!app.editors[0].modified);
    }

    #[test]
    fn external_delete_flags_buffer_modified() {
        let (_d, p, mut app) = open_one("keep me\n");
        std::fs::remove_file(&p).unwrap();
        app.recheck_files_soon();
        app.poll_file_changes();
        assert!(app.editors[0].modified, "deleted file -> dirty so save restores");
        assert!(app.editors[0].rope.to_string().contains("keep me"));
        // We reported it once; a second poll must not panic or re-flag endlessly.
        app.recheck_files_soon();
        app.poll_file_changes();
    }

    #[test]
    fn own_save_is_not_seen_as_external_change() {
        let (_d, _p, mut app) = open_one("original\n");
        app.active_buffer().unwrap().insert_str("mine\n");
        app.save_active();
        assert!(!app.editors[0].modified);
        app.recheck_files_soon();
        app.poll_file_changes();
        // Our own write must not trigger a reload or a conflict prompt.
        assert!(!app.prompt.active, "own save isn't an external change");
        assert!(app.editors[0].rope.to_string().contains("mine"));
    }

    #[test]
    fn save_clears_modified() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let p = dir.path().join("notes.txt");
        app.open_path(&p);
        app.active_buffer().unwrap().insert_str("Z");
        assert!(app.editors[0].modified);
        app.save_active();
        assert!(!app.editors[0].modified);
        assert!(fs::read_to_string(&p).unwrap().starts_with('Z'));
    }

    #[test]
    fn highlight_cache_recomputes_on_edit() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let lines1 = app.highlighted_for(0).len();
        app.active_buffer().unwrap().insert_str("\n// more\n");
        let lines2 = app.highlighted_for(0).len();
        assert!(lines2 > lines1);
    }

    #[test]
    fn dialog_scopes_search_into_a_folder() {
        let dir = workspace(); // notes.txt + src/main.rs
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        assert!(app.dialog_tree_mode(), "opens in browse-tree mode");

        // Browse to `src` (folders sort first) and scope the search into it (Tab).
        app.dialog_down();
        app.dialog_tab();
        assert_eq!(app.dialog_scope.as_deref(), Some(dir.path().join("src").as_path()));

        // A file outside the scope is no longer findable; one inside is.
        app.file_dialog.query = "notes".into();
        app.dialog_refilter();
        assert!(app.file_dialog.matches.is_empty(), "out-of-scope file excluded");
        app.file_dialog.query = "main".into();
        app.dialog_refilter();
        assert!(!app.file_dialog.matches.is_empty(), "in-scope file found");

        // Clearing the query, then one more Backspace, drills back out.
        app.file_dialog.query.clear();
        app.dialog_refilter();
        app.dialog_backspace(); // empty query + scope -> pop the scope
        assert!(app.dialog_scope.is_none(), "drilled back out to the whole project");
        app.file_dialog.query = "notes".into();
        app.dialog_refilter();
        assert!(!app.file_dialog.matches.is_empty(), "out-of-scope file findable again");
    }

    #[test]
    fn dialog_shift_tab_drills_out_of_folder() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        app.dialog_down(); // select `src`
        app.dialog_tab(); // Tab: scope into it
        assert!(app.dialog_scope.is_some(), "scoped into the folder");
        app.dialog_backtab(); // Shift+Tab: the mirror — drill back out
        assert!(app.dialog_scope.is_none(), "Shift+Tab leaves the folder");
        // A second Shift+Tab at the top level is a harmless no-op.
        app.dialog_backtab();
        assert!(app.dialog_scope.is_none());
    }

    #[test]
    fn opening_a_file_dismisses_open_dialogs() {
        // In the terminal, opening a file should focus the file — the terminal
        // (and any picker) gets out of the way.
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        assert_eq!(app.top_dialog(), Some(Dialog::Terminal));
        app.open_path(&dir.path().join("src/main.rs"));
        assert_eq!(app.top_dialog(), None, "opening a file closes the dialogs");
        assert_eq!(app.editors.len(), 1);
        // A binary file, by contrast, leaves the dialog up.
        std::fs::write(dir.path().join("p.png"), [0u8, 1, 2]).unwrap();
        app.open_file_dialog();
        app.open_path(&dir.path().join("p.png"));
        assert_eq!(app.top_dialog(), Some(Dialog::Files), "binary leaves the picker open");
    }

    #[test]
    fn terminal_requires_an_open_folder() {
        // With no folder open, ⌥T must behave like ⌥F: refuse and hint instead of
        // spawning a terminal with no working directory.
        let mut app = App::new(None).unwrap();
        app.toggle_terminal_modal();
        assert!(app.terminals.is_empty(), "no terminal spawned without a folder");
        assert_eq!(app.top_dialog(), None, "terminal dialog not opened");

        // With a folder it works as before.
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        assert_eq!(app.top_dialog(), Some(Dialog::Terminal));
    }

    #[test]
    fn binary_files_are_not_opened_as_text() {
        let dir = workspace();
        std::fs::write(dir.path().join("pic.png"), [0u8, 1, 2, 3, 255]).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("pic.png"));
        assert!(app.editors.is_empty(), "a binary file should not open in an editor");
        // A real text file still opens.
        app.open_path(&dir.path().join("src/main.rs"));
        assert_eq!(app.editors.len(), 1);
    }

    #[test]
    fn dialog_new_file_creates_at_root() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog(); // root node selected
        app.dialog_new_file();
        app.prompt.input = "fresh.txt".to_string();
        app.confirm_prompt();
        assert!(dir.path().join("fresh.txt").exists());
    }

    #[test]
    fn search_boosts_recently_opened_file() {
        let dir = workspace(); // has notes.txt, src/main.rs
        fs::write(dir.path().join("mainframe.rs"), "").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // Open src/main.rs — records it as most-recently-used.
        app.open_path(&dir.path().join("src/main.rs"));

        app.open_file_dialog();
        app.file_dialog.query = "main".to_string();
        app.dialog_refilter();
        let top = app.dialog_entries[app.file_dialog.matches[0]].0.clone();
        assert!(
            top.ends_with("src/main.rs"),
            "the recently-opened file tops the search, got {top:?}"
        );
    }

    #[test]
    fn reopen_closed_tab_restores_it() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        app.open_path(&dir.path().join("notes.txt")); // active
        assert_eq!(app.editors.len(), 2);
        app.close_tab(); // closes notes.txt
        assert_eq!(app.editors.len(), 1);
        app.reopen_closed_tab();
        assert_eq!(app.editors.len(), 2);
        assert!(app.editors.iter().any(|b| b.name() == "notes.txt"));
    }

    #[test]
    fn editor_scroll_moves_view_not_cursor() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        for _ in 0..50 {
            app.active_buffer().unwrap().insert_str("x\n");
        }
        let cursor = app.editors[0].cursor;
        app.editor_height = 10;
        app.editor_scroll(5);
        assert_eq!(app.editors[0].scroll_row, 5);
        assert_eq!(app.editors[0].cursor, cursor, "wheel scroll leaves the cursor put");
    }

    #[test]
    fn copy_active_path_absolute_and_relative() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let file = dir.path().join("src/main.rs");
        app.open_path(&file);

        app.copy_active_path(false);
        assert_eq!(app.clipboard_text, file.to_string_lossy());
        app.copy_active_path(true);
        assert_eq!(app.clipboard_text, "src/main.rs");
    }

    #[test]
    fn file_dialog_opens_multiple_ticked_files() {
        let dir = workspace(); // notes.txt + src/main.rs
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();

        // Tick notes.txt, then (across a query change) src/main.rs.
        app.file_dialog.query = "notes".to_string();
        app.dialog_refilter();
        app.dialog_toggle_check();
        app.file_dialog.query = "main".to_string();
        app.dialog_refilter();
        app.dialog_toggle_check();
        assert_eq!(app.file_dialog.checked.len(), 2, "ticks persist across query changes");

        app.dialog_open_selected();
        assert_eq!(app.editors.len(), 2, "both ticked files opened");
        assert!(!app.file_dialog.active, "dialog closed after multi-open");
    }

    #[test]
    fn copy_dialog_path_uses_the_selection() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        app.file_dialog.query = "main".to_string();
        app.dialog_refilter();

        app.copy_dialog_path(true);
        assert_eq!(app.clipboard_text, "src/main.rs");
        app.copy_dialog_path(false);
        assert_eq!(app.clipboard_text, dir.path().join("src/main.rs").to_string_lossy());
    }

    #[test]
    fn dialog_rename_then_delete() {
        let dir = workspace();
        fs::write(dir.path().join("old.txt"), "hi").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        // Select old.txt by searching for it (flat list, top result).
        app.file_dialog.query = "old.txt".to_string();
        app.dialog_refilter();
        app.dialog_rename();
        app.prompt.input = "new.txt".to_string();
        app.confirm_prompt();
        assert!(dir.path().join("new.txt").exists());
        assert!(!dir.path().join("old.txt").exists());

        app.file_dialog.query = "new.txt".to_string();
        app.dialog_refilter();
        app.dialog_delete();
        app.confirm_prompt();
        assert!(!dir.path().join("new.txt").exists());
    }
}
