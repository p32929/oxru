//! Central application state.
//!
//! The current app is deliberately small: a set of open editor buffers and a
//! file dialog (the single entry point to the filesystem). Everything is driven
//! through a handful of methods called from [`crate::input`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::style::Color;

use crate::buffer::{Buffer, DiskStatus};
use crate::config::Config;
use crate::filedialog::FileDialog;
use crate::fstree::{self, FileTree};
use crate::icons::Icons;
use crate::prompt::{Prompt, PromptKind};
use crate::search::{self, ProjectSearch};
use crate::syntax::{self, Span};
use crate::termbridge;
use crate::terminalpane::TerminalPane;
use crate::theme::Theme;

/// How long a toast stays fully on screen before it disappears.
const TOAST_TTL: Duration = Duration::from_millis(2200);

/// Hard ceiling on simultaneously open embedded terminals — see `new_terminal`.
const MAX_TERMINALS: usize = 20;

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
    /// A quick-switch palette (⌘K) stacked on top of `Terminal`: type to
    /// fuzzy-filter open terminals by name and jump straight to one, instead
    /// of stepping through them one at a time with Ctrl+Tab.
    TerminalPicker,
    Help,
    /// Project-wide "Search in Files" (⌘⇧F) — search a query across every
    /// file in the project, VSCode's Search view.
    SearchFiles,
}

/// State for the terminal quick-switcher (⌘K while the terminal dialog is
/// focused). Deliberately simpler than [`crate::filedialog::FileDialog`]'s
/// query field (append/backspace only, no cursor navigation) — with at most
/// [`MAX_TERMINALS`] short labels to filter, there's no need for the full
/// editable-line machinery a project-wide file search needs.
#[derive(Default)]
pub struct TerminalPicker {
    pub query: String,
    /// Indices into `App::terminals`, best match first — every terminal (tab
    /// order) when the query is empty.
    pub matches: Vec<usize>,
    pub selected: usize,
}

/// A clickable region of a tab strip (editor or terminal) — clicking it
/// selects (and arms a possible drag-reorder of) tab `tab`. Tabs can only be
/// closed with a keyboard shortcut, not the mouse, so this is just a select
/// target, not an action enum. In absolute screen coordinates. Rebuilt every
/// frame by `ui::render_tabs`/`render_terminal_modal`; mouse handlers only
/// ever consult these, never recompute tab layout — the same pattern as
/// `editor_panes` and `gui_carets`. `row` is the strip's *row-major grid
/// row*, not a screen row — tabs wrap onto as many rows as they need (see
/// `ui::wrapping_tab_grid`), so a hit is only a match when both the row and
/// the column line up.
#[derive(Clone, Copy)]
pub struct TabHit {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub tab: usize,
}

