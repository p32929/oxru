//! One embedded terminal: a PTY running the user's shell, with its output fed
//! through a `vt100` emulator so it can be rendered as a cell grid. A background
//! thread reads the PTY into a channel; the UI thread drains it in [`pump`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

/// How often to re-check the foreground process for the tab label.
const PROC_POLL: Duration = Duration::from_millis(400);

/// Lines of scrollback history each terminal keeps. A terminal that's actually
/// filled its scrollback with real output can run tens of MB per 1,000 lines
/// (each cell carries its own styling), so this is a real memory knob, not
/// just a UX one — 3,000 lines is still generous (VSCode's own default is
/// 1,000) while keeping a single terminal's worst case well under 30MB.
const SCROLLBACK: usize = 3_000;

/// Called from a terminal's reader thread whenever new output arrives, to wake
/// the GUI event loop so it redraws promptly (winit throttles its idle timer on
/// macOS, which otherwise leaves a running CLI looking "stuck" between inputs).
pub type Waker = std::sync::Arc<dyn Fn() + Send + Sync>;

/// A read lock on a terminal's emulator that dereferences to its
/// [`vt100::Screen`], so callers keep writing `term.screen().cell(..)` even
/// though the parser now lives behind a mutex (written by the reader thread).
pub struct ScreenGuard<'a>(std::sync::MutexGuard<'a, vt100::Parser>);

impl std::ops::Deref for ScreenGuard<'_> {
    type Target = vt100::Screen;
    fn deref(&self) -> &vt100::Screen {
        self.0.screen()
    }
}

pub struct TerminalPane {
    /// The base label (project folder), e.g. `server_nestjs`.
    pub folder: String,
    /// The live foreground process name, e.g. `node` (empty when only the
    /// shell is running).
    proc: String,
    /// Basename of the user's shell, so we can tell "just a shell" from a
    /// running command.
    shell_name: String,
    last_proc_check: Instant,
    /// The vt100 emulator, written by the **reader thread** (so PTY output is
    /// consumed even when the UI event loop is parked) and read under the lock
    /// for rendering and queries.
    parser: Arc<Mutex<vt100::Parser>>,
    /// Bytes the reader thread has parsed since the last [`pump`] — lets the UI
    /// know a redraw is warranted without re-draining anything itself.
    pending: Arc<AtomicU64>,
    /// Latency probe: the reader stamps the first chunk of a fresh output burst
    /// here; [`pump`] takes it and logs if that output waited too long for a
    /// frame (i.e. the UI loop was parked) — the freeze, quantified.
    stamp: Arc<Mutex<Option<Instant>>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    rows: u16,
    cols: u16,
    /// Scrollback offset in rows (0 = following the live bottom).
    scroll: usize,
    /// Selection anchor / live end as `(line_id, col)`, where `line_id` is the
    /// number of lines above the live bottom (0 = bottom row). Storing it this way
    /// (rather than visible coords) keeps a selection anchored to its text as the
    /// view scrolls, so it can span far more than one screen.
    sel_anchor: Option<(usize, u16)>,
    sel_cursor: Option<(usize, u16)>,
    /// Copy mode: a free-floating cursor + selection over the screen, decoupled
    /// from the shell — arrows move the cursor instead of being sent to the
    /// running program. Toggled with ⌥↑/⌥↓, exited with Esc.
    copy_mode: bool,
    /// Set when a foreground command finishes (`is_running()` goes
    /// true→false) while nobody's watching this tab; cleared by
    /// [`Self::mark_viewed`]. Lets the tab strip flag "something finished
    /// running here" separately from "something is running right now".
    finished_unseen: bool,
    /// Test-only recorder of every byte handed to `send_input`, so tests can
    /// assert the exact escape sequences a key/mouse action produces without
    /// racing the shell's echo.
    #[cfg(test)]
    sent: Vec<u8>,
}

impl TerminalPane {
    /// Spawn the user's `$SHELL` in `cwd` as a **login** shell. `folder` is the
    /// base tab label (usually the project directory name).
    ///
    /// `new_default_prog` runs `$SHELL` with argv0 prefixed by `-` (e.g. `-zsh`),
    /// which makes it a login shell — so it sources `~/.zprofile` / `~/.zlogin`
    /// (where macOS users usually set up PATH, Homebrew, and aliases), exactly
    /// like VSCode's integrated terminal. A plain non-login shell would skip
    /// those and end up missing commands and PATH entries.
    pub fn new(
        folder: impl Into<String>,
        rows: u16,
        cols: u16,
        cwd: &Path,
        waker: Option<Waker>,
    ) -> Result<Self> {
        let mut cmd = CommandBuilder::new_default_prog();
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        // Route scripts that try to open new OS Terminal windows back into Oxru.
        for (k, v) in crate::termbridge::child_env() {
            cmd.env(k, v);
        }
        Self::spawn(folder, rows, cols, cmd, waker)
    }

    /// Whether a foreground command (beyond the bare shell) is running — i.e.
    /// closing this terminal would interrupt something.
    pub fn is_running(&self) -> bool {
        !self.proc.is_empty()
    }

    /// Whether a foreground command finished here since this tab was last
    /// viewed — distinct from [`Self::is_running`], which is about *now*.
    pub fn finished_unseen(&self) -> bool {
        self.finished_unseen
    }

    /// Call when this terminal is the one actually on screen (the active tab
    /// of a focused terminal dialog) — clears the finished-unseen flag.
    pub fn mark_viewed(&mut self) {
        self.finished_unseen = false;
    }

    /// The shell's actual live working directory (reflects any `cd` the user
    /// has typed), best-effort. `None` if it can't be determined — the
    /// caller falls back to whatever cwd it already has on hand.
    pub fn current_dir(&self) -> Option<PathBuf> {
        process_cwd(self.master.process_group_leader()?)
    }

    /// Force `is_running()` to report true without actually spawning and
    /// waiting on a real foreground process — real detection depends on the
    /// shell's own OSC reporting and is already covered elsewhere; callers
    /// testing the *quit* flow just need a `RunningTerminal` blocker to exist.
    #[cfg(test)]
    pub fn set_running_for_test(&mut self) {
        self.proc = "sleep".to_string();
    }

    /// Force `finished_unseen()` to report true without needing a real
    /// running→idle process transition — for app/ui-level tests of the tab
    /// indicator; the transition logic itself is covered directly in this
    /// module's own tests.
    #[cfg(test)]
    pub fn set_finished_unseen_for_test(&mut self) {
        self.finished_unseen = true;
    }

    /// The tab label: the folder, plus the running command when one is active.
    pub fn display_name(&self) -> String {
        if self.proc.is_empty() {
            self.folder.clone()
        } else {
            format!("{} \u{00b7} {}", self.folder, self.proc)
        }
    }