/// The tab index under `(row, col)`, if any.
pub(crate) fn tab_hit_at(hits: &[TabHit], row: u16, col: u16) -> Option<usize> {
    hits.iter()
        .find(|h| h.row == row && col >= h.col_start && col < h.col_end)
        .map(|h| h.tab)
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
    /// Whether the replace field is showing (Ctrl+H toggles it).
    pub replace_active: bool,
    pub replace: String,
    /// Cursor/anchor within `replace` (char indices), same scheme as `query`.
    pub replace_cursor: usize,
    pub replace_anchor: Option<usize>,
    /// Whether keyboard input targets the replace field rather than the query
    /// (Tab swaps focus while the replace field is showing).
    pub replace_focus: bool,
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

/// One caret's cell position in GUI mode (see `App::gui_carets`).
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub struct GuiCaret {
    pub col: u16,
    pub row: u16,
    pub color: Color,
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
    /// Project-wide "Search in Files" (⌘⇧F): a case-insensitive search across
    /// every project file, grouped by file with a line preview per match.
    pub project_search: ProjectSearch,

    // Embedded terminals, shown in a full-screen modal as tabs (or a grid).
    pub terminals: Vec<TerminalPane>,
    pub active_terminal: usize,
    pub terminal_modal: bool,
    pub terminal_grid: bool,
    /// Wakes the GUI event loop when terminal output arrives (set by the GUI).
    terminal_waker: Option<crate::terminalpane::Waker>,
    /// The most recently used terminal cwd this session (live-queried from
    /// the active terminal's shell when available, else remembered from the
    /// last one spawned) — new terminals start here instead of always
    /// resetting to the project root.
    last_terminal_cwd: Option<PathBuf>,
    /// Bytes already consumed from the terminal-bridge request file.
    request_offset: u64,
    /// The active terminal's body rect in global cell coords (x, y, w, h),
    /// recorded during render so mouse events can map to terminal cells.
    pub terminal_view: Option<(u16, u16, u16, u16)>,
    /// This frame's terminal tab-strip hit regions, and the strip's rect
    /// (`None` when the terminal dialog is buried) — recorded by
    /// `ui::render_terminal_modal`, consulted by the mouse handlers.
    pub terminal_tab_hits: Vec<TabHit>,
    pub terminal_tabstrip_rect: Option<(u16, u16, u16, u16)>,
    /// The terminal tab being drag-reordered, if a drag is in progress.
    pub terminal_tab_drag: Option<usize>,
    /// This frame's grid-mode cell rects, index-aligned with `terminals`
    /// (empty outside grid mode) — recorded by `ui::render_terminal_modal` so
    /// clicking a non-active cell can switch focus to it, the same pattern as
    /// `terminal_tab_hits`.
    pub terminal_grid_rects: Vec<(u16, u16, u16, u16)>,
    /// The terminal quick-switcher (⌘K) state.
    pub terminal_picker: TerminalPicker,
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
    /// This process's own resident-set size in KiB, refreshed by `poll_memory`
    /// — shown in the footer so a runaway (e.g. too many terminals, a huge
    /// scrollback) is visible from inside the window that's causing it,
    /// rather than only discoverable in Activity Monitor after the fact.
    /// `None` until the first check completes.
    pub mem_rss_kb: Option<u64>,
    /// Throttle for `poll_memory` — sampling actual RSS every frame would be
    /// silly (it barely moves frame to frame and reading it isn't free).
    mem_last_check: Instant,
    /// The open folder's current git branch and whether the working tree has
    /// uncommitted changes, or `None` if it isn't a git repo (or `git` isn't
    /// installed) — shown, color-coded, in the status bar.
    pub git_branch: Option<(String, bool)>,
    /// Throttle for `poll_git`, same reasoning as `mem_last_check`.
    git_last_check: Instant,
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
    /// Bumped every time `dialog_display` is rebuilt — lets
    /// `FileDialog::refilter` tell "same project file list, query changed"
    /// (reuse its prepared-candidate cache) apart from "the entry list itself
    /// changed" (rebuild it).
    dialog_entries_rev: u64,
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
    /// The Files dialog's results-list rect (x, y, w, h) and the index of its
    /// first visible row — recorded by `ui::render_file_dialog` every frame so
    /// clicks/hover can resolve a screen row to a tree/match index without
    /// recomputing the scroll math.
    pub dialog_list_rect: Option<(u16, u16, u16, u16)>,
    pub dialog_list_start: usize,
    /// Row index (tree entry, or match visible-index) under the pointer, for
    /// the file-picker hover highlight.
    pub dialog_hover: Option<usize>,
    /// The tree row (by index) a mouse-down armed as a possible drag-move, if
    /// any. Cleared on release; if the release lands on a different row, the
    /// entry is moved there instead of opened.
    pub tree_drag: Option<usize>,
    /// This frame's breadcrumb segment hit regions: (col start, col end,
    /// folder path) — recorded by `ui::render_breadcrumb`, consulted by the
    /// mouse handlers. Only populated in single-file view (see
    /// [`App::breadcrumb_row_shown`]).
    pub breadcrumb_hits: Vec<(u16, u16, PathBuf)>,
    /// First visible row of the shortcuts cheat-sheet (F1), for scrolling.
    pub help_scroll: usize,

    // Look & feel.
    pub theme: Theme,
    pub icons: Icons,
    /// Whether `icons = "..."` was set explicitly in config. When it wasn't, the
    /// terminal entry point downgrades to the font-independent Unicode set (a
    /// host terminal can't be guaranteed to have a Nerd Font — unlike the GUI,
    /// which ships its own). See [`App::ensure_terminal_icons`].
    icons_explicit: bool,
    /// Font size for windowed mode (logical points); read by the GUI backend.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub gui_font_size: u32,
    /// Windowed-mode terminal repaint rate (frames/sec) — one of
    /// `config::FPS_OPTIONS`. Only throttles *unattended* terminal-output-
    /// driven repaints (see `terminal_repaint_due`); typing and other real
    /// interaction always redraws immediately regardless.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub gui_fps: u32,
    /// Dialog/terminal-modal size as a percent of the screen (80-99, default
    /// 99) — replaces a fixed 90% for every dialog. Used by both backends:
    /// every dialog (Files, Recent, Settings, Help, Terminal) renders
    /// through the same ratatui layout in the TUI and the GUI.
    pub dialog_size_pct: u32,
    /// Last time a terminal-output-driven repaint actually happened, for
    /// `terminal_repaint_due`'s throttle.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    last_terminal_repaint: Instant,
    /// Measured composited-frames-per-second, sampled roughly once a second
    /// by the GUI loop. Temporary debug readout (shown in the footer) for
    /// verifying the configured `gui_fps` is actually being honored.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub measured_fps: Option<f64>,
    /// Whether we're running in the windowed (GUI) backend. The terminal backend
    /// draws the editor caret with the host terminal's own hardware cursor (a real
    /// thin blinking bar that never covers the glyph); the GUI's wgpu backend has
    /// no native cursor, so it draws its own overlay instead — see `gui_carets`.
    pub gui: bool,
    /// This frame's editor caret cell positions, in GUI mode only — cleared and
    /// repopulated by `ui::render` on every frame. The terminal build never
    /// touches this; it uses the host's hardware cursor instead. The GUI reads
    /// this after drawing and feeds it to a `PostProcessor` that paints each
    /// caret as a GPU overlay *on top of* the already-rendered text, rather than
    /// overwriting a cell's glyph the way a hand-drawn bar character would (which
    /// hides whatever character was in that cell).
    pub gui_carets: Vec<GuiCaret>,

    /// Cached syntax highlight: (editor index, revision, lines).
    hl_cache: std::collections::HashMap<u64, (u64, Vec<Vec<Span>>)>,
    /// (buffer id, revision) pairs with a background highlight job already in
    /// flight, so a stale cache doesn't spawn a duplicate job every frame while
    /// waiting for the first one to land.
    hl_pending: HashSet<(u64, u64)>,
    /// Plain (unstyled) fallback lines shown for whatever's on screen *right
    /// now* while its real highlight is being computed off-thread — always
    /// text-accurate, just uncoloured for a frame or two. See `highlighted_for`.
    hl_fallback: Vec<Vec<Span>>,
    /// Send half of the background-highlight-job channel — cloned into each
    /// spawned job; results land on `hl_rx`.
    hl_tx: Sender<(u64, u64, Vec<Vec<Span>>)>,
    /// Completed background highlight jobs, drained at the top of
    /// `highlighted_for`. A result is only adopted if its revision still
    /// matches the buffer's *current* revision — anything superseded by a
    /// later edit is simply discarded.
    hl_rx: Receiver<(u64, u64, Vec<Vec<Span>>)>,
    /// Lowercased haystack for `recompute_find`, cached by (buffer id,
    /// revision) so typing further into the find query doesn't re-collect
    /// the whole file's characters on every keystroke.
    find_hay_cache: Option<(u64, u64, Vec<char>)>,
    /// Next id to hand to a newly-opened buffer.
    next_buffer_id: u64,
    /// Tile all open files in a grid (split view), like the terminal grid.
    pub editor_grid: bool,
    /// In grid view, each pane's content rect (editor index, (x,y,w,h)) in global
    /// cells — recorded at render time so mouse clicks can map to a pane.
    pub editor_panes: Vec<(usize, (u16, u16, u16, u16))>,
    /// This frame's scrollbar thumb for each pane whose content overflows its
    /// viewport: (pane index, thumb top row, thumb height), in absolute
    /// screen rows — recorded by `ui::render_scrollbar_col`. The track itself
    /// is just the column one past that pane's `editor_panes` rect, so no
    /// separate rect is stored for it.
    pub scrollbar_thumbs: Vec<(usize, u16, u16)>,
    /// The pane being scrollbar-dragged, and the row offset within the thumb
    /// where the drag grabbed it (so the thumb doesn't jump under the cursor).
    pub scrollbar_drag: Option<(usize, u16)>,
    /// This frame's editor tab-strip hit regions — recorded by
    /// `ui::render_tabs`, consulted by the mouse handlers.
    pub tab_hits: Vec<TabHit>,
    /// The editor tab strip's rect (x, y, w, h) this frame — it now wraps
    /// onto as many rows as the open tabs need, so mouse handlers need this
    /// to tell "inside the strip" from "inside the editor body" and to turn
    /// a screen row into the strip-relative grid row `TabHit` uses.
    pub tab_strip_rect: Option<(u16, u16, u16, u16)>,
    /// The editor tab being drag-reordered, if a drag is in progress.
    pub tab_drag: Option<usize>,
    /// Last rendered editor viewport height, for scroll math.
    pub editor_height: u16,
    /// (editor index, cursor) at the last render — to detect cursor movement so
    /// wheel-scroll can detach the view from the cursor.
    last_cursor: Option<(usize, usize)>,
    /// Buffer id + timestamp of the most recently opened *new* tab. Wheel deltas
    /// targeting that exact buffer are ignored for a brief window after opening
    /// (see `editor_scroll`) — trailing trackpad momentum from scrolling a list
    /// (e.g. the Files dialog) right before opening a file otherwise lands on
    /// the newly active buffer and drags it away from row 0.
    just_opened: Option<(u64, Instant)>,

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

    /// Where [`Self::persist_settings`] writes — the real global config path
    /// in production; overridden in tests so they never touch the user's
    /// actual config file.
    config_path: Option<PathBuf>,
}

impl App {
    pub fn new(root: Option<PathBuf>) -> Result<Self> {
        let config = match &root {
            Some(r) => Config::load(r),
            None => Config::load_global(),
        };
        let theme = config.theme();
        let icons = config.icons();
        let icons_explicit = config.icons.is_some();

        let (tree, files_cache) = match &root {
            Some(r) => (FileTree::new(r), fstree::collect_files(r, false)),
            None => (FileTree::empty(), Vec::new()),
        };

        let (hl_tx, hl_rx) = std::sync::mpsc::channel();

        Ok(App {
            root,
            dialogs: Vec::new(),
            tree,
            editors: Vec::new(),
            active_editor: None,
            find: Find::default(),
            project_search: ProjectSearch::default(),
            terminals: Vec::new(),
            active_terminal: 0,
            terminal_modal: false,
            terminal_grid: false,
            terminal_waker: None,
            last_terminal_cwd: None,
            request_offset: 0,
            terminal_view: None,
            terminal_tab_hits: Vec::new(),
            terminal_tabstrip_rect: None,
            terminal_tab_drag: None,
            terminal_grid_rects: Vec::new(),
            terminal_picker: TerminalPicker::default(),
            terminal_dragging: false,
            terminal_drag_last: None,
            mouse_to_app: false,
            last_click: None,
            click_count: 0,
            open_folder_requested: false,
            last_file_check: Instant::now(),
            mem_rss_kb: None,
            mem_last_check: Instant::now() - Duration::from_secs(10),
            git_branch: None,
            git_last_check: Instant::now() - Duration::from_secs(10),
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
            dialog_entries_rev: 0,
            dialog_ignored: Vec::new(),
            dialog_show_junk: false,
            dialog_scope: None,
            dialog_list_rect: None,
            dialog_list_start: 0,
            dialog_hover: None,
            tree_drag: None,
            breadcrumb_hits: Vec::new(),
            help_scroll: 0,
            theme,
            icons,
            icons_explicit,
            gui_font_size: config.gui_font_size(),
            gui_fps: config.gui_fps(),
            dialog_size_pct: config.gui_dialog_size(),
            last_terminal_repaint: Instant::now(),
            measured_fps: None,
            gui: false,
            gui_carets: Vec::new(),
            hl_cache: std::collections::HashMap::new(),
            hl_pending: HashSet::new(),
            hl_fallback: Vec::new(),
            hl_tx,
            hl_rx,
            find_hay_cache: None,
            next_buffer_id: 1,
            editor_grid: false,
            editor_panes: Vec::new(),
            scrollbar_thumbs: Vec::new(),
            scrollbar_drag: None,
            tab_hits: Vec::new(),
            tab_strip_rect: None,
            tab_drag: None,
            editor_height: 20,
            last_cursor: None,
            just_opened: None,
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
            config_path: crate::config::global_config_path(),
        })
    }

    /// Whether a project folder is open (vs. the welcome / no-folder state).
    pub fn has_folder(&self) -> bool {
        self.root.is_some()
    }

    /// A directory to spawn terminals in: the active terminal's live cwd (so
    /// `cd`ing around and opening another terminal continues from there),
    /// else whatever the last terminal opened in, else the open folder, else
    /// the home dir.
    fn terminal_cwd(&self) -> PathBuf {
        self.terminals
            .get(self.active_terminal)
            .and_then(TerminalPane::current_dir)
            .or_else(|| self.last_terminal_cwd.clone())
            .or_else(|| self.root.clone())
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
        // Force an immediate re-check next frame instead of showing the
        // outgoing project's branch (or its absence) until the throttle
        // window happens to expire.
        self.git_branch = None;
        self.git_last_check = Instant::now() - Duration::from_secs(10);
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
            Dialog::TerminalPicker => self.init_terminal_picker(),
            Dialog::Help => self.help_scroll = 0,
            Dialog::SearchFiles => self.project_search.open(),
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
            Dialog::TerminalPicker => self.terminal_picker.query.clear(),
            Dialog::Help => self.help_scroll = 0,
            Dialog::SearchFiles => self.project_search.close(),
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

    /// Remove the checked folders — or, if none are checked, the one under the
    /// cursor — from the recents list. Only edits the recents file; the
    /// folders themselves (and any window that has one open) are untouched.
    /// A folder currently open (in this window or another) is skipped rather
    /// than removed — deleting it from Recent while it's open would strand
    /// that live session with no way back to it from here.
    pub fn recent_delete_selected(&mut self) {
        if self.recent_folders.is_empty() {
            return;
        }
        let checked: Vec<usize> = (0..self.recent_folders.len())
            .filter(|i| self.recent_checked.get(*i).copied().unwrap_or(false))
            .collect();
        let requested = if checked.is_empty() { vec![self.recent_cursor] } else { checked };
        let (targets, blocked): (Vec<usize>, Vec<usize>) =
            requested.into_iter().partition(|&i| !self.recent_is_disabled(i));

        if targets.is_empty() {
            self.notify("Cannot delete an open folder from Recent", ToastKind::Error);
            return;
        }
        let remove_set: HashSet<usize> = targets.iter().copied().collect();

        for &i in &targets {
            if let Some(p) = self.recent_folders.get(i) {
                crate::recent::remove(p);
            }
        }

        let mut i = 0usize;
        self.recent_folders.retain(|_| {
            let keep = !remove_set.contains(&i);
            i += 1;
            keep
        });
        let mut i = 0usize;
        self.recent_checked.retain(|_| {
            let keep = !remove_set.contains(&i);
            i += 1;
            keep
        });
        let mut i = 0usize;
        self.recent_disabled.retain(|_| {
            let keep = !remove_set.contains(&i);
            i += 1;
            keep
        });

        self.recent_cursor = self.recent_cursor.min(self.recent_folders.len().saturating_sub(1));
        let n = targets.len();
        if blocked.is_empty() {
            self.notify(format!("Removed {n} folder(s) from recents"), ToastKind::Info);
        } else {
            let skipped = blocked.len();
            self.notify(
                format!("Removed {n} folder(s); skipped {skipped} open folder(s)"),
                ToastKind::Info,
            );
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

    /// Persist the chosen size / FPS / colour to the global config so they
    /// survive a restart, and show a confirmation toast. Called when the
    /// dialog is dismissed (each individual adjustment already saved quietly
    /// via [`Self::persist_settings_quiet`] — this is a final flush + a
    /// visible "saved" acknowledgement).
    pub fn persist_settings(&mut self) {
        match self.persist_settings_quiet() {
            Ok(()) => self.notify("Settings saved", ToastKind::Success),
            Err(e) => self.notify(format!("Couldn't save settings: {e}"), ToastKind::Error),
        }
    }

    /// Save without a toast on success (errors still surface) — used after
    /// every individual Settings-dialog adjustment so a change is never lost
    /// even if the app quits or crashes before the dialog is explicitly
    /// closed.
    fn persist_settings_quiet(&mut self) -> std::io::Result<()> {
        let Some(path) = self.config_path.clone() else {
            return Ok(()); // no resolvable config dir on this platform — nothing to persist to
        };
        let result = crate::config::save_prefs_to(
            &path,
            self.gui_font_size,
            self.gui_fps,
            self.dialog_size_pct,
            self.theme.accent_rgb(),
        );
        if let Err(e) = &result {
            self.notify(format!("Couldn't save settings: {e}"), ToastKind::Error);
        }
        result
    }

    /// The number of focusable sections in the Settings dialog.
    pub const SETTINGS_SECTIONS: usize = 4;

    /// Move between the dialog's sections (font / terminal FPS / dialog size
    /// / colour). `delta` of -1 (Up / Shift+Tab) goes to the previous
    /// section, +1 (Down / Tab) to the next — both wrap around.
    pub fn settings_move_focus(&mut self, delta: i32) {
        let n = Self::SETTINGS_SECTIONS as i32;
        self.settings_focus = (self.settings_focus as i32 + delta).rem_euclid(n) as usize;
    }

    /// Left/right within the focused section: resize the font, step the
    /// terminal FPS, resize dialogs, or pick a colour. Every change is saved
    /// immediately (see [`Self::persist_settings_quiet`]) so nothing is lost
    /// if the app quits before the dialog is closed normally.
    pub fn settings_adjust(&mut self, delta: i32) {
        match self.settings_focus {
            0 => {
                let n = (self.gui_font_size as i32 + delta).clamp(8, 72);
                self.gui_font_size = n as u32;
            }
            1 => {
                let opts = crate::config::FPS_OPTIONS;
                let cur = opts.iter().position(|&f| f == self.gui_fps).unwrap_or(0);
                let idx = (cur as i32 + delta).clamp(0, opts.len() as i32 - 1) as usize;
                self.gui_fps = opts[idx];
            }
            2 => {
                let range = crate::config::DIALOG_SIZE_RANGE;
                let n = (self.dialog_size_pct as i32 + delta)
                    .clamp(*range.start() as i32, *range.end() as i32);
                self.dialog_size_pct = n as u32;
            }
            _ => {
                let len = crate::theme::ACCENT_PALETTE.len() as i32;
                let idx = (self.settings_color as i32 + delta).rem_euclid(len) as usize;
                self.settings_color = idx;
                self.theme.set_accent(crate::theme::ACCENT_PALETTE[idx].1);
            }
        }
        let _ = self.persist_settings_quiet();
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

    /// Pick icons that will actually render in a host terminal. The GUI bundles
    /// its own font (so Nerd glyphs always work there), but a terminal uses
    /// whatever font the user configured — which may lack Nerd glyphs and show
    /// tofu boxes. We try to install the bundled symbols font for the terminal's
    /// glyph fallback (`font`); the default Nerd set is kept only when that font
    /// is already on the system. A user's explicit `icons = "..."` is always
    /// honoured. Call once, before the TUI loop starts.
    pub fn ensure_terminal_icons(&mut self, font: crate::fonts::FontInstall) {
        use crate::fonts::FontInstall;
        if self.icons_explicit || self.icons.mode != crate::icons::IconMode::Nerd {
            return;
        }
        match font {
            // The symbols font is already on the system — the terminal can fall
            // back to it, so keep the Nerd icons.
            FontInstall::AlreadyPresent => {}
            // We just installed it (won't load until the next launch), or we
            // couldn't — use the font-independent set so nothing shows as tofu.
            FontInstall::Installed => {
                self.icons = Icons::new(crate::icons::IconMode::Unicode);
                self.status =
                    "Installed Nerd symbols font — restart Oxru for terminal icon glyphs".into();
            }
            FontInstall::Unsupported | FontInstall::Failed => {
                self.icons = Icons::new(crate::icons::IconMode::Unicode);
            }
        }
    }

    /// Esc — collapse multiple carets back to one. Returns whether it did.
    pub fn clear_editor_carets(&mut self) -> bool {
        self.active_buffer().map(|b| b.clear_extra_carets()).unwrap_or(false)
    }

    /// VSCode-style caret blink half-period (on for this long, then off).
    const BLINK: Duration = Duration::from_millis(500);

    /// How often the terminal event loop must wake up (even with no input) so a
    /// stationary caret keeps visibly blinking instead of sitting solid between
    /// keystrokes. Half the blink period, so neither the "on" nor the "off" phase
    /// is ever skipped entirely.
    pub fn blink_poll_interval() -> Duration {
        Self::BLINK / 2
    }

    /// The gap between unattended repaints at the configured `gui_fps` (e.g.
    /// 100ms at 10fps) — governs both terminal-output-driven repaints
    /// ([`Self::terminal_repaint_due`]) and the GUI's idle floor (toasts,
    /// labels, clock) so "fps" caps the *whole* app's unattended redraw
    /// rate, not just the terminal pane specifically.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn repaint_interval(&self) -> Duration {
        Duration::from_millis(1000 / self.gui_fps.max(1) as u64)
    }

    /// Whether fresh terminal output (`pumped`) is reason enough to repaint
    /// *right now*, given the configured terminal FPS. This only throttles
    /// *unattended* output — the caller should still repaint immediately for
    /// any other reason (a keystroke, mouse input, a dialog opening, …) and
    /// call [`Self::note_terminal_repaint`] whenever a repaint actually
    /// happens, so the interval is measured from the last real paint.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn terminal_repaint_due(&self, pumped: bool) -> bool {
        pumped && self.last_terminal_repaint.elapsed() >= self.repaint_interval()
    }

    /// Record that a repaint just happened, resetting the terminal-FPS clock.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn note_terminal_repaint(&mut self) {
        self.last_terminal_repaint = Instant::now();
    }

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
                // `last_cursor` may still hold a stale `(index, cursor)` pair from
                // a tab that used to sit at this same Vec index (tab removal
                // shifts everything after it down). If that pair happens to
                // collide with this brand-new buffer's (index, cursor 0), the
                // next `ensure_cursor_visible` would wrongly think the cursor
                // hasn't moved and skip re-anchoring to the top, leaving any
                // stray pre-open scroll_row mutation in place. Clearing it here
                // forces that first render to always treat this as a fresh
                // cursor and snap the view to row 0.
                self.last_cursor = None;
                self.just_opened = Some((self.editors[self.active_editor.unwrap()].id, Instant::now()));
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
    ///
    /// Tree-sitter re-parsing + querying a whole file from scratch on every
    /// keystroke is real work (tens of milliseconds for a few-thousand-line
    /// file) — doing it synchronously here on every edit would delay the
    /// frame that shows what was just typed by exactly that much, which reads
    /// as input lag. So only the *first* highlight of a freshly opened buffer
    /// (which already waits on the file read, so a brief one-time cost is
    /// unsurprising) runs synchronously; every stale-from-an-edit recompute
    /// after that runs on a background thread instead.
    ///
    /// The lines shown in the meantime are always split fresh from the
    /// buffer's *current* text (so what you just typed is never missing or
    /// stale), but line-by-line the *colour* is carried over from the last
    /// completed highlight whenever that line's text hasn't changed — only
    /// the line(s) actually touched by the edit fall back to plain. Without
    /// this, every single keystroke blanked the *entire file* to plain text
    /// for a frame or two while the background job ran, which at normal
    /// typing speed (faster than that background job on anything but a
    /// trivial file) reads as constant, whole-file colour flicker rather than
    /// the intended "briefly uncoloured on the rare slow recompute".
    pub fn highlighted_for(&mut self, idx: usize) -> &[Vec<Span>] {
        // Adopt any completed background jobs. A result is only kept if its
        // revision still matches the buffer's current one — one superseded by
        // a later edit (the job started before that edit landed) is stale and
        // just discarded; the job already queued for the new revision (see
        // below) will replace it.
        while let Ok((id, rev, lines)) = self.hl_rx.try_recv() {
            self.hl_pending.remove(&(id, rev));
            let current_rev = self.editors.iter().find(|b| b.id == id).map(|b| b.revision());
            if current_rev == Some(rev) {
                self.hl_cache.insert(id, (rev, lines));
            }
        }

        let Some(buf) = self.editors.get(idx) else {
            return &[];
        };
        let id = buf.id;
        let rev = buf.revision();
        let had_baseline = self.hl_cache.contains_key(&id);
        let stale = match self.hl_cache.get(&id) {
            Some((r, _)) => *r != rev,
            None => true,
        };
        if !stale {
            return &self.hl_cache.get(&id).unwrap().1;
        }

        // `buf`'s borrow ends after this line (everything it's used for is
        // copied out), so the `&mut self` calls below are free to proceed.
        let text = buf.rope.to_string();
        let lang = buf.lang;

        if !had_baseline {
            // Just opened: no previous highlight to show meanwhile anyway, so
            // compute it inline once, exactly like before this change.
            let lines = syntax::highlight(&text, lang, &self.theme);
            self.hl_cache.insert(id, (rev, lines));
            return &self.hl_cache.get(&id).unwrap().1;
        }

        // Build the fallback view: current text, but colour reused per-line
        // from the last completed highlight wherever that line's text is
        // byte-identical to what's there now — so an edit on line 40 doesn't
        // blank lines 1-39 and 41-end while the background job catches up.
        let plain_lines = syntax::plain(&text);
        let old_lines = self.hl_cache.get(&id).map(|(_, l)| l.as_slice()).unwrap_or(&[]);
        self.hl_fallback = plain_lines
            .into_iter()
            .enumerate()
            .map(|(i, plain_line)| {
                let Some(old_line) = old_lines.get(i) else {
                    return plain_line;
                };
                let plain_text = &plain_line[0].0;
                let old_text: String = old_line.iter().map(|(s, _)| s.as_str()).collect();
                if old_text == *plain_text {
                    old_line.clone()
                } else {
                    plain_line
                }
            })
            .collect();

        if self.hl_pending.insert((id, rev)) {
            let theme = self.theme.clone();
            let tx = self.hl_tx.clone();
            std::thread::spawn(move || {
                let lines = syntax::highlight(&text, lang, &theme);
                let _ = tx.send((id, rev, lines));
            });
        }

        &self.hl_fallback
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

    /// Trailing trackpad momentum can keep delivering wheel deltas for a beat
    /// after the gesture that produced them (e.g. scrolling the Files dialog's
    /// list right before pressing Enter to open a match) — long enough to land
    /// on a tab that only just became active. Ignore wheel-driven scrolling of
    /// a buffer for this long after it was newly opened.
    const JUST_OPENED_WHEEL_GRACE: Duration = Duration::from_millis(300);

    /// Scroll the active editor by `delta` rows (mouse wheel). Positive = down.
    /// The cursor stays put; the view can scroll until the last line reaches the
    /// top.
    pub fn editor_scroll(&mut self, delta: i32) {
        let Some(idx) = self.active_editor else {
            return;
        };
        let buf = &mut self.editors[idx];
        if let Some((id, opened_at)) = self.just_opened {
            if id == buf.id && opened_at.elapsed() < Self::JUST_OPENED_WHEEL_GRACE {
                return;
            }
        }
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
    /// cancel at any point leaves every file and terminal untouched. When more
    /// than one blocker remains, the prompt also offers a fast "resolve
    /// everything left the same way" path (`A` / `D`) instead of making the
    /// user step through each tab and terminal one at a time.
    fn prompt_next_quit_blocker(&mut self) {
        let remaining = self.quit_queue.len() - self.quit_pos;
        while let Some(blocker) = self.quit_queue.get(self.quit_pos).copied() {
            match blocker {
                QuitBlocker::UnsavedFile(i) => {
                    let name = self
                        .editors
                        .get(i)
                        .map(|b| b.name())
                        .unwrap_or_else(|| "file".to_string());
                    let all_hint = if remaining > 1 {
                        "  A = save all,  D = discard all,"
                    } else {
                        ""
                    };
                    self.prompt.open_confirm(
                        PromptKind::QuitUnsaved,
                        format!(
                            "\"{name}\" has unsaved changes.   Y = save & quit,  N = discard,{all_hint}  Esc = cancel"
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
                    let all_hint = if remaining > 1 { "  A = close all," } else { "" };
                    self.prompt.open_confirm(
                        PromptKind::QuitTerminal,
                        format!(
                            "\"{label}\" is still running.   Y = close & quit,{all_hint}  N/Esc = cancel"
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

    /// "Save all & quit": resolve every remaining blocker in one step, saving
    /// each remaining unsaved file instead of asking about it individually.
    /// Running terminals need no action — they're just approved for closing.
    pub fn quit_save_all(&mut self) {
        self.quit_finish_all(true);
    }

    /// "Discard all & quit": resolve every remaining blocker in one step,
    /// discarding each remaining unsaved file's changes instead of saving them.
    pub fn quit_discard_all(&mut self) {
        self.quit_finish_all(false);
    }

    fn quit_finish_all(&mut self, save: bool) {
        let remaining: Vec<QuitBlocker> = self.quit_queue[self.quit_pos..].to_vec();
        for blocker in remaining {
            if let QuitBlocker::UnsavedFile(i) = blocker {
                if save {
                    if let Some(buf) = self.editors.get_mut(i) {
                        if let Err(e) = buf.save() {
                            // A single failure aborts the whole quit, same as
                            // the one-at-a-time path — never lose work silently.
                            self.notify(format!("Save failed: {e}"), ToastKind::Error);
                            self.cancel_quit();
                            return;
                        }
                    }
                }
            }
        }
        self.prompt.close();
        self.quitting = false;
        self.quit_queue.clear();
        self.quit_pos = 0;
        self.should_quit = true;
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
        self.find.replace_active = false;
        self.find.replace.clear();
        self.find.replace_cursor = 0;
        self.find.replace_anchor = None;
        self.find.replace_focus = false;
    }

    /// Ctrl+H: toggle the replace field. Opening it moves keyboard focus there;
    /// closing it always returns focus to the query.
    pub fn find_toggle_replace(&mut self) {
        self.find.replace_active = !self.find.replace_active;
        self.find.replace_focus = self.find.replace_active;
    }

    /// Tab while the replace field is showing: swap keyboard focus between the
    /// query and replace fields.
    pub fn find_toggle_focus(&mut self) {
        if self.find.replace_active {
            self.find.replace_focus = !self.find.replace_focus;
        }
    }

    pub fn replace_input(&mut self, c: char) {
        crate::editline::insert(
            &mut self.find.replace,
            &mut self.find.replace_cursor,
            &mut self.find.replace_anchor,
            c,
        );
    }

    pub fn replace_backspace(&mut self) {
        crate::editline::backspace(
            &mut self.find.replace,
            &mut self.find.replace_cursor,
            &mut self.find.replace_anchor,
        );
    }

    pub fn replace_copy(&mut self) {
        if let Some(t) =
            crate::editline::selected_text(&self.find.replace, self.find.replace_cursor, self.find.replace_anchor)
        {
            self.set_clipboard(t);
        }
    }

    pub fn replace_cut(&mut self) {
        self.replace_copy();
        crate::editline::delete_selection(
            &mut self.find.replace,
            &mut self.find.replace_cursor,
            &mut self.find.replace_anchor,
        );
    }

    pub fn replace_paste(&mut self) {
        let t = one_line(self.get_clipboard());
        if !t.is_empty() {
            crate::editline::insert_str(
                &mut self.find.replace,
                &mut self.find.replace_cursor,
                &mut self.find.replace_anchor,
                &t,
            );
        }
    }

    /// Replace the current match with the replace field's text, then advance
    /// to the next match past the edit — reuses the existing match-selection
    /// path, so it's a single undoable edit.
    pub fn replace_current(&mut self) {
        if self.find.matches.is_empty() {
            return;
        }
        let Some(i) = self.active_editor else {
            return;
        };
        let replacement = self.find.replace.clone();
        self.select_current_match();
        self.editors[i].insert_str(&replacement);
        self.recompute_find();
        if !self.find.matches.is_empty() {
            let pos = self.editors[i].cursor;
            self.find.current = self.find.matches.iter().position(|(s, _)| *s >= pos).unwrap_or(0);
            self.select_current_match();
        }
    }

    /// Replace every match with the replace field's text, editing back-to-
    /// front so earlier matches' offsets stay valid as later ones are edited.
    pub fn replace_all(&mut self) {
        if self.find.matches.is_empty() {
            return;
        }
        let Some(i) = self.active_editor else {
            return;
        };
        let replacement = self.find.replace.clone();
        // `find.matches` and `editors` are disjoint fields, so this can walk
        // the match list in place instead of cloning it — worth avoiding
        // since a big file's "replace all" can mean many thousands of matches.
        for &(s, e) in self.find.matches.iter().rev() {
            self.editors[i].select_range(s, e);
            self.editors[i].insert_str(&replacement);
        }
        self.recompute_find();
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

    // ---- project-wide search in files (⌘⇧F) ----------------------------

    /// Open the "Search in Files" dialog (raising it if already open). Needs
    /// an open folder — there's nothing to search across otherwise.
    pub fn open_search_files(&mut self) {
        if self.root.is_none() {
            self.notify("Open a folder first (\u{2325}O)", ToastKind::Info);
            return;
        }
        self.open_dialog(Dialog::SearchFiles);
    }

    /// Re-run after the query text changed. The old results no longer match
    /// the new text, so they're dropped immediately rather than left stale on
    /// screen — but the actual (potentially expensive) disk search is
    /// debounced (see [`Self::poll_pending_search`]), not run on every
    /// keystroke.
    pub fn search_files_query_changed(&mut self) {
        crate::editline::clamp(&self.project_search.query, &mut self.project_search.cursor, &mut self.project_search.anchor);
        self.project_search.invalidate();
    }

    pub fn search_files_input(&mut self, c: char) {
        crate::editline::insert(&mut self.project_search.query, &mut self.project_search.cursor, &mut self.project_search.anchor, c);
        self.search_files_query_changed();
    }

    pub fn search_files_backspace(&mut self) {
        crate::editline::backspace(&mut self.project_search.query, &mut self.project_search.cursor, &mut self.project_search.anchor);
        self.search_files_query_changed();
    }

    pub fn search_files_copy(&mut self) {
        if let Some(t) = crate::editline::selected_text(&self.project_search.query, self.project_search.cursor, self.project_search.anchor) {
            self.set_clipboard(t);
        }
    }

    pub fn search_files_cut(&mut self) {
        self.search_files_copy();
        if crate::editline::delete_selection(&mut self.project_search.query, &mut self.project_search.cursor, &mut self.project_search.anchor) {
            self.search_files_query_changed();
        }
    }

    pub fn search_files_paste(&mut self) {
        let t = one_line(self.get_clipboard());
        if !t.is_empty() {
            crate::editline::insert_str(&mut self.project_search.query, &mut self.project_search.cursor, &mut self.project_search.anchor, &t);
            self.search_files_query_changed();
        }
    }

    pub fn search_files_up(&mut self) {
        self.project_search.move_up();
    }

    pub fn search_files_down(&mut self) {
        self.project_search.move_down();
    }

    /// Run the actual project-wide search — auto-triggered once the debounce
    /// elapses (see [`Self::poll_pending_search`]). Synchronous — reads every
    /// matching project file off disk, so it's deliberately not run on every
    /// keystroke, only after the query settles.
    pub fn run_project_search(&mut self) {
        self.project_search.pending_search_at = None;
        let Some(root) = self.root.clone() else { return };
        let query = self.project_search.query.clone();
        if query.is_empty() {
            self.project_search.invalidate();
            return;
        }
        let (results, truncated) = search::search_project(&root, &query, false);
        let total: usize = results.iter().map(|r| r.matches.len()).sum();
        self.project_search.results = results;
        self.project_search.selected = 0;
        self.project_search.searched = true;
        self.project_search.truncated = truncated;
        if total == 0 {
            self.notify(format!("No results for \"{query}\""), ToastKind::Info);
        }
    }

    /// Auto-run the project search once its debounce window elapses since the
    /// last keystroke — live search-as-you-type without re-scanning the whole
    /// project on every single character. Called every tick from the main
    /// loop (both TUI and GUI), like `poll_file_changes`/`poll_git`. Returns
    /// whether a search actually ran, so the GUI can force an immediate
    /// repaint instead of waiting for the idle floor.
    pub fn poll_pending_search(&mut self) -> bool {
        if self.project_search.pending_search_at.is_some_and(|at| Instant::now() >= at) {
            self.run_project_search();
            true
        } else {
            false
        }
    }

    /// Enter on a result: open its file and select the matched text. Closes
    /// the dialog (via `open_path`), same as picking a file in the Files
    /// dialog.
    pub fn search_files_open_selected(&mut self) {
        let Some((fi, mi)) = self.project_search.selected_match() else {
            return;
        };
        let (path, m) = {
            let r = &self.project_search.results[fi];
            (r.path.clone(), r.matches[mi].clone())
        };
        self.open_path(&path);
        if let Some(buf) = self.active_buffer() {
            let target = m.line.min(buf.line_count().saturating_sub(1));
            let col = m.col.min(buf.line_len_chars(target));
            let end_col = m.end_col.min(buf.line_len_chars(target)).max(col);
            let start = buf.rope.line_to_char(target) + col;
            let end = buf.rope.line_to_char(target) + end_col;
            buf.select_range(start, end);
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
        // The lowercased haystack only actually needs rebuilding when the
        // buffer's *content* changed — typing further into the find query
        // (or a plain re-jump) hits this every keystroke otherwise, redoing
        // an O(file size) collect for no reason. Cached the same way as
        // syntax highlighting: keyed by buffer id + revision.
        let buf = &self.editors[i];
        let (id, rev) = (buf.id, buf.revision());
        let stale = match &self.find_hay_cache {
            Some((cid, crev, _)) => *cid != id || *crev != rev,
            None => true,
        };
        if stale {
            let hay: Vec<char> = buf.rope.chars().map(|c| c.to_ascii_lowercase()).collect();
            self.find_hay_cache = Some((id, rev, hay));
        }
        let hay = &self.find_hay_cache.as_ref().unwrap().2;
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

    /// Move editor tab `from` to position `to` (drag-to-reorder by mouse),
    /// keeping it active.
    fn reorder_editor_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.editors.len() || to >= self.editors.len() {
            return;
        }
        let buf = self.editors.remove(from);
        self.editors.insert(to, buf);
        self.active_editor = Some(to);
        self.hl_cache.clear();
    }

    // ---- mouse ---------------------------------------------------------

    /// Move the cursor to the body cell (`col`, `row`). With `keep_anchor`, the
    /// selection anchor is preserved (so a drag extends the selection); without
    /// it, any selection is dropped. With `alt`, the click instead drops a new
    /// multi-cursor caret there (Alt+Click), leaving the rest of the selection
    /// state untouched — the mouse equivalent of Cmd+Alt+↑/↓.
    fn place_cursor_at(&mut self, col: u16, row: u16, keep_anchor: bool, alt: bool) {
        let Some(idx) = self.active_editor else {
            return;
        };
        // The editor body starts right below the tab strip (which may be
        // several rows tall — see `tab_strip_rows`), plus one more when the
        // breadcrumb bar is also showing above it.
        let body_offset = self.tab_strip_rows() + if self.breadcrumb_row_shown() { 1 } else { 0 };
        let buf = &mut self.editors[idx];
        let view_row = row.saturating_sub(body_offset) as usize;
        let gutter_w = (buf.line_count().max(1).to_string().len() as u16).max(3) + 1;
        let target = (buf.scroll_row + view_row).min(buf.line_count().saturating_sub(1));
        let click_col = col.saturating_sub(gutter_w) as usize;
        let c = click_col.min(buf.line_len_chars(target));
        let pos = buf.rope.line_to_char(target) + c;
        if alt {
            buf.add_caret_at(pos, c);
            return;
        }
        if !keep_anchor {
            buf.clear_selection();
            buf.clear_extra_carets();
        }
        buf.cursor = pos;
        buf.goal_col = c;
    }

    /// True when the editor is showing the tiled grid (and there's more than one
    /// file to tile).
    fn in_grid(&self) -> bool {
        self.editor_grid && self.editors.len() > 1
    }

    /// How many screen rows the editor tab strip occupies this frame — it
    /// wraps onto more rows as more tabs are open (see `ui::tab_grid_rows`),
    /// so mouse handling can't assume a fixed height. Falls back to 1 before
    /// the first render populates `tab_strip_rect`.
    fn tab_strip_rows(&self) -> u16 {
        self.tab_strip_rect.map(|(_, _, _, h)| h).unwrap_or(1)
    }

    /// Whether the breadcrumb bar is shown above the editor body: single-file
    /// view (not the tiled grid), a project root open, and the active buffer
    /// has a real path (not an unsaved "untitled" buffer) — there's nothing
    /// meaningful to show a path for otherwise. The single source of truth
    /// for this shared by rendering (`ui::render`) and the mouse row math.
    pub fn breadcrumb_row_shown(&self) -> bool {
        !self.in_grid()
            && self.root.is_some()
            && self
                .active_editor
                .is_some_and(|i| self.editors[i].path.is_some())
    }

    /// A click at breadcrumb column `col`: open the Files dialog scoped to
    /// whichever folder segment was hit.
    fn breadcrumb_mouse_down(&mut self, col: u16) {
        let Some(folder) = self
            .breadcrumb_hits
            .iter()
            .find(|(s, e, _)| col >= *s && col < *e)
            .map(|(_, _, p)| p.clone())
        else {
            return;
        };
        self.breadcrumb_click(&folder);
    }

    /// Open the Files dialog scoped to `folder` (the project root clears the
    /// scope back to the whole project) — what clicking a breadcrumb segment
    /// or the root does.
    fn breadcrumb_click(&mut self, folder: &Path) {
        self.open_file_dialog();
        if !self.file_dialog.active {
            return; // no root open (shouldn't happen if the breadcrumb was shown)
        }
        let root = self.root.clone();
        self.dialog_scope = if root.as_deref() == Some(folder) {
            None
        } else {
            Some(folder.to_path_buf())
        };
        self.reroot_dialog();
    }

    /// The grid pane `(index, content rect)` under (`col`, `row`), if any.
    fn pane_at(&self, col: u16, row: u16) -> Option<(usize, (u16, u16, u16, u16))> {
        self.editor_panes
            .iter()
            .copied()
            .find(|(_, (x, y, w, h))| col >= *x && col < x + w && row >= *y && row < y + h)
    }

    /// Move the cursor in grid pane `idx` (its content starts at (`px`,`py`)).
    /// See [`Self::place_cursor_at`] for `keep`/`alt`.
    fn place_cursor_in_pane(
        &mut self,
        idx: usize,
        px: u16,
        py: u16,
        col: u16,
        row: u16,
        keep: bool,
        alt: bool,
    ) {
        let buf = &mut self.editors[idx];
        let view_row = row.saturating_sub(py) as usize;
        let gutter_w = (buf.line_count().max(1).to_string().len() as u16).max(3) + 1;
        let target = (buf.scroll_row + view_row).min(buf.line_count().saturating_sub(1));
        let click_col = col.saturating_sub(px + gutter_w) as usize;
        let c = click_col.min(buf.line_len_chars(target));
        let pos = buf.rope.line_to_char(target) + c;
        if alt {
            buf.add_caret_at(pos, c);
            return;
        }
        if !keep {
            buf.clear_selection();
            buf.clear_extra_carets();
        }
        buf.cursor = pos;
        buf.goal_col = c;
    }

    /// Mouse pressed in the editor: focus the pane (grid view), position the
    /// cursor, and start a (potential) drag-selection anchored there. `alt`
    /// drops a multi-cursor caret there instead (Alt+Click) and skips the
    /// selection/drag setup — see [`Self::place_cursor_at`].
    pub fn editor_mouse_down(&mut self, col: u16, row: u16, alt: bool) {
        if self.prompt.active || !self.dialogs.is_empty() || self.active_editor.is_none() {
            return;
        }
        // A click in the editor body means the user wants to type there, not
        // in the find bar — hand keyboard focus back instead of leaving find
        // active (and silently swallowing every keystroke) underneath it.
        if self.find.active {
            self.find_close();
        }
        if let Some(idx) = self.scrollbar_pane_at(col, row) {
            self.scrollbar_mouse_down(idx, row);
            return;
        }
        let clicks = self.click_count;
        if self.in_grid() {
            if let Some((idx, (px, py, _, _))) = self.pane_at(col, row) {
                self.active_editor = Some(idx);
                self.place_cursor_in_pane(idx, px, py, col, row, false, alt);
                if !alt {
                    let buf = &mut self.editors[idx];
                    buf.anchor = Some(buf.cursor);
                    apply_click_select(buf, clicks);
                }
            }
            return;
        }
        let tab_rows = self.tab_strip_rows();
        if row < tab_rows {
            // Select (and arm a possible drag-reorder of) the clicked tab.
            // Tabs can only be closed with a keyboard shortcut, not the mouse.
            if let Some(i) = tab_hit_at(&self.tab_hits, row, col) {
                self.active_editor = Some(i);
                self.tab_drag = Some(i);
            }
            return;
        }
        if row == tab_rows && self.breadcrumb_row_shown() {
            self.breadcrumb_mouse_down(col);
            return;
        }
        self.place_cursor_at(col, row, false, alt);
        if !alt {
            if let Some(buf) = self.active_buffer() {
                buf.anchor = Some(buf.cursor); // anchor the drag here
                apply_click_select(buf, clicks);
            }
        }
    }

    /// The editor pane whose scrollbar column (one past its content rect)
    /// contains `(col, row)`, if any.
    fn scrollbar_pane_at(&self, col: u16, row: u16) -> Option<usize> {
        self.editor_panes
            .iter()
            .find(|(_, (x, y, w, h))| col == *x + *w && row >= *y && row < y + h)
            .map(|(i, _)| *i)
    }

    /// Scrollbar press in pane `idx` at screen row `row`: grab the thumb to
    /// drag it, or jump one page toward the click if it landed on the track.
    fn scrollbar_mouse_down(&mut self, idx: usize, row: u16) {
        let Some((_, ty, th)) = self.scrollbar_thumbs.iter().find(|(i, ..)| *i == idx).copied()
        else {
            return; // nothing to scroll
        };
        if row >= ty && row < ty + th {
            self.scrollbar_drag = Some((idx, row - ty));
            return;
        }
        let Some((_, (_, _, _, ph))) = self.editor_panes.iter().find(|(i, _)| *i == idx).copied()
        else {
            return;
        };
        let page = (ph as usize).saturating_sub(1).max(1);
        if let Some(buf) = self.editors.get_mut(idx) {
            if row < ty {
                buf.scroll_row = buf.scroll_row.saturating_sub(page);
            } else {
                buf.scroll_row = (buf.scroll_row + page).min(buf.line_count().saturating_sub(1));
            }
        }
    }

    /// Continue a scrollbar-thumb drag to screen row `row`; `grab_offset` is
    /// the row within the thumb where the press first grabbed it.
    fn scrollbar_drag_to(&mut self, idx: usize, row: u16, grab_offset: u16) {
        let Some((_, (_, py, _, ph))) = self.editor_panes.iter().find(|(i, _)| *i == idx).copied()
        else {
            return;
        };
        let Some((_, _, th)) = self.scrollbar_thumbs.iter().find(|(i, ..)| *i == idx).copied()
        else {
            return;
        };
        let Some(buf) = self.editors.get_mut(idx) else {
            return;
        };
        let total = buf.line_count().max(1);
        let viewport = ph as usize;
        if total <= viewport {
            return;
        }
        let max_scroll = total - viewport;
        let track_h = ph.saturating_sub(th).max(1) as usize;
        let new_top = row.saturating_sub(py + grab_offset) as usize;
        let ratio = new_top.min(track_h) as f64 / track_h as f64;
        buf.scroll_row = ((ratio * max_scroll as f64).round() as usize).min(max_scroll);
    }

    /// Mouse dragged with the button down: extend the selection to (`col`,`row`).
    pub fn editor_mouse_drag(&mut self, col: u16, row: u16) {
        if self.prompt.active || !self.dialogs.is_empty() || self.active_editor.is_none() {
            return;
        }
        if let Some((idx, grab_offset)) = self.scrollbar_drag {
            self.scrollbar_drag_to(idx, row, grab_offset);
            return;
        }
        // A double/triple-click already selected the word/line under the
        // press. Real pointer hardware almost never reports a click as
        // perfectly motionless, so a drag event lands even on a "still"
        // click — if it hasn't actually left that cell, re-placing the
        // cursor here would collapse the word/line selection back down to
        // just the raw click position. Only skip while still on that exact
        // cell; a genuine drag to a different cell still extends normally.
        if self.click_stayed_on_press_cell(col, row) {
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
                    self.place_cursor_in_pane(idx, px, py, c, r, true, false);
                }
            }
            return;
        }
        if row == 0 {
            return;
        }
        self.place_cursor_at(col, row, true, false);
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
    ///
    /// Capped at [`MAX_TERMINALS`]: every embedded terminal is a real shell
    /// process + a full scrollback buffer (tens of MB once it's actually
    /// produced output), and this is also the path a runaway script can hit
    /// automatically (see [`Self::open_terminal_with_command`], reached via
    /// `termbridge` for every `osascript … do script` the shell shim
    /// intercepts) — with no cap, a script that fires that repeatedly (a
    /// broken watch/build hook looping, say) would spawn shells without limit
    /// until the process runs the machine out of memory.
    pub fn new_terminal(&mut self) {
        if self.terminals.len() >= MAX_TERMINALS {
            self.notify(
                format!("Too many terminals open (max {MAX_TERMINALS}) — close some first"),
                ToastKind::Error,
            );
            return;
        }
        let cwd = self.terminal_cwd();
        match TerminalPane::new(folder_name(&cwd), 24, 80, &cwd, self.terminal_waker.clone()) {
            Ok(pane) => {
                self.last_terminal_cwd = Some(cwd);
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

    /// Move terminal tab `from` to position `to` (drag-to-reorder by mouse),
    /// keeping it active.
    fn reorder_terminal_tab(&mut self, from: usize, to: usize) {
        if from == to || from >= self.terminals.len() || to >= self.terminals.len() {
            return;
        }
        let term = self.terminals.remove(from);
        self.terminals.insert(to, term);
        self.active_terminal = to;
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

    /// Cmd+1..9 — jump straight to the Nth terminal tab (1-indexed), like
    /// browser/VSCode tab shortcuts, instead of stepping through Ctrl+Tab.
    /// No-op past the last tab (so Cmd+9 with 3 terminals just does nothing,
    /// rather than wrapping or erroring).
    pub fn jump_to_terminal(&mut self, n: usize) {
        if n >= 1 && n <= self.terminals.len() {
            self.active_terminal = n - 1;
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
        // Matches macOS's own default double-click interval (~500ms at the
        // "medium" system setting) — 400ms was tighter than that, so a
        // perfectly normal-speed double-click could register as two separate
        // single clicks instead of a word-select.
        const DOUBLE_MS: u128 = 500;
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

    /// Whether `(col, row)` is still the cell of an in-progress double/triple
    /// click — i.e. a drag event fired without the pointer actually having
    /// left the pressed cell. Both `editor_mouse_drag` and
    /// `terminal_mouse_drag` skip re-placing the selection cursor in that
    /// case, so a same-cell drag can't collapse a word/line click-select
    /// back down to the raw click position.
    fn click_stayed_on_press_cell(&self, col: u16, row: u16) -> bool {
        self.click_count >= 2 && self.last_click.is_some_and(|(_, c, r)| c == col && r == row)
    }

    /// Left-button press at global cell `(col, row)`. `shift` forces local text
    /// selection even when a program has grabbed the mouse; `alt` drops a
    /// multi-cursor caret there instead (editor only — see
    /// [`Self::editor_mouse_down`]).
    pub fn mouse_down(&mut self, col: u16, row: u16, shift: bool, alt: bool) {
        self.bump_click(col, row);
        if !self.prompt.active && self.top_dialog() == Some(Dialog::Files) {
            self.dialog_mouse_down(col, row);
            return;
        }
        if self.terminal_focused() {
            // Select (and arm a possible drag-reorder of) the clicked tab.
            // Tabs can only be closed with a keyboard shortcut, not the mouse.
            if let Some(i) = self.terminal_tab_hit(col, row) {
                self.active_terminal = i;
                self.terminal_tab_drag = Some(i);
                return;
            }
            // Grid mode: clicking a non-active cell switches focus to it (a
            // click already inside the active cell falls through below, so it
            // still places the cursor / starts a selection as usual).
            if let Some(i) = self.terminal_grid_hit(col, row) {
                if i != self.active_terminal {
                    self.active_terminal = i;
                    return;
                }
            }
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
            self.editor_mouse_down(col, row, alt);
        }
    }

    /// Pointer moved with the left button held.
    pub fn mouse_drag(&mut self, col: u16, row: u16) {
        if self.terminal_focused() {
            if let Some(dragged) = self.terminal_tab_drag {
                if let Some(j) = self.terminal_tab_hit(col, row) {
                    if j != dragged {
                        self.reorder_terminal_tab(dragged, j);
                        self.terminal_tab_drag = Some(j);
                    }
                }
                return;
            }
            if self.mouse_to_app {
                self.terminal_mouse_drag_report(col, row);
            } else {
                self.terminal_mouse_drag(col, row);
            }
        } else {
            if let Some(dragged) = self.tab_drag {
                if row < self.tab_strip_rows() {
                    if let Some(j) = tab_hit_at(&self.tab_hits, row, col) {
                        if j != dragged {
                            self.reorder_editor_tab(dragged, j);
                            self.tab_drag = Some(j);
                        }
                    }
                }
                return;
            }
            self.editor_mouse_drag(col, row);
        }
    }

    /// Left-button release at global cell `(col, row)`.
    pub fn mouse_up(&mut self, col: u16, row: u16) {
        self.tab_drag = None;
        self.terminal_tab_drag = None;
        self.scrollbar_drag = None;
        if !self.prompt.active && self.top_dialog() == Some(Dialog::Files) {
            self.dialog_mouse_up(col, row);
            return;
        }
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

    /// The terminal tab under `(col, row)`, if the terminal tab strip is
    /// showing (focused, not buried) and the cell falls within its
    /// (possibly multi-row) grid.
    fn terminal_tab_hit(&self, col: u16, row: u16) -> Option<usize> {
        let (x, y, w, h) = self.terminal_tabstrip_rect?;
        if row < y || row >= y + h || col < x || col >= x + w {
            return None;
        }
        tab_hit_at(&self.terminal_tab_hits, row - y, col)
    }

    /// Mouse wheel: `lines` > 0 is wheel-up (toward history / page top).
    pub fn mouse_wheel(&mut self, lines: i32, col: u16, row: u16, shift: bool) {
        // Unlike mouse_down/up/move, this used to skip the dialog check entirely
        // and scroll straight through to whatever editor tab was active — so
        // wheeling over the Files dialog's list (a very natural way to browse
        // search results) silently dragged the *background* editor's scroll
        // position around instead. If a newly-opened file happened to land in
        // that same active-editor slot moments later (or trailing trackpad
        // momentum kept delivering wheel deltas for a bit after Enter/click
        // closed the dialog), it would open already scrolled away from the top
        // instead of at row 0. Route to the dialog like every other mouse method
        // does whenever one is on top.
        if !self.prompt.active && self.top_dialog() == Some(Dialog::Files) {
            self.dialog_wheel(lines);
            return;
        }
        if self.terminal_focused() {
            self.terminal_wheel(lines, col, row, shift);
        } else {
            // Editor: wheel-up moves the view toward the top (negative delta).
            self.editor_scroll(-lines);
        }
    }

    /// Move the Files dialog's selection by the wheel amount (matches
    /// `terminal_wheel`'s step-and-cap convention) instead of falling through
    /// to the editor behind it.
    fn dialog_wheel(&mut self, lines: i32) {
        let n = (lines.unsigned_abs() as usize).min(10);
        for _ in 0..n {
            if lines > 0 {
                self.dialog_up();
            } else {
                self.dialog_down();
            }
        }
    }

    /// Per-frame hook: keep a local-selection drag held against an edge
    /// auto-scrolling. No-op during a program-forwarded drag.
    pub fn mouse_drag_tick(&mut self) {
        if !self.mouse_to_app {
            self.terminal_drag_autoscroll();
        }
    }

    /// Pointer moved with no button held. Drives the file picker's row hover
    /// highlight and which tab (if any) shows its close "×".
    pub fn mouse_move(&mut self, col: u16, row: u16) {
        if !self.prompt.active && self.top_dialog() == Some(Dialog::Files) {
            self.dialog_mouse_move(col, row);
        } else if self.dialog_hover.is_some() {
            self.dialog_hover = None;
        }
    }

    // ---- Files dialog mouse (tree browse + fuzzy results list) ---------

    /// The tree/match row index under global cell `(col, row)`, if it falls
    /// inside the results list recorded by `ui::render_file_dialog`.
    fn dialog_row_at(&self, col: u16, row: u16) -> Option<usize> {
        let (x, y, w, h) = self.dialog_list_rect?;
        if row < y || row >= y + h || col < x || col >= x + w {
            return None;
        }
        Some(self.dialog_list_start + (row - y) as usize)
    }

    pub fn dialog_mouse_down(&mut self, col: u16, row: u16) {
        self.tree_drag = None;
        let Some((x, ..)) = self.dialog_list_rect else {
            return;
        };
        let Some(idx) = self.dialog_row_at(col, row) else {
            return;
        };
        if self.dialog_tree_mode() {
            let Some(e_is_dir) = self.tree.entries.get(idx).map(|e| e.is_dir) else {
                return;
            };
            let e_depth = self.tree.entries[idx].depth;
            self.tree.selected = idx;
            let chevron_start = x + e_depth as u16 * 2;
            if e_is_dir && col >= chevron_start && col < chevron_start + 2 {
                self.toggle_tree_selected();
            } else {
                // Arm a possible drag-move; `dialog_mouse_up` decides whether
                // it was a plain click (open) or a drop onto another row.
                self.tree_drag = Some(idx);
            }
        } else {
            if idx >= self.file_dialog.matches.len() {
                return;
            }
            self.file_dialog.selected = idx;
            if !self.file_dialog.checked.is_empty() && col == x {
                self.dialog_toggle_check();
            }
        }
    }

    pub fn dialog_mouse_up(&mut self, col: u16, row: u16) {
        let Some(dragged) = self.tree_drag.take() else {
            // No drag was armed: search-mode selection / a chevron toggle /
            // checkbox click already happened on press — release just opens.
            if !self.dialog_tree_mode() {
                self.dialog_open_selected();
            }
            return;
        };
        match self.dialog_row_at(col, row) {
            Some(target) if target != dragged => self.drop_tree_entry(dragged, target),
            _ => self.dialog_open_selected(),
        }
    }

    pub fn dialog_mouse_move(&mut self, col: u16, row: u16) {
        self.dialog_hover = self.dialog_row_at(col, row);
    }

    /// A tree drag-release landed on a different row: move `from_idx`'s entry
    /// into `to_idx`'s folder if it is one, else just open the dragged entry
    /// (dropping onto a file isn't a meaningful move target).
    fn drop_tree_entry(&mut self, from_idx: usize, to_idx: usize) {
        let Some(from_path) = self.tree.entries.get(from_idx).map(|e| e.path.clone()) else {
            return;
        };
        let Some(to_is_dir) = self.tree.entries.get(to_idx).map(|e| e.is_dir) else {
            return;
        };
        if !to_is_dir {
            self.tree.selected = from_idx;
            self.dialog_open_selected();
            return;
        }
        let to_path = self.tree.entries[to_idx].path.clone();
        self.move_entry_into(&from_path, &to_path);
    }

    /// Move `from` into directory `to_dir` (drag-and-drop in the Files dialog
    /// tree), keeping its filename, then refresh the tree/search corpus
    /// exactly like the rename prompt does.
    fn move_entry_into(&mut self, from: &Path, to_dir: &Path) {
        let Some(name) = from.file_name() else {
            return;
        };
        let dest = to_dir.join(name);
        if dest.as_path() == from {
            return;
        }
        match std::fs::rename(from, &dest) {
            Ok(()) => {
                let label = to_dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string());
                self.notify(format!("Moved to {label}/"), ToastKind::Success);
            }
            Err(e) => self.notify(format!("Move failed: {e}"), ToastKind::Error),
        }
        self.tree.refresh();
        if let Some(root) = self.root.clone() {
            self.files_cache = fstree::collect_files(&root, false);
        }
        self.rebuild_dialog_entries();
        if self.file_dialog.active {
            self.dialog_refilter();
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
        // See `click_stayed_on_press_cell` — a same-cell drag from a
        // double/triple-click must not collapse the word/line selection.
        if self.click_stayed_on_press_cell(col, row) {
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

    // ---- terminal quick-switcher (⌘K) -----------------------------------

    /// Open the terminal quick-switcher (raising it if already open).
    pub fn open_terminal_picker(&mut self) {
        if self.terminals.len() > 1 {
            self.open_dialog(Dialog::TerminalPicker);
        } else {
            self.notify("Only one terminal open", ToastKind::Info);
        }
    }

    fn init_terminal_picker(&mut self) {
        self.terminal_picker.query.clear();
        self.terminal_picker_refilter();
    }

    /// Re-rank `matches` against the current query. Empty query = every
    /// terminal in tab order (so the palette is a full overview, not just a
    /// search box); freshly recomputed on every keystroke rather than cached
    /// — at most `MAX_TERMINALS` short labels, cheap either way.
    fn terminal_picker_refilter(&mut self) {
        let labels = self.terminal_labels();
        let q = &self.terminal_picker.query;
        let mut scored: Vec<(i64, usize)> = labels
            .iter()
            .enumerate()
            .filter_map(|(i, label)| {
                crate::filedialog::fuzzy_score(q, label).map(|(score, _)| (score, i))
            })
            .collect();
        // Stable sort by score descending — ties keep tab order.
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        self.terminal_picker.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.terminal_picker.selected = 0;
    }

    pub fn terminal_picker_input(&mut self, c: char) {
        self.terminal_picker.query.push(c);
        self.terminal_picker_refilter();
    }

    pub fn terminal_picker_backspace(&mut self) {
        self.terminal_picker.query.pop();
        self.terminal_picker_refilter();
    }

    pub fn terminal_picker_move(&mut self, delta: i32) {
        let n = self.terminal_picker.matches.len();
        if n == 0 {
            return;
        }
        let i = (self.terminal_picker.selected as i32 + delta).rem_euclid(n as i32) as usize;
        self.terminal_picker.selected = i;
    }

    /// Jump to the highlighted terminal and close the picker (back to the
    /// terminal dialog, now showing it).
    pub fn terminal_picker_confirm(&mut self) {
        if let Some(&i) = self.terminal_picker.matches.get(self.terminal_picker.selected) {
            self.active_terminal = i;
        }
        self.dismiss_dialog(Dialog::TerminalPicker);
    }

    /// The grid-mode cell whose rect contains `(col, row)`, if any — see
    /// `terminal_grid_rects`.
    fn terminal_grid_hit(&self, col: u16, row: u16) -> Option<usize> {
        self.terminal_grid_rects.iter().position(|&(x, y, w, h)| {
            col >= x && col < x + w && row >= y && row < y + h
        })
    }

    /// Open a new embedded terminal that runs `cmd` — used when a script in a
    /// terminal asks to launch a command in its own window.
    fn open_terminal_with_command(&mut self, cmd: &str) {
        // `new_terminal` silently no-ops past `MAX_TERMINALS` (just a toast) —
        // without this check we'd fall through and inject `cmd` into whatever
        // terminal was already active, which would be actively wrong rather
        // than just declined.
        let before = self.terminals.len();
        self.new_terminal();
        if self.terminals.len() == before {
            return;
        }
        if let Some(term) = self.active_terminal_mut() {
            term.folder = short_term_title(cmd);
            term.send_input(format!("{cmd}\n").as_bytes());
        }
        self.terminal_modal = true;
        self.notify("Opened embedded terminal", ToastKind::Info);
    }

    /// Refresh `mem_rss_kb` (throttled to every 2s — real RSS barely moves
    /// frame to frame, and reading it forks a `ps`, so there's no reason to
    /// do it more often). Called once per frame from both run loops.
    pub fn poll_memory(&mut self) {
        if self.mem_last_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.mem_last_check = Instant::now();
        self.mem_rss_kb = read_own_rss_kb();
    }

    /// Refresh `git_branch` (throttled to every 3s — same reasoning as
    /// `poll_memory`, one `git` spawn is enough to stay current without
    /// forking on every frame). Called once per frame from both run loops.
    pub fn poll_git(&mut self) {
        if self.git_last_check.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.git_last_check = Instant::now();
        self.git_branch = self.root.as_deref().and_then(read_git_status);
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
        // Tells `FileDialog::refilter`'s prepared-candidate cache the entry
        // list actually changed, so it knows to rebuild instead of reusing a
        // stale cache from before this rebuild.
        self.dialog_entries_rev = self.dialog_entries_rev.wrapping_add(1);
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
        // MRU position of each entry, so recently-opened files rank to the top.
        let recent: Vec<Option<usize>> = self
            .dialog_entries
            .iter()
            .map(|(p, _)| self.recent_rank(p))
            .collect();
        // `dialog_display` and `file_dialog` are disjoint fields, so this reads
        // the (potentially large, one-String-per-project-file) display list in
        // place instead of cloning it on every keystroke of a search query.
        self.file_dialog
            .refilter(&self.dialog_display, self.dialog_entries_rev, &recent);
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

    /// Reveal the selected file/folder in the OS file manager (Finder on
    /// macOS), with it highlighted — not just opening its parent folder.
    pub fn dialog_reveal_in_finder(&mut self) {
        let Some((p, _)) = self.dialog_selected() else {
            self.notify("Nothing selected", ToastKind::Info);
            return;
        };
        reveal_in_file_manager(&p);
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

/// Open the OS file manager with `path` selected/highlighted (macOS Finder
/// via `open -R`; on Linux most file managers support the same "select this
/// file" convention via `xdg-open`'s `--select`-less siblings, but there's no
/// portable equivalent, so this just opens the containing folder there).
/// Silent on failure — same "best effort, no error dialog" convention as the
/// clipboard/spawn helpers nearby.
#[cfg(target_os = "macos")]
fn reveal_in_file_manager(path: &Path) {
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_in_file_manager(path: &Path) {
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

#[cfg(not(unix))]
fn reveal_in_file_manager(path: &Path) {
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

/// This process's current resident-set size in KiB, via `ps` — the same
/// number Activity Monitor / `top` would show for this PID, so it's directly
/// comparable to what a user sees there. `None` on any failure (no `ps` on
/// this platform, spawn failed, unparseable output) — the footer just omits
/// the reading rather than showing something wrong.
fn read_own_rss_kb() -> Option<u64> {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The open folder's git branch and whether the working tree is dirty, via a
/// single `git status --porcelain --branch` (its first line carries the
/// branch, e.g. `## main...origin/main`; any further line means an
/// uncommitted change). `None` if `root` isn't inside a git repo, `git` isn't
/// installed, or HEAD is detached — the footer just omits the branch rather
/// than showing something misleading.
fn read_git_status(root: &Path) -> Option<(String, bool)> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "--branch"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let head = lines.next()?.strip_prefix("## ")?;
    let branch = head.split("...").next().unwrap_or(head).trim();
    if branch.is_empty() || branch.starts_with("HEAD") {
        return None; // detached HEAD — no single branch name to show
    }
    let dirty = lines.next().is_some();
    Some((branch.to_string(), dirty))
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
    use crate::input;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Style;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "hello\n").unwrap();
        dir
    }

    #[test]
    fn opening_a_new_file_always_starts_scrolled_to_the_top() {
        let dir = workspace();
        let big: String = (0..500).map(|i| format!("line {i}\n")).collect();
        fs::write(dir.path().join("big.txt"), &big).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("big.txt"));
        app.ensure_cursor_visible(20); // establishes last_cursor for this tab
        // Simulate an old tab that's already well past its just-opened grace
        // window and genuinely mid-scrolled (bypass `editor_scroll`, which
        // would itself be within its own grace window this soon after opening).
        app.editors[app.active_editor.unwrap()].scroll_row = 300;
        app.just_opened = None;
        app.ensure_cursor_visible(20);
        assert_eq!(app.editors[app.active_editor.unwrap()].scroll_row, 300);

        // Open a brand-new file that wasn't open before, then simulate a wheel
        // delta (e.g. trailing trackpad momentum) landing right after the open
        // but before the next render gets a chance to run.
        app.open_path(&dir.path().join("notes.txt"));
        let idx = app.active_editor.unwrap();
        app.mouse_wheel(-3, 0, 0, false);
        assert_eq!(app.editors[idx].scroll_row, 0, "a stray wheel delta right after opening must be ignored");

        app.ensure_cursor_visible(20);
        assert_eq!(app.editors[idx].scroll_row, 0, "freshly opened file should render at the top");
    }

    #[test]
    fn mouse_wheel_scrolls_the_files_dialog_not_the_background_editor() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.ensure_cursor_visible(20);
        app.editor_scroll(1);
        let scroll_before = app.editors[app.active_editor.unwrap()].scroll_row;

        app.open_file_dialog();
        assert_eq!(app.top_dialog(), Some(Dialog::Files));
        app.mouse_wheel(-3, 5, 5, false);

        assert_eq!(
            app.editors[app.active_editor.unwrap()].scroll_row, scroll_before,
            "wheeling over the dialog must not touch the editor behind it"
        );
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
    fn replace_current_swaps_one_match_and_advances() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "foo bar foo baz foo").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        for c in "foo".chars() {
            app.find_input(c);
        }
        assert_eq!(app.find.matches.len(), 3);
        for c in "QUX".chars() {
            app.replace_input(c);
        }

        app.replace_current();
        assert_eq!(app.editors[0].rope.to_string(), "QUX bar foo baz foo");
        assert_eq!(app.find.matches.len(), 2, "the replaced text no longer matches");
    }

    #[test]
    fn replace_all_swaps_every_match() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "foo bar foo baz foo").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        for c in "foo".chars() {
            app.find_input(c);
        }
        for c in "QUX".chars() {
            app.replace_input(c);
        }

        app.replace_all();
        assert_eq!(app.editors[0].rope.to_string(), "QUX bar QUX baz QUX");
        assert!(app.find.matches.is_empty(), "nothing left to match after replacing all");
    }

    #[test]
    fn search_files_requires_a_folder() {
        let mut app = App::new(None).unwrap();
        app.open_search_files();
        assert_eq!(app.top_dialog(), None, "no folder open -> dialog refuses to open");
    }

    #[test]
    fn search_files_auto_search_finds_matches_across_files() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "needle in a haystack\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        app.open_search_files();
        assert_eq!(app.top_dialog(), Some(Dialog::SearchFiles));
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        assert!(!app.project_search.searched, "typing alone must not immediately run the (expensive) disk search");

        // Simulate the debounce window having passed — the only way a search
        // runs now (there's no Enter-to-search fallback).
        app.project_search.pending_search_at = Some(Instant::now() - Duration::from_millis(1));
        assert!(app.poll_pending_search());
        assert!(app.project_search.searched);
        assert_eq!(app.project_search.total_matches(), 2);
        assert_eq!(app.project_search.results.len(), 2);
    }

    #[test]
    fn search_files_opens_selected_match_and_selects_the_matched_text() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        app.run_project_search();
        assert_eq!(app.project_search.total_matches(), 1);

        app.search_files_open_selected();
        assert_eq!(app.top_dialog(), None, "opening a result closes the dialog");
        assert_eq!(app.active_editor, Some(0));
        let buf = &app.editors[0];
        assert_eq!(buf.selected_text().as_deref(), Some("needle"), "the matched word is selected");
        assert_eq!(buf.cursor_row(), 1, "cursor lands on the matching line");
    }

    #[test]
    fn search_files_editing_query_invalidates_stale_results() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "needle\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        app.run_project_search();
        assert!(app.project_search.searched);
        assert_eq!(app.project_search.total_matches(), 1);

        // Editing the query again must drop the now-stale results rather than
        // leave them on screen next to text that no longer matches them.
        app.search_files_input('!');
        assert!(!app.project_search.searched);
        assert!(app.project_search.results.is_empty());
    }

    #[test]
    fn search_files_auto_runs_after_the_debounce_elapses() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "needle\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        assert!(!app.project_search.searched, "typing alone doesn't run it immediately");

        // Debounce window hasn't elapsed yet -> still nothing.
        assert!(!app.poll_pending_search());
        assert!(!app.project_search.searched);

        // Simulate the debounce window having passed (same backdating trick
        // used for the double-click timing tests).
        app.project_search.pending_search_at = Some(Instant::now() - Duration::from_millis(1));
        assert!(app.poll_pending_search(), "poll should report that a search just ran");
        assert!(app.project_search.searched);
        assert_eq!(app.project_search.total_matches(), 1);
        assert!(app.project_search.pending_search_at.is_none(), "the timer is consumed once it fires");
    }

    #[test]
    fn search_files_enter_never_forces_a_search_only_opens_the_selection() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "needle\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        assert!(app.project_search.pending_search_at.is_some(), "debounce armed");

        // Enter, before the debounce would have fired: with no results yet,
        // there's nothing to open, and — unlike the old Enter-searches
        // behavior — it must NOT short-circuit the wait and search early.
        app.search_files_open_selected();
        assert!(!app.project_search.searched, "Enter no longer triggers a search");
        assert!(app.project_search.pending_search_at.is_some(), "the debounce is left to run its course");
        assert_eq!(app.top_dialog(), Some(Dialog::SearchFiles), "nothing to open -> dialog stays put");

        // Once the debounce actually produces a result, Enter opens it.
        app.project_search.pending_search_at = Some(Instant::now() - Duration::from_millis(1));
        app.poll_pending_search();
        assert!(app.project_search.searched);
        app.search_files_open_selected();
        assert_eq!(app.top_dialog(), None, "Enter on a real result opens it and closes the dialog");
        assert_eq!(app.active_editor, Some(0));
    }

    #[test]
    fn ctrl_r_toggles_replace_and_tab_swaps_focus() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui = true; // this test is about find/replace, not the TUI 3-key rule
        app.open_path(&dir.path().join("notes.txt"));
        app.find_open();
        assert!(!app.find.replace_active);

        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        input::handle_key(&mut app, ctrl_r);
        assert!(app.find.replace_active, "Ctrl+R opens the replace field");
        assert!(app.find.replace_focus, "opening replace also focuses it");

        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        input::handle_key(&mut app, tab);
        assert!(!app.find.replace_focus, "Tab swaps focus back to the query");
        input::handle_key(&mut app, tab);
        assert!(app.find.replace_focus, "Tab swaps focus back to replace");
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

    /// A new terminal should continue from wherever the active terminal's
    /// shell actually is (after a `cd`), not reset to the project root.
    #[test]
    #[cfg(target_os = "macos")]
    fn new_terminal_inherits_the_active_terminals_live_cwd() {
        let dir = workspace();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.new_terminal(); // t0, at the project root
        let i = app.active_terminal;
        app.terminals[i].send_input(b"cd sub && printf 'HERE\\n'\n");
        assert!(pump_until(&mut app, |t| screen_has(t, "HERE")), "cd landed");
        // `lsof` needs a beat to catch up with the shell's new cwd (compare by
        // basename since the tmpdir path may differ from its canonical form,
        // e.g. `/var/folders/..` vs `/private/var/folders/..` on macOS).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && app.terminals[i]
                .current_dir()
                .and_then(|p| p.file_name().map(|n| n.to_owned()))
                .as_deref()
                != Some(std::ffi::OsStr::new("sub"))
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        app.new_terminal(); // t1, should start in `sub`
        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.terminals[app.active_terminal].folder, "sub");
    }

    /// A runaway `termbridge` request loop (or just a lot of manual `⌥T`s)
    /// must not be able to spawn shells without limit — each one is a real
    /// process plus a scrollback buffer, so unbounded growth is a genuine
    /// memory (and file-descriptor) leak, not just visual clutter.
    #[test]
    fn new_terminal_stops_at_the_cap() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        for _ in 0..MAX_TERMINALS + 5 {
            app.new_terminal();
        }
        assert_eq!(app.terminals.len(), MAX_TERMINALS);
    }

    /// The termbridge auto-open path must not fall through to injecting the
    /// queued command into whatever terminal happens to be active once the
    /// cap is hit — it should just decline, leaving existing terminals alone.
    #[test]
    fn capped_terminal_request_does_not_hijack_an_existing_terminal() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        for _ in 0..MAX_TERMINALS {
            app.new_terminal();
        }
        let label_before = app.terminals[app.active_terminal].folder.clone();
        app.open_terminal_with_command("echo should-not-run");
        assert_eq!(app.terminals.len(), MAX_TERMINALS);
        assert_eq!(app.terminals[app.active_terminal].folder, label_before);
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

    /// Clicking in the editor body while Find is open must hand keyboard
    /// focus back to the editor, not leave it stuck in the find bar.
    #[test]
    fn clicking_editor_while_find_active_closes_find() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "hello world\nsecond line\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        for c in "world".chars() {
            app.find_input(c);
        }
        assert!(app.find.active);

        app.editor_mouse_down(0, 1, false);
        assert!(!app.find.active, "click on editor text should close find");
    }

    #[test]
    fn double_click_selects_word_triple_click_selects_line() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "hello world\nsecond line\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        // gutter width for a 2-line file is 4 ("1 " budget rounded up to 3, +1);
        // row 2 is the body's first text row (1 tab-strip row + 1 breadcrumb
        // row, both defaulted since nothing has rendered yet). Click lands on
        // the 'o' in "world" (chars 6-10 -> col 4+6..4+10).
        let col = 4 + 8;
        let row = 2;

        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        assert!(app.editors[app.active_editor.unwrap()].selection().is_none(), "a single click leaves no selection");

        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        let buf = &app.editors[app.active_editor.unwrap()];
        assert_eq!(buf.selected_text().as_deref(), Some("world"), "double-click should select the word");

        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        let buf = &app.editors[app.active_editor.unwrap()];
        assert_eq!(buf.selected_text().as_deref(), Some("hello world\n"), "triple-click should select the whole line");
    }

    /// A double-click at the default macOS "medium" double-click speed
    /// (~500ms between presses) must still register as a streak, not reset
    /// to a fresh single click.
    #[test]
    fn double_click_registers_at_a_slower_but_still_normal_click_speed() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "hello world\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        let col = 4 + 8;
        let row = 2;
        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        // Backdate the click so the next press is ~450ms later — slower than
        // a fast double-click, but well within a normal one.
        app.last_click = app.last_click.map(|(_, c, r)| (Instant::now() - Duration::from_millis(450), c, r));
        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);

        let buf = &app.editors[app.active_editor.unwrap()];
        assert_eq!(buf.selected_text().as_deref(), Some("world"), "a ~450ms gap should still count as a double-click");
    }

    /// Real pointer hardware essentially never reports a click as perfectly
    /// motionless, so a `CursorMoved`/drag event lands even on a "still"
    /// double-click — without the same-cell guard, that phantom drag re-runs
    /// `place_cursor_at`, which recomputes the cursor from the raw click
    /// column and collapses the word selection down to "up to the click
    /// point" instead of the full word.
    #[test]
    fn phantom_drag_on_a_stationary_double_click_does_not_shrink_the_word_selection() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "hello world\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        let col = 4 + 8; // lands mid-word, inside "world"
        let row = 2;
        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        app.mouse_down(col, row, false, false); // the double-click
        app.editor_mouse_drag(col, row); // same-cell phantom drag before release
        app.mouse_up(col, row);

        let buf = &app.editors[app.active_editor.unwrap()];
        assert_eq!(
            buf.selected_text().as_deref(),
            Some("world"),
            "a same-cell drag must not shrink the double-click's word selection"
        );
    }

    /// `recompute_find` must not re-collect the whole buffer into a lowercased
    /// `Vec<char>` on every keystroke of the find query — only when the
    /// buffer's content has actually changed since the cached copy.
    #[test]
    fn find_haystack_cache_reused_until_buffer_edited() {
        let dir = workspace();
        let f = dir.path().join("f.txt");
        fs::write(&f, "cat cat cat").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);

        app.find_open();
        app.find_input('c');
        assert_eq!(app.find.matches.len(), 3);
        let rev_after_first = app.find_hay_cache.as_ref().map(|(_, r, _)| *r);
        assert!(rev_after_first.is_some());

        // Typing more of the query without touching the buffer: same
        // revision, so the cached haystack must be reused, not rebuilt.
        app.find_input('a');
        assert_eq!(app.find.matches.len(), 3, "\"ca\" still matches 3 times");
        assert_eq!(
            app.find_hay_cache.as_ref().map(|(_, r, _)| *r),
            rev_after_first,
            "haystack cache should not rebuild when the buffer didn't change"
        );

        // An actual edit must invalidate it.
        app.active_buffer().unwrap().insert_str("x");
        app.find_changed();
        assert_ne!(
            app.find_hay_cache.as_ref().map(|(_, r, _)| *r),
            rev_after_first,
            "an edit must invalidate the cached haystack"
        );
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
    fn quit_save_all_saves_every_remaining_unsaved_file() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.open_path(&dir.path().join("src/main.rs"));
        app.active_buffer().unwrap().insert_str("Y");
        app.request_quit();
        assert_eq!(app.prompt.kind(), Some(PromptKind::QuitUnsaved));
        app.quit_save_all();
        assert!(app.should_quit, "resolves every blocker and quits in one step");
        assert!(!app.prompt.active);
        assert!(
            fs::read_to_string(dir.path().join("notes.txt")).unwrap().starts_with('X'),
            "first file saved"
        );
        assert!(
            fs::read_to_string(dir.path().join("src/main.rs")).unwrap().starts_with('Y'),
            "second file saved too"
        );
    }

    #[test]
    fn quit_discard_all_discards_every_remaining_unsaved_file() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.active_buffer().unwrap().insert_str("X");
        app.open_path(&dir.path().join("src/main.rs"));
        app.active_buffer().unwrap().insert_str("Y");
        app.request_quit();
        app.quit_discard_all();
        assert!(app.should_quit);
        assert_eq!(fs::read_to_string(dir.path().join("notes.txt")).unwrap(), "hello\n");
        assert_eq!(fs::read_to_string(dir.path().join("src/main.rs")).unwrap(), "fn main() {}\n");
    }

    #[test]
    fn quit_close_all_terminals_in_one_step() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.new_terminal();
        app.new_terminal();
        app.terminals[0].set_running_for_test();
        app.terminals[1].set_running_for_test();
        app.request_quit();
        assert_eq!(app.prompt.kind(), Some(PromptKind::QuitTerminal));
        app.quit_discard_all();
        assert!(app.should_quit, "both running-terminal blockers resolved in one step");
        assert!(!app.prompt.active);
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
    fn terminal_picker_filters_and_jumps_to_a_match() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // terminal 0: "<folder>"
        app.new_terminal(); // terminal 1 (inserted beside 0): "<folder> 2"
        app.new_terminal(); // terminal 2 (inserted beside 1): "<folder> 3"
        assert_eq!(app.terminals.len(), 3);
        app.active_terminal = 0;

        app.open_terminal_picker();
        assert_eq!(app.top_dialog(), Some(Dialog::TerminalPicker));
        // Unfiltered: every terminal, in tab order.
        assert_eq!(app.terminal_picker.matches, vec![0, 1, 2]);

        // "3" only matches the disambiguated third terminal's label.
        app.terminal_picker_input('3');
        assert_eq!(app.terminal_picker.matches, vec![2]);

        app.terminal_picker_confirm();
        assert_eq!(app.active_terminal, 2, "confirming jumps to the matched terminal");
        // Back to the terminal dialog underneath, not fully closed.
        assert_eq!(app.top_dialog(), Some(Dialog::Terminal));
    }

    #[test]
    fn terminal_picker_backspace_widens_the_filter_again() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        app.new_terminal();
        app.open_terminal_picker();

        app.terminal_picker_input('2');
        assert_eq!(app.terminal_picker.matches.len(), 1);
        app.terminal_picker_backspace();
        assert_eq!(app.terminal_picker.matches.len(), 2, "empty query shows every terminal again");
    }

    #[test]
    fn terminal_grid_click_switches_focus_without_touching_the_active_cell() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        app.new_terminal();
        app.terminal_grid = true;
        app.active_terminal = 0;
        // Fake what `ui::render_terminal_modal` would have recorded this frame.
        app.terminal_grid_rects = vec![(0, 1, 40, 10), (40, 1, 40, 10)];

        // Clicking the second (non-active) cell switches focus and consumes
        // the click — it does not also start a text selection there.
        app.mouse_down(45, 5, false, false);
        assert_eq!(app.active_terminal, 1);
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
    fn recent_delete_removes_cursor_row_when_nothing_checked() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let a = dir.path().join("a-recent-target");
        let b = dir.path().join("b-recent-target");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        app.recent_open = true;
        app.recent_folders = vec![a.clone(), b.clone()];
        app.recent_cursor = 0;
        app.recent_checked = vec![false; 2];
        app.recent_disabled = vec![false; 2];

        app.recent_delete_selected();

        assert_eq!(app.recent_folders, vec![b]);
        assert_eq!(app.recent_checked, vec![false]);
        assert_eq!(app.recent_cursor, 0);
    }

    #[test]
    fn recent_delete_removes_all_checked_rows() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let a = dir.path().join("a-recent-target");
        let b = dir.path().join("b-recent-target");
        let c = dir.path().join("c-recent-target");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        app.recent_open = true;
        app.recent_folders = vec![a.clone(), b.clone(), c.clone()];
        app.recent_cursor = 2;
        app.recent_checked = vec![true, false, true];
        app.recent_disabled = vec![false; 3];

        app.recent_delete_selected();

        // Only the unchecked middle row survives, and the cursor is clamped
        // into range rather than pointing past the end.
        assert_eq!(app.recent_folders, vec![b]);
        assert_eq!(app.recent_cursor, 0);
    }

    #[test]
    fn recent_delete_refuses_a_currently_open_folder() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let a = dir.path().join("a-recent-target");
        std::fs::create_dir_all(&a).unwrap();
        app.recent_open = true;
        app.recent_folders = vec![a.clone()];
        app.recent_cursor = 0;
        app.recent_checked = vec![false];
        app.recent_disabled = vec![true]; // "a" is open in some window

        app.recent_delete_selected();

        assert_eq!(app.recent_folders, vec![a], "the open folder must survive");
        assert_eq!(
            app.toast.as_ref().map(|t| t.message.as_str()),
            Some("Cannot delete an open folder from Recent")
        );
    }

    #[test]
    fn recent_delete_skips_open_folders_but_removes_the_rest() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let a = dir.path().join("a-recent-target");
        let b = dir.path().join("b-recent-target");
        let c = dir.path().join("c-recent-target");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        app.recent_open = true;
        app.recent_folders = vec![a.clone(), b.clone(), c.clone()];
        app.recent_checked = vec![true, true, true]; // all three selected
        app.recent_disabled = vec![false, true, false]; // "b" is open

        app.recent_delete_selected();

        assert_eq!(app.recent_folders, vec![b], "only the open folder survives");
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
    fn settings_dialog_has_font_fps_size_and_colour_sections() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_settings();
        assert_eq!(app.settings_focus, 0); // font
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 1); // terminal FPS
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 2); // dialog size
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 3); // colour
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 0, "wraps back to font (only four sections)");
    }

    #[test]
    fn settings_move_focus_negative_delta_goes_backward() {
        // Up (delta -1) must actually move to the *previous* section, not
        // just repeat whatever the "next section" step does — Up and Down
        // used to both call the same forward-only toggle.
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_settings();
        assert_eq!(app.settings_focus, 0);

        app.settings_move_focus(-1);
        assert_eq!(app.settings_focus, 3, "Up from the first section wraps to the last");
        app.settings_move_focus(-1);
        assert_eq!(app.settings_focus, 2);
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 3, "Down moves forward again");
    }

    #[test]
    fn settings_adjusts_font_and_accent() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // Every settings_adjust now autosaves — point it at a scratch file so
        // this test never touches the real machine config.
        app.config_path = Some(dir.path().join("scratch-config.toml"));
        app.open_settings();
        assert!(app.settings_open);
        assert_eq!(app.settings_focus, 0); // font size first

        let before = app.gui_font_size;
        app.settings_adjust(1);
        assert_eq!(app.gui_font_size, before + 1);
        app.settings_adjust(-1);
        assert_eq!(app.gui_font_size, before);

        // Switch to the colour section and pick the next colour.
        app.settings_move_focus(1);
        app.settings_move_focus(1);
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 3);
        let c0 = app.settings_color;
        app.settings_adjust(1);
        assert_ne!(app.settings_color, c0);
        // The live theme accent now matches the picked palette entry.
        assert_eq!(app.theme.accent_index(), Some(app.settings_color));

        // Settings is still the focused dialog in the stack.
        assert_eq!(app.top_dialog(), Some(Dialog::Settings));
    }

    #[test]
    fn settings_adjusts_terminal_fps_through_the_offered_steps() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.config_path = Some(dir.path().join("scratch-config.toml"));
        // The default-of-60 case is already covered hermetically by
        // config::tests::missing_files_yield_defaults; here we only care
        // about the stepping behaviour, so start from a known value.
        app.gui_fps = 60;
        app.open_settings();
        app.settings_move_focus(1);
        assert_eq!(app.settings_focus, 1);

        app.settings_adjust(-1);
        assert_eq!(app.gui_fps, 30);
        app.settings_adjust(-1);
        assert_eq!(app.gui_fps, 20);
        app.settings_adjust(-10); // clamps at the lowest offered step, doesn't wrap
        assert_eq!(app.gui_fps, 1);
        app.settings_adjust(1);
        assert_eq!(app.gui_fps, 5);
        app.settings_adjust(10); // clamps at the highest, doesn't wrap either
        assert_eq!(app.gui_fps, 60);
    }

    #[test]
    fn settings_adjusts_dialog_size_within_80_99_range() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.config_path = Some(dir.path().join("scratch-config.toml"));
        app.dialog_size_pct = 90;
        app.open_settings();
        app.settings_move_focus(2);
        assert_eq!(app.settings_focus, 2);

        app.settings_adjust(-1);
        assert_eq!(app.dialog_size_pct, 89);
        app.settings_adjust(-20); // clamps at the floor, doesn't go below 80
        assert_eq!(app.dialog_size_pct, 80);
        app.settings_adjust(30); // clamps at the ceiling, doesn't go above 99
        assert_eq!(app.dialog_size_pct, 99);
    }

    /// The actual bug report this fixes: fps was only written to disk when
    /// the Settings dialog closed cleanly, so a change was silently lost if
    /// the app quit (or crashed) first. Now every adjustment is persisted
    /// immediately.
    #[test]
    fn settings_adjust_persists_immediately_without_closing_the_dialog() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let config_path = dir.path().join("scratch-config.toml");
        app.config_path = Some(config_path.clone());
        app.open_settings();
        app.settings_move_focus(1);
        app.gui_fps = 60;

        app.settings_adjust(-1); // -> 30, saved without the dialog ever closing
        assert!(app.settings_open, "dialog is still open");

        let saved = crate::config::Config::load_with(Some(&config_path), dir.path());
        assert_eq!(saved.gui_fps(), 30, "the adjustment reached disk immediately");
    }

    #[test]
    fn terminal_repaint_is_throttled_to_configured_fps() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui_fps = 5; // 200ms between terminal-driven repaints
        app.note_terminal_repaint();

        // Right after a repaint, more output alone isn't reason to repaint yet.
        assert!(!app.terminal_repaint_due(true));
        // With no new output at all, never due regardless of elapsed time.
        assert!(!app.terminal_repaint_due(false));

        // Back-date the clock past the interval: now output is due to show.
        app.last_terminal_repaint = Instant::now() - Duration::from_millis(250);
        assert!(app.terminal_repaint_due(true));
    }

    #[test]
    fn repaint_interval_matches_configured_fps() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui_fps = 1;
        assert_eq!(app.repaint_interval(), Duration::from_millis(1000));
        app.gui_fps = 60;
        assert_eq!(app.repaint_interval(), Duration::from_millis(16));
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
    fn terminal_keeps_nerd_when_symbols_font_present() {
        use crate::fonts::FontInstall;
        use crate::icons::IconMode;
        // Default is Nerd; when the symbols font is already installed the
        // terminal can fall back to it, so we keep the Nerd icons.
        let mut app = App::new(None).unwrap();
        assert_eq!(app.icons.mode, IconMode::Nerd);
        app.ensure_terminal_icons(FontInstall::AlreadyPresent);
        assert_eq!(app.icons.mode, IconMode::Nerd);
    }

    #[test]
    fn terminal_falls_back_to_unicode_when_font_missing() {
        use crate::fonts::FontInstall;
        use crate::icons::IconMode;
        // Just-installed (not yet loaded) or unavailable → font-independent set
        // so nothing renders as tofu this session.
        for outcome in [FontInstall::Installed, FontInstall::Unsupported, FontInstall::Failed] {
            let mut app = App::new(None).unwrap();
            app.ensure_terminal_icons(outcome);
            assert_eq!(app.icons.mode, IconMode::Unicode, "{outcome:?} -> Unicode");
        }
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

        // Render once so the tab-strip hit regions (`app.tab_hits`) are
        // populated, the same way a real frame would before any click lands.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let select_col = |app: &App, i: usize| {
            app.tab_hits
                .iter()
                .find(|h| h.tab == i)
                .map(|h| h.col_start)
                .unwrap_or_else(|| panic!("tab {i} has a select hit region"))
        };
        let (tab0_col, tab1_col) = (select_col(&app, 0), select_col(&app, 1));

        app.editor_mouse_down(tab0_col, 0, false);
        assert_eq!(app.active_editor, Some(0), "clicking tab 0 selects it");
        app.editor_mouse_down(tab1_col, 0, false);
        assert_eq!(app.active_editor, Some(1), "clicking tab 1 selects it");

        // Click into the body: row 3 (tab strip + breadcrumb bar + editor
        // view row 1 -> line 1), past the gutter, lands the cursor there.
        app.editor_mouse_down(20, 3, false);
        app.editor_mouse_up();
        let b = &app.editors[1];
        assert_eq!(b.cursor_row(), 1, "cursor moved to the clicked line");
        assert!(b.selection().is_none(), "a plain click leaves no selection");
    }

    #[test]
    fn clicking_a_wrapped_tab_on_row_two_selects_it() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // At TestBackend width 80 (3 columns of TAB_CELL_W=24 each), 4 tabs
        // wrap onto a second grid row.
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            app.open_path(&dir.path().join(name));
        }
        assert_eq!(app.active_editor, Some(3));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let tab3_hit = app
            .tab_hits
            .iter()
            .find(|h| h.tab == 3)
            .expect("tab 3 has a select hit region");
        assert_eq!(tab3_hit.row, 1, "the 4th tab wrapped onto grid row 1");

        app.active_editor = Some(0);
        app.editor_mouse_down(tab3_hit.col_start, 1, false);
        assert_eq!(app.active_editor, Some(3), "clicking grid row 1 selects the wrapped tab");
    }

    #[test]
    fn mouse_never_closes_a_tab_only_selects() {
        // Tabs close via a keyboard shortcut only — no close glyph is ever
        // rendered (hover or not), and clicking anywhere in a tab's cell,
        // including the column where the old close button used to sit, only
        // ever selects it.
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("notes.txt"));
        app.open_path(&dir.path().join("src/main.rs"));
        let open_count = app.editors.len();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();

        let (hit_start, hit_end) = {
            let h = app.tab_hits.iter().find(|h| h.tab == 0).expect("tab 0 has a hit region");
            (h.col_start, h.col_end)
        };
        let row_text: String = (0..80)
            .map(|c| terminal.backend().buffer()[(c, 0)].symbol().to_string())
            .collect();
        assert!(!row_text.contains('\u{d7}'), "no close glyph is ever rendered in the tab strip");

        // Hovering (mouse_move, no button) changes nothing about the tab
        // strip's own content — there's no hover state left to reveal.
        app.mouse_move(hit_start, 0);
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let after_hover: String = (0..80)
            .map(|c| terminal.backend().buffer()[(c, 0)].symbol().to_string())
            .collect();
        assert_eq!(row_text, after_hover, "hovering a tab doesn't change the strip's rendering");

        // Clicking anywhere across tab 0's cell — including where a close
        // button used to be — only ever selects it, never closes it.
        for col in hit_start..hit_end {
            app.editor_mouse_down(col, 0, false);
            assert_eq!(app.editors.len(), open_count, "clicking a tab never closes it");
        }
        assert_eq!(app.active_editor, Some(0), "the last click selected tab 0");
    }

    #[test]
    fn mouse_drag_selects_text() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs")); // "fn main() {}\n"

        // Press at the start of line 0 (row 2: tab strip + breadcrumb bar),
        // drag a few columns right.
        app.editor_mouse_down(4, 2, false); // gutter is ~4 cols, so col 0 of the text
        app.editor_mouse_drag(8, 2);
        app.editor_mouse_up();
        let b = &app.editors[0];
        let sel = b.selected_text().expect("drag created a selection");
        assert!(!sel.is_empty(), "selection has text, got {sel:?}");
    }

    #[test]
    fn alt_click_drops_a_multi_cursor_caret() {
        let dir = workspace();
        fs::write(dir.path().join("two.txt"), "abc\ndef\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("two.txt"));
        assert_eq!(app.editors[0].caret_count(), 1);

        // Plain click on line 0 places the single cursor there (row 2: tab
        // strip + breadcrumb bar).
        app.editor_mouse_down(4, 2, false);
        assert_eq!(app.editors[0].cursor_row(), 0);
        assert_eq!(app.editors[0].caret_count(), 1);

        // Alt+click on line 1 (row 3) drops a second caret instead of moving
        // the existing cursor — the mouse equivalent of Cmd+Alt+↓.
        app.editor_mouse_down(4, 3, true);
        let b = &app.editors[0];
        assert_eq!(b.caret_count(), 2, "alt+click adds a caret rather than replacing the cursor");
        assert_eq!(b.cursor_row(), 1, "the new caret is the primary, on the clicked line");
    }

    #[test]
    fn breadcrumb_click_scopes_files_dialog_to_that_folder() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        assert!(app.breadcrumb_row_shown());

        render_once(&mut app);
        let (col_start, _, folder) = app
            .breadcrumb_hits
            .iter()
            .find(|(_, _, p)| p == &dir.path().join("src"))
            .cloned()
            .expect("src/ is a breadcrumb segment");
        assert_eq!(folder, dir.path().join("src"));

        // Clicking the breadcrumb row (row 1, below the tab strip) on the
        // "src" segment opens the Files dialog scoped to that folder.
        app.editor_mouse_down(col_start, 1, false);
        assert_eq!(app.top_dialog(), Some(Dialog::Files));
        assert_eq!(app.dialog_scope.as_deref(), Some(dir.path().join("src").as_path()));
    }

    #[test]
    fn breadcrumb_root_click_clears_scope() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        render_once(&mut app);
        let (col_start, _, _) = app
            .breadcrumb_hits
            .iter()
            .find(|(_, _, p)| p == &dir.path().to_path_buf())
            .cloned()
            .expect("the project root is the first breadcrumb segment");

        app.editor_mouse_down(col_start, 1, false);
        assert!(app.dialog_scope.is_none(), "clicking the root segment clears any scope");
    }

    #[test]
    fn scrollbar_drag_scrolls_the_view() {
        let dir = workspace();
        let long = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        fs::write(dir.path().join("long.txt"), &long).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("long.txt"));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let (idx, ty, _th) = *app
            .scrollbar_thumbs
            .first()
            .expect("a 200-line file overflows a 24-row viewport");
        assert_eq!(idx, 0);
        assert_eq!(app.editors[0].scroll_row, 0);

        let (_, (x, _, w, _)) = *app.editor_panes.iter().find(|(i, _)| *i == 0).unwrap();
        let bar_col = x + w; // one past the content rect, where the scrollbar column sits

        // Grab the thumb and drag it down the track.
        app.editor_mouse_down(bar_col, ty, false);
        app.editor_mouse_drag(bar_col, ty + 15);
        assert!(app.editors[0].scroll_row > 0, "dragging the thumb down scrolls the view");
    }

    #[test]
    fn scrollbar_track_click_pages_toward_the_click() {
        let dir = workspace();
        let long = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        fs::write(dir.path().join("long.txt"), &long).unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("long.txt"));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let (_, (x, _, w, _)) = *app.editor_panes.iter().find(|(i, _)| *i == 0).unwrap();
        let bar_col = x + w;
        let (_, ty, th) = *app.scrollbar_thumbs.first().unwrap();

        // Click the track below the thumb: pages down without needing a drag.
        app.editor_mouse_down(bar_col, ty + th + 1, false);
        assert!(app.editors[0].scroll_row > 0, "track click below the thumb pages down");
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
        app.mouse_down(3, 2, false, false);
        let press = String::from_utf8_lossy(&app.terminals[i].take_sent()).into_owned();
        assert!(
            press.contains("\u{1b}[<0;4;3M"),
            "plain click forwards SGR press, got {press:?}"
        );
        app.mouse_up(3, 2);
        let _ = app.terminals[i].take_sent();

        // Shift+click -> local selection; nothing reaches the program.
        app.mouse_down(3, 2, true, false);
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
        app.mouse_down(0, 0, false, false);
        app.mouse_drag(79, 6);
        app.mouse_up(79, 6);
        let txt = app.terminals[i].selection_text().unwrap_or_default();
        assert!(
            txt.contains("PICKME-TARGET"),
            "drag should select the visible output, got {txt:?}"
        );
    }

    #[test]
    fn phantom_drag_on_a_stationary_double_click_does_not_shrink_terminal_word_selection() {
        let (_d, mut app) = terminal_mouse_app();
        let i = app.active_terminal;
        app.terminals[i].send_input(b"printf 'hello wonderful\\n'\n");
        assert!(pump_until(&mut app, |t| screen_has(t, "hello wonderful")), "printf output");

        // Find "wonderful" on screen and land the click mid-word.
        let needle = "wonderful";
        let (row, col) = {
            let t = &app.terminals[i];
            let screen = t.screen();
            let mut found = None;
            'outer: for r in 0..24u16 {
                for c in 0..(80u16.saturating_sub(needle.len() as u16)) {
                    let s: String = (0..needle.len() as u16)
                        .filter_map(|k| screen.cell(r, c + k).map(|cell| cell.contents()))
                        .collect();
                    if s == needle {
                        found = Some((r, c + 3));
                        break 'outer;
                    }
                }
            }
            found.expect("printf output should be visible on screen")
        };

        app.mouse_down(col, row, false, false);
        app.mouse_up(col, row);
        app.mouse_down(col, row, false, false); // the double-click
        app.mouse_drag(col, row); // same-cell phantom drag before release
        app.mouse_up(col, row);

        assert_eq!(
            app.terminals[i].selection_text().as_deref(),
            Some(needle),
            "a same-cell drag must not shrink the terminal's double-click word selection"
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

    /// An edit to an already-highlighted buffer must never block on a full
    /// tree-sitter re-highlight: `highlighted_for` should hand back text that
    /// already reflects the edit immediately (so typing is never delayed),
    /// with the real (coloured) highlight landing a beat later once the
    /// background job completes.
    #[test]
    fn editing_a_highlighted_buffer_shows_text_instantly_then_recolors() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        // Establish the initial (synchronous, first-open) highlight baseline.
        let _ = app.highlighted_for(0);

        app.active_buffer().unwrap().insert_str("XYZZY");
        // Right after the edit — even before any background job could
        // possibly have finished — the returned lines must already contain
        // the freshly typed text.
        let rebuilt: String = app
            .highlighted_for(0)
            .iter()
            .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rebuilt.contains("XYZZY"), "typed text must show up without delay");

        // Give the background recompute a moment to land, then confirm the
        // cache picks it up (poll: it's a real thread, not deterministic).
        let mut recolored = false;
        for _ in 0..200 {
            if app.highlighted_for(0).iter().flatten().any(|(_, s)| *s != Style::default()) {
                recolored = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(recolored, "highlight should catch up in the background");
    }

    /// An edit to one line must not blank the *whole file's* colour while the
    /// background recompute is pending — only the line that actually changed
    /// should fall back to plain; every other line keeps its last-known
    /// colour. Without this, every keystroke flickered the entire buffer to
    /// plain text for a frame or two.
    #[test]
    fn editing_one_line_keeps_other_lines_colored_while_pending() {
        let dir = workspace();
        let src = dir.path().join("src/main.rs");
        fs::write(&src, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&src);
        // Establish the initial (synchronous, first-open) highlight baseline.
        let baseline = app.highlighted_for(0).to_vec();
        assert!(
            baseline.iter().flatten().any(|(_, s)| *s != Style::default()),
            "sanity: the baseline highlight actually colours something"
        );

        // Edit only line 1 ("fn two() {}") — move the cursor there and type.
        let buf = app.active_buffer().unwrap();
        buf.cursor = "fn one() {}\nfn ".len();
        buf.insert_str("X");

        let fallback = app.highlighted_for(0);
        assert_eq!(fallback.len(), baseline.len());
        for (i, (old_line, new_line)) in baseline.iter().zip(fallback.iter()).enumerate() {
            if i == 1 {
                continue; // the edited line — allowed to fall back to plain
            }
            assert_eq!(old_line, new_line, "unedited line {i} must keep its cached colour");
        }
        let edited_text: String = fallback[1].iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(edited_text, "fn Xtwo() {}", "the edited line's text is still current");
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
        app.just_opened = None; // past the just-opened wheel grace window
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

    /// A render pass is required before any Files-dialog mouse test: the list
    /// rect/scroll offset (`dialog_list_rect`/`dialog_list_start`) are only
    /// populated by `ui::render_file_dialog`, mirroring how a real frame would
    /// precede any click.
    fn render_once(app: &mut App) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| crate::ui::render(f, app)).unwrap();
    }

    #[test]
    fn dialog_click_toggles_folder_then_opens_file() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        render_once(&mut app);
        let (x, y, ..) = app.dialog_list_rect.expect("Files dialog list rect recorded");

        // entries[0] is the root folder itself (auto-expanded); entries[1] is
        // "src" (a directory — dirs sort first) at depth 1 — click its
        // chevron (column x + depth*2) to expand it.
        assert_eq!(app.tree.entries[1].name, "src");
        assert!(app.tree.entries[1].is_dir);
        assert!(!app.tree.entries[1].expanded);
        let chevron_col = x + app.tree.entries[1].depth as u16 * 2;
        app.dialog_mouse_down(chevron_col, y + 1);
        app.dialog_mouse_up(chevron_col, y + 1);
        assert!(app.tree.entries[1].expanded, "clicking the chevron expands the folder");

        // Re-render: src/ is now expanded with main.rs beneath it at row 2.
        render_once(&mut app);
        let (x, y, ..) = app.dialog_list_rect.unwrap();
        assert_eq!(app.tree.entries[2].name, "main.rs");
        app.dialog_mouse_down(x, y + 2);
        app.dialog_mouse_up(x, y + 2);
        let i = app.active_editor.expect("clicking a file opens it");
        assert_eq!(app.editors[i].name(), "main.rs");
    }

    #[test]
    fn dialog_drag_moves_file_into_folder() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();

        // entries[0] is the root folder itself; among its children, dirs sort
        // first: [1] "src" (dir), [2] "notes.txt" (file).
        assert_eq!(app.tree.entries[1].name, "src");
        assert_eq!(app.tree.entries[2].name, "notes.txt");

        // Drag row 2 (notes.txt) onto row 1 (src/).
        app.drop_tree_entry(2, 1);

        assert!(dir.path().join("src/notes.txt").exists(), "file moved into src/");
        assert!(!dir.path().join("notes.txt").exists(), "no longer at root");
    }

    #[test]
    fn dialog_hover_tracks_row_under_pointer() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        render_once(&mut app);
        let (x, y, ..) = app.dialog_list_rect.unwrap();

        app.mouse_move(x, y + 1);
        assert_eq!(app.dialog_hover, Some(1));
        app.mouse_move(0, 0); // outside the results list
        assert_eq!(app.dialog_hover, None);
    }

    #[test]
    fn terminal_dialog_position_is_stable_when_something_opens_on_top() {
        // The terminal dialog used to "peek out" (shift a couple cells) like
        // every other stacked dialog whenever something opened on top of it
        // — jarring here because its content (shell text, blinking cursor)
        // is dense and live, unlike the quieter dialogs that convention
        // actually suits. Opening the ⌘K quick-switcher over it should dim
        // it but never move it.
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal();
        app.new_terminal();

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 45)).unwrap();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let find_borders = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| -> Vec<(u16, u16)> {
            let buf = terminal.backend().buffer();
            (0..buf.area().height)
                .flat_map(|y| (0..buf.area().width).map(move |x| (x, y)))
                .filter(|&(x, y)| buf[(x, y)].symbol() == "\u{256d}")
                .collect()
        };
        let before = find_borders(&terminal);
        assert_eq!(before.len(), 1, "just the terminal dialog's own border");

        app.open_terminal_picker();
        terminal.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let after = find_borders(&terminal);
        assert!(
            after.contains(&before[0]),
            "the terminal dialog's border is still exactly where it was ({:?}), found: {:?}",
            before[0],
            after
        );
    }
}