    pub fn spawn(
        folder: impl Into<String>,
        rows: u16,
        cols: u16,
        cmd: CommandBuilder,
        waker: Option<Waker>,
    ) -> Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let pty = NativePtySystem::default();
        let pair = pty.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id();
        // Drop the slave so the master sees EOF when the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        // The emulator lives behind a lock and is fed by the reader thread, NOT
        // the UI thread. This is the core of the no-freeze design: PTY output is
        // parsed into the screen the instant it arrives, so even if winit parks
        // the event loop (macOS does this during occlusion / resize / background),
        // the terminal contents stay current and there's never a catch-up burst —
        // the UI just renders the already-up-to-date screen whenever it next wakes.
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));
        let pending = Arc::new(AtomicU64::new(0));
        let stamp: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        {
            let parser = parser.clone();
            let pending = pending.clone();
            let stamp = stamp.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            tracing::info!(?pid, "terminal reader: EOF (shell exited)");
                            break;
                        }
                        Err(e) => {
                            tracing::info!(?pid, error = %e, "terminal reader: read error");
                            break;
                        }
                        Ok(n) => {
                            // Parse off the UI thread so output is consumed even
                            // while the event loop is parked.
                            parser.lock().unwrap().process(&buf[..n]);
                            pending.fetch_add(n as u64, Ordering::Release);
                            // Stamp the first chunk of a burst so pump() can measure
                            // how long it waited to be shown (the freeze probe).
                            if let Ok(mut s) = stamp.lock() {
                                if s.is_none() {
                                    *s = Some(Instant::now());
                                }
                            }
                            // Nudge the UI to redraw now that there's fresh output.
                            if let Some(w) = &waker {
                                w();
                            }
                        }
                    }
                }
            });
        }
        tracing::info!(?pid, rows, cols, "terminal spawned");

        let shell_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(|n| n.to_string()))
            .unwrap_or_else(|| "sh".to_string());

        Ok(TerminalPane {
            folder: folder.into(),
            proc: String::new(),
            shell_name,
            last_proc_check: Instant::now() - PROC_POLL,
            parser,
            pending,
            stamp,
            writer,
            master: pair.master,
            child,
            rows,
            cols,
            scroll: 0,
            sel_anchor: None,
            sel_cursor: None,
            copy_mode: false,
            finished_unseen: false,
            #[cfg(test)]
            sent: Vec::new(),
        })
    }

    /// Feed any pending PTY output into the emulator. Returns whether any new
    /// bytes were processed (so callers can decide to redraw).
    pub fn pump(&mut self) -> usize {
        // The reader thread already parsed the bytes into the emulator; we only
        // settle the view and report how many arrived so the caller can redraw.
        let bytes = self.pending.swap(0, Ordering::Acquire) as usize;
        if bytes > 0 {
            // Latency probe: how long did this output wait for a frame? A large
            // value means the UI event loop was parked (the freeze) — log it so a
            // recurrence is captured as hard evidence, not a vague report.
            if let Some(t) = self.stamp.lock().unwrap().take() {
                let lag = t.elapsed();
                if lag >= Duration::from_millis(250) {
                    tracing::warn!(
                        lag_ms = lag.as_millis() as u64,
                        "terminal output waited for a frame (UI loop was parked)"
                    );
                }
            }
            // Keep the view anchored where the user left it (0 = live bottom).
            let mut p = self.parser.lock().unwrap();
            p.screen_mut().set_scrollback(self.scroll);
            self.scroll = p.screen().scrollback();
        }
        self.refresh_foreground();
        bytes
    }

    // ---- scrollback ----------------------------------------------------

    /// Scroll the view by `delta` rows (positive = up into history).
    pub fn scroll_lines(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).max(0) as usize;
        self.set_scroll(next);
        // In copy mode keep the free cursor on screen, so scrolling (PageUp/Dn)
        // and then moving it doesn't snap the view back to where it was.
        if self.copy_mode {
            if let Some((lid, col)) = self.sel_cursor {
                let top = self.scroll + self.rows as usize - 1;
                self.sel_cursor = Some((lid.clamp(self.scroll, top), col));
            }
        }
    }

    /// Scroll by (almost) a full screen; `dir` +1 = up into history, -1 = down.
    pub fn scroll_page(&mut self, dir: i32) {
        let page = (self.rows.saturating_sub(1)).max(1) as i32;
        self.scroll_lines(dir * page);
    }

    /// Jump back to the live bottom of the terminal.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.parser.lock().unwrap().screen_mut().set_scrollback(0);
    }

    /// Current scrollback offset (0 = at the live bottom).
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Whether a full-screen program (vim, less, htop, …) owns the alternate
    /// screen. When it does, PageUp/PageDown belong to that program, not our
    /// scrollback — there's no scrollback on the alt screen anyway.
    pub fn on_alternate_screen(&self) -> bool {
        self.parser.lock().unwrap().screen().alternate_screen()
    }

    // ---- selection (scrollback-aware) ----------------------------------
    //
    // Endpoints are `(line_id, col)` with `line_id` = lines above the live
    // bottom. Moving the cursor past a visible edge auto-scrolls to follow it, so
    // a selection (mouse or keyboard) can run across the whole scrollback.

    /// The line_id of visible `row` at the current scroll offset.
    fn cell_lineid(&self, row: u16) -> usize {
        let r = row.min(self.rows.saturating_sub(1));
        (self.rows - 1 - r) as usize + self.scroll
    }

    /// The visible row currently showing `lineid`, or `None` if it's off-screen.
    fn lineid_row(&self, lineid: usize) -> Option<u16> {
        let rows = self.rows as usize;
        if lineid >= self.scroll && lineid < self.scroll + rows {
            Some((rows - 1 - (lineid - self.scroll)) as u16)
        } else {
            None
        }
    }

    /// A reading-order scalar (top→bottom, left→right increasing) for range tests.
    fn ord(&self, lineid: usize, col: u16) -> i64 {
        ((1i64 << 40) - lineid as i64) * (self.cols as i64 + 1) + col as i64
    }

    /// Set the scrollback offset directly (clamped to the real history depth).
    fn set_scroll(&mut self, s: usize) {
        let mut p = self.parser.lock().unwrap();
        p.screen_mut().set_scrollback(s);
        self.scroll = p.screen().scrollback();
    }

    /// Scroll so `lineid` is on screen; returns it clamped to reachable history.
    fn ensure_visible(&mut self, lineid: usize) -> usize {
        let rows = self.rows as usize;
        if lineid + 1 > self.scroll + rows {
            self.set_scroll(lineid + 1 - rows); // bring it to the top visible row
        } else if lineid < self.scroll {
            self.set_scroll(lineid); // bring it to the bottom visible row
        }
        lineid.min(self.scroll + rows - 1)
    }

    pub fn begin_selection(&mut self, row: u16, col: u16) {
        let lid = self.cell_lineid(row);
        self.sel_anchor = Some((lid, col));
        self.sel_cursor = Some((lid, col));
    }

    pub fn update_selection(&mut self, row: u16, col: u16) {
        if self.sel_anchor.is_none() {
            return;
        }
        // Dragging against an edge auto-scrolls so the selection can keep growing.
        if row == 0 {
            self.scroll_lines(1);
        } else if row + 1 >= self.rows {
            self.scroll_lines(-1);
        }
        let lid = self.cell_lineid(row);
        self.sel_cursor = Some((lid, col));
    }

    /// Extend the selection to visible `(row, col)` **without** moving the
    /// anchor — the Shift+Click endpoint. With no prior selection, the clicked
    /// cell becomes the anchor. Lets you click a start, wheel-scroll the end into
    /// view, then Shift+Click it to grab text spanning the whole scrollback.
    pub fn extend_selection(&mut self, row: u16, col: u16) {
        let lid = self.cell_lineid(row);
        if self.sel_anchor.is_none() {
            self.sel_anchor = self.sel_cursor.or(Some((lid, col)));
        }
        self.sel_cursor = Some((lid, col));
    }

    pub fn clear_selection(&mut self) {
        self.sel_anchor = None;
        self.sel_cursor = None;
    }

    /// Select the whole word under visible cell `(row, col)` — the double-click
    /// gesture.
    pub fn select_word_cell(&mut self, row: u16, col: u16) {
        let lid = self.cell_lineid(row);
        let left = self.word_boundary(row, col, -1);
        let right = self.word_boundary(row, col, 1);
        self.sel_anchor = Some((lid, left));
        self.sel_cursor = Some((lid, right));
    }

    /// Select the entire visible line at `row` — the triple-click gesture.
    pub fn select_line_cell(&mut self, row: u16) {
        let lid = self.cell_lineid(row);
        self.sel_anchor = Some((lid, 0));
        self.sel_cursor = Some((lid, self.cols));
    }

    /// Move the selection cursor to `(lineid, col)`, anchoring/dropping the mark
    /// per `select` and scrolling to keep it visible.
    fn extend(&mut self, lineid: usize, col: u16, select: bool) {
        if select {
            if self.sel_anchor.is_none() {
                self.sel_anchor = self.sel_cursor;
            }
        } else {
            self.sel_anchor = None;
        }
        let lid = self.ensure_visible(lineid);
        self.sel_cursor = Some((lid, col));
    }

    /// One cell step in line_id space (Shift+arrow, copy-mode arrows). `drow` −1 =
    /// up (older), +1 = down; `dcol` wraps to the adjacent line at row edges.
    fn step(&mut self, drow: i32, dcol: i32, select: bool) {
        let cols = self.cols as i32;
        let (lid0, col0) = self.sel_cursor.unwrap_or((self.scroll, 0));
        let mut lid = lid0 as i64;
        let mut c = col0 as i32;
        if dcol != 0 {
            c += dcol;
            if c < 0 {
                lid += 1; // wrap to the end of the older line
                c = cols - 1;
            } else if c >= cols {
                if lid > 0 {
                    lid -= 1; // wrap to the start of the newer line
                    c = 0;
                } else {
                    c = cols - 1;
                }
            }
        }
        lid -= drow as i64; // up (drow = −1) is older = a larger line_id
        let lid = lid.max(0) as usize;
        let c = c.clamp(0, cols - 1) as u16;
        self.extend(lid, c, select);
    }

    /// Move the cursor to the previous/next word boundary on its line.
    fn word_step(&mut self, dir: i32, select: bool) {
        let (lid, col) = self.sel_cursor.unwrap_or((self.scroll, 0));
        if let Some(row) = self.lineid_row(lid) {
            let nc = self.word_boundary(row, col, dir);
            self.extend(lid, nc, select);
        }
    }

    /// Seed the cursor at the shell cursor if there's no selection yet.
    fn start_from_shell_cursor(&mut self) {
        if self.sel_cursor.is_none() {
            let (r, c) = self.parser.lock().unwrap().screen().cursor_position();
            self.sel_cursor = Some((self.cell_lineid(r), c));
        }
    }

    /// Shift+arrow quick-mark (without entering copy mode).
    pub fn select_key(&mut self, drow: i32, dcol: i32) {
        self.start_from_shell_cursor();
        self.step(drow, dcol, true);
    }

    /// Shift+Option+arrow quick word-mark.
    pub fn select_word(&mut self, dir: i32) {
        self.start_from_shell_cursor();
        self.word_step(dir, true);
    }

    /// The column of the word boundary reached by moving from `(row, col)` in
    /// `dir` (words are alphanumeric / underscore runs; everything else is a gap).
    fn word_boundary(&self, row: u16, col: u16, dir: i32) -> u16 {
        let guard = self.parser.lock().unwrap();
        let screen = guard.screen();
        let ch_at = |c: u16| -> char {
            screen
                .cell(row, c)
                .and_then(|cell| cell.contents().chars().next())
                .unwrap_or(' ')
        };
        let is_word = |ch: char| ch.is_alphanumeric() || ch == '_';
        if dir < 0 {
            let mut c = col;
            while c > 0 && !is_word(ch_at(c - 1)) {
                c -= 1;
            }
            while c > 0 && is_word(ch_at(c - 1)) {
                c -= 1;
            }
            c
        } else {
            let mut c = col;
            while c < self.cols && !is_word(ch_at(c)) {
                c += 1;
            }
            while c < self.cols && is_word(ch_at(c)) {
                c += 1;
            }
            c
        }
    }

    // ---- copy mode -----------------------------------------------------

    /// Whether copy mode (free cursor + select, decoupled from the shell) is on.
    pub fn copy_mode(&self) -> bool {
        self.copy_mode
    }

    /// The copy-mode cursor cell `(row, col)` for drawing — `None` unless in copy
    /// mode and the cursor is currently on screen.
    pub fn copy_cursor(&self) -> Option<(u16, u16)> {
        if !self.copy_mode {
            return None;
        }
        let (lid, col) = self.sel_cursor?;
        self.lineid_row(lid).map(|r| (r, col))
    }

    /// Enter copy mode: park a free cursor at the shell cursor, no selection yet.
    pub fn enter_copy_mode(&mut self) {
        self.copy_mode = true;
        let (r, c) = self.parser.lock().unwrap().screen().cursor_position();
        self.sel_cursor = Some((self.cell_lineid(r), c));
        self.sel_anchor = None;
    }

    /// Leave copy mode and drop any selection.
    pub fn exit_copy_mode(&mut self) {
        self.copy_mode = false;
        self.clear_selection();
    }

    /// Move the copy cursor by `(drow, dcol)` (auto-scrolling at edges). With
    /// `select` (Shift) the move marks/extends text; without it, it just moves.
    pub fn copy_move(&mut self, drow: i32, dcol: i32, select: bool) {
        if self.copy_mode {
            self.step(drow, dcol, select);
        }
    }

    /// Move the copy cursor by a word, marking when `select` (Shift) is held.
    pub fn copy_move_word(&mut self, dir: i32, select: bool) {
        if self.copy_mode {
            self.word_step(dir, select);
        }
    }

    /// Whether visible cell `(row, col)` falls inside the current selection
    /// (reading order, end-exclusive) — used to paint the highlight.
    pub fn is_selected(&self, row: u16, col: u16) -> bool {
        let (a, c) = match (self.sel_anchor, self.sel_cursor) {
            (Some(a), Some(c)) if a != c => (a, c),
            _ => return false,
        };
        let pos = self.ord(self.cell_lineid(row), col);
        let (pa, pc) = (self.ord(a.0, a.1), self.ord(c.0, c.1));
        pos >= pa.min(pc) && pos < pa.max(pc)
    }

    /// The selected text, gathered across scrollback (temporarily scrolls to read
    /// each line, then restores the view). `None` when nothing is selected.
    pub fn selection_text(&mut self) -> Option<String> {
        let (a, c) = match (self.sel_anchor, self.sel_cursor) {
            (Some(a), Some(c)) if a != c => (a, c),
            _ => return None,
        };
        // `start` is the earlier cell in reading order (top-left), `end` later.
        let (start, end) = if self.ord(a.0, a.1) <= self.ord(c.0, c.1) {
            (a, c)
        } else {
            (c, a)
        };
        let cols = self.cols;
        let saved = self.scroll;
        let mut out = String::new();
        let mut lid = start.0; // top line has the larger line_id
        loop {
            self.set_scroll(lid); // bring this line onto the bottom visible row
            let Some(r) = self.lineid_row(lid) else { break };
            let (c0, c1) = if lid == start.0 && lid == end.0 {
                (start.1, end.1)
            } else if lid == start.0 {
                (start.1, cols)
            } else if lid == end.0 {
                (0, end.1)
            } else {
                (0, cols)
            };
            let mut line = String::new();
            {
                let guard = self.parser.lock().unwrap();
                let screen = guard.screen();
                for cc in c0..c1 {
                    match screen.cell(r, cc) {
                        Some(cell) => {
                            let s = cell.contents();
                            line.push_str(if s.is_empty() { " " } else { &s });
                        }
                        None => line.push(' '),
                    }
                }
            }
            out.push_str(line.trim_end());
            if lid == end.0 || lid == 0 {
                break;
            }
            out.push('\n');
            lid -= 1;
        }
        self.set_scroll(saved);
        Some(out)
    }

    /// Update the foreground-process label (throttled). Shows the running
    /// command, or nothing when only the shell is at the prompt.
    fn refresh_foreground(&mut self) {
        if self.last_proc_check.elapsed() < PROC_POLL {
            return;
        }
        self.last_proc_check = Instant::now();
        let name = self
            .master
            .process_group_leader()
            .and_then(process_name)
            .unwrap_or_default();
        // Treat the bare shell (login or not, e.g. "-zsh"/"zsh") as idle.
        let trimmed = name.trim_start_matches('-');
        let proc = if trimmed.is_empty() || trimmed == self.shell_name {
            String::new()
        } else {
            trimmed.to_string()
        };
        self.set_proc(proc);
    }

    /// Apply a freshly-detected foreground process name, flagging
    /// finished-unseen on a running→idle edge. Split out from
    /// [`Self::refresh_foreground`] so tests can drive the transition
    /// directly instead of needing a real OS process check.
    fn set_proc(&mut self, proc: String) {
        let was_running = self.is_running();
        self.proc = proc;
        if was_running && !self.is_running() {
            self.finished_unseen = true;
        }
    }

    /// Test-only: drain and return everything sent to the shell since the last
    /// call, so a test can assert exactly what a key/mouse action produced.
    #[cfg(test)]
    pub fn take_sent(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sent)
    }

    /// Whether the running program has asked to receive mouse events (any xterm
    /// mouse mode is active). When true, the GUI forwards clicks/drags/wheel to
    /// it instead of using them for oxru's own selection & scrollback. Holding
    /// Shift bypasses this so local text selection still works.
    pub fn wants_mouse(&self) -> bool {
        self.parser.lock().unwrap().screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
    }

    /// Whether the program reports motion (drag) events, not just press/release.
    pub fn wants_mouse_motion(&self) -> bool {
        use vt100::MouseProtocolMode as M;
        matches!(
            self.parser.lock().unwrap().screen().mouse_protocol_mode(),
            M::ButtonMotion | M::AnyMotion
        )
    }

    /// Report a mouse event to the program in its requested xterm encoding.
    /// `cb` is the button byte (0 left, 1 middle, 2 right, 64/65 wheel up/down,
    /// with +32 OR'd in for motion). `(col,row)` are 0-based terminal cells;
    /// `release` marks a button-up. No-op if the program hasn't enabled mouse
    /// reporting.
    pub fn send_mouse(&mut self, cb: u8, col: u16, row: u16, release: bool) {
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let (mode, enc) = {
            let guard = self.parser.lock().unwrap();
            let screen = guard.screen();
            (screen.mouse_protocol_mode(), screen.mouse_protocol_encoding())
        };
        if mode == M::None {
            return;
        }
        let cx = col as u32 + 1; // xterm mouse coords are 1-based
        let cy = row as u32 + 1;
        let seq: Vec<u8> = match enc {
            E::Sgr => {
                let fin = if release { 'm' } else { 'M' };
                format!("\x1b[<{cb};{cx};{cy}{fin}").into_bytes()
            }
            _ => {
                // Default / UTF-8 X10 encoding: ESC [ M (cb+32)(cx+32)(cy+32).
                // A release reports button 3; coords clamp at the 223 ceiling.
                let b = if release { 3 } else { cb };
                let off = |v: u32| -> u8 { (v.min(223) as u8).wrapping_add(32) };
                vec![0x1b, b'[', b'M', b.wrapping_add(32), off(cx), off(cy)]
            }
        };
        self.send_input(&seq);
    }

    /// Send raw bytes to the shell.
    pub fn send_input(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        self.sent.extend_from_slice(bytes);
        // XOFF (Ctrl+S) suspends the program's output until XON — the classic
        // "terminal froze, a keypress un-froze it" cause. Log it so we can catch it.
        if bytes.contains(&0x13) {
            tracing::warn!("sending XOFF (Ctrl+S) to the shell — this suspends output");
        }
        if let Err(e) = self.writer.write_all(bytes).and_then(|()| self.writer.flush()) {
            tracing::warn!(error = %e, bytes = bytes.len(), "terminal write failed");
        }
    }

    /// Paste `text` into the shell. Snaps to the live bottom first, and — when
    /// the running program has enabled **bracketed paste** mode — wraps the text
    /// in the `ESC[200~ … ESC[201~` markers so it's handled as a paste (no
    /// auto-indent, no executing each newline) instead of typed keystrokes.
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.scroll_to_bottom();
        // Normalize newlines to CR, which is what a terminal paste delivers.
        let body = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.parser.lock().unwrap().screen().bracketed_paste();
        if bracketed {
            self.send_input(b"\x1b[200~");
            self.send_input(body.as_bytes());
            self.send_input(b"\x1b[201~");
        } else {
            self.send_input(body.as_bytes());
        }
    }

    /// Resize the PTY + emulator to fit a render area.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut p = self.parser.lock().unwrap();
        p.screen_mut().set_size(rows, cols);
        // A resize can change the scrollback bounds; keep our offset valid.
        p.screen_mut().set_scrollback(self.scroll);
        self.scroll = p.screen().scrollback();
    }

    pub fn screen(&self) -> ScreenGuard<'_> {
        ScreenGuard(self.parser.lock().unwrap())
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        // Kill the shell when its terminal is closed.
        let pid = self.child.process_id();
        if let Err(e) = self.child.kill() {
            tracing::debug!(?pid, error = %e, "terminal kill on close failed");
        } else {
            tracing::info!(?pid, "terminal closed");
        }
    }
}

/// The short name of process `pid` (macOS via libproc's `proc_name`).
#[cfg(target_os = "macos")]
fn process_name(pid: i32) -> Option<String> {
    unsafe extern "C" {
        fn proc_name(pid: i32, buffer: *mut std::ffi::c_void, buffersize: u32) -> i32;
    }
    if pid <= 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    let n = unsafe { proc_name(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn process_name(_pid: i32) -> Option<String> {
    None
}

/// The live working directory of `pid`, via `lsof` (no stable libproc struct
/// layout to lean on across macOS versions, so we shell out like
/// [`read_git_status`](crate::app) already does for `git`).
#[cfg(target_os = "macos")]
fn process_cwd(pid: i32) -> Option<PathBuf> {
    if pid <= 0 {
        return None;
    }
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
}

#[cfg(not(target_os = "macos"))]
fn process_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn idle_term() -> TerminalPane {
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.cwd(&dir);
        TerminalPane::spawn("t", 24, 80, cmd, None).unwrap()
    }

    #[test]
    fn finished_unseen_starts_false() {
        let term = idle_term();
        assert!(!term.finished_unseen());
    }

    #[test]
    fn finished_unseen_flags_on_running_to_idle_transition() {
        let mut term = idle_term();
        term.set_proc("sleep".to_string());
        assert!(!term.finished_unseen(), "still running, nothing finished yet");
        term.set_proc(String::new());
        assert!(term.finished_unseen(), "sleep -> idle should flag finished-unseen");
    }

    #[test]
    fn finished_unseen_stays_false_while_command_changes_but_keeps_running() {
        let mut term = idle_term();
        term.set_proc("sleep".to_string());
        term.set_proc("vim".to_string());
        assert!(!term.finished_unseen(), "one command replacing another isn't 'finished'");
    }

    #[test]
    fn mark_viewed_clears_finished_unseen() {
        let mut term = idle_term();
        term.set_proc("sleep".to_string());
        term.set_proc(String::new());
        assert!(term.finished_unseen());
        term.mark_viewed();
        assert!(!term.finished_unseen());
    }

    #[test]
    fn captures_command_output() {
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "printf OXRUOK"]);
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            term.pump();
            let text: String = (0..24)
                .flat_map(|r| (0..80).filter_map(move |c| (r, c).into()))
                .filter_map(|(r, c): (u16, u16)| term.screen().cell(r, c).map(|cell| cell.contents().to_string()))
                .collect();
            if text.contains("OXRUOK") {
                found = true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(found, "expected the command output to reach the emulator");
    }

    #[test]
    fn paste_delivers_text_to_shell() {
        // Pasting "printf PASTED\n" should reach the shell and run, just like
        // typing it — newlines normalized to CR so the line executes.
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        term.paste("printf PASTED\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline && !found {
            term.pump();
            let text: String = (0..24)
                .flat_map(|r| (0..80).filter_map(move |c| (r, c).into()))
                .filter_map(|(r, c): (u16, u16)| term.screen().cell(r, c).map(|cell| cell.contents().to_string()))
                .collect();
            if text.contains("PASTED") {
                found = true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(found, "pasted command should reach and run in the shell");
    }

    #[test]
    fn selection_reads_across_scrollback() {
        // Mark text that runs off the top of the screen into scrollback and copy
        // it — the selection must span more than one screen.
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "for i in $(seq 1 60); do echo L$i; done; sleep 5"]);
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = false;
        while Instant::now() < deadline && !got {
            term.pump();
            let screen: String = (0..24)
                .flat_map(|r| (0..80).map(move |c| (r, c)))
                .filter_map(|(r, c)| term.screen().cell(r, c).map(|x| x.contents().to_string()))
                .collect();
            got = screen.contains("L60");
            if !got {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(got, "expected the loop output on screen");

        // Shift+Up far enough to auto-scroll into history while marking.
        term.enter_copy_mode();
        for _ in 0..40 {
            term.copy_move(-1, 0, true);
        }
        let text = term.selection_text().unwrap_or_default();
        let lines = text.lines().filter(|l| l.trim_start().starts_with('L')).count();
        assert!(
            lines >= 5,
            "selection across scrollback should capture many lines, got {lines}: {text:?}"
        );
    }

    #[test]
    fn shift_click_extends_selection_across_scroll() {
        // Simulate: click a start point, wheel-scroll up into history, then
        // Shift+Click a point now on screen. The selection must span the lines
        // scrolled past — proving long-text selection works without dragging.
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "for i in $(seq 1 60); do echo L$i; done; sleep 5"]);
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = false;
        while Instant::now() < deadline && !got {
            term.pump();
            let screen: String = (0..24)
                .flat_map(|r| (0..80).map(move |c| (r, c)))
                .filter_map(|(r, c)| term.screen().cell(r, c).map(|x| x.contents().to_string()))
                .collect();
            got = screen.contains("L60");
            if !got {
                thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(got, "expected the loop output on screen");

        // Click near the bottom (anchor), scroll up a screenful, Shift+Click the
        // top — exactly the click → scroll → shift-click flow in the GUI.
        term.begin_selection(22, 0);
        term.scroll_lines(20);
        term.extend_selection(0, 2);
        let text = term.selection_text().unwrap_or_default();
        let lines = text.lines().filter(|l| l.trim_start().starts_with('L')).count();
        assert!(
            lines >= 5,
            "shift-click after scroll should capture many lines, got {lines}: {text:?}"
        );
    }

    #[test]
    fn detects_mouse_reporting_request() {
        // A program that turns on xterm mouse tracking (1000) with SGR encoding
        // (1006) must flip wants_mouse() — that's what makes the GUI forward the
        // wheel/clicks to it instead of scrolling oxru's own scrollback.
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "printf '\\033[?1000h\\033[?1006h'; sleep 5"]);
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !term.wants_mouse() {
            term.pump();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(term.wants_mouse(), "should detect xterm mouse-mode enable");
    }

    #[test]
    fn display_name_reflects_running_command() {
        // Use a plain `/bin/sh` (not the user's login shell) so the test doesn't
        // depend on a slow/noisy `~/.zprofile`; we only need the foreground-proc
        // label logic, which is shell-agnostic.
        let dir = std::env::temp_dir();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.cwd(&dir);
        let mut term = TerminalPane::spawn("proj", 24, 80, cmd, None).unwrap();

        // Run a long-lived command; the label should pick it up.
        term.send_input(b"sleep 5\n");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ok = false;
        while Instant::now() < deadline {
            term.pump();
            if term.display_name().contains("sleep") {
                ok = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            ok,
            "label should show the running command, got {:?}",
            term.display_name()
        );
    }
}
