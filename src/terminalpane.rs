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

/// Floor between working-directory reads for one pane. Slower than
/// [`PROC_POLL`] because each read forks `lsof`, and it only fires at all after
/// the pane has printed something (see `TerminalPane::cwd_dirty`), so a quiet
/// terminal never pays for it.
const CWD_POLL: Duration = Duration::from_millis(600);

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
    /// Set whenever the pane produces output, cleared once the working
    /// directory has been re-read. A shell that printed something is a shell
    /// that may have just `cd`'d; one sitting silent cannot have moved, so it
    /// costs nothing to leave alone — which matters because reading the cwd
    /// forks `lsof` (see [`process_cwd`]).
    cwd_dirty: bool,
    last_cwd_check: Instant,
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
    /// Rows of history at the last pump, so new output can be measured. See
    /// the scroll-lock handling in [`TerminalPane::pump`].
    history_len: usize,
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

    /// The URL under terminal cell `(row, col)`, if there is one.
    ///
    /// Long URLs almost always wrap, so the logical line is rebuilt first: a row
    /// whose last cell is occupied is treated as continuing into the next one,
    /// which is how a terminal wraps. Without that, clicking a wrapped link
    /// would hand back half of it.
    pub fn url_at(&self, row: u16, col: u16) -> Option<String> {
        let screen = self.screen();
        let (rows, cols) = screen.size();
        if row >= rows || col >= cols {
            return None;
        }
        let text_of = |r: u16| -> String {
            (0..cols)
                .map(|c| {
                    screen
                        .cell(r, c)
                        .map(|cell| {
                            let s = cell.contents();
                            if s.is_empty() { " ".to_string() } else { s.to_string() }
                        })
                        .unwrap_or_else(|| " ".to_string())
                })
                .collect()
        };
        let is_full = |r: u16| -> bool {
            screen.cell(r, cols - 1).map(|c| !c.contents().trim().is_empty()).unwrap_or(false)
        };

        // Walk back to the first row of this wrapped run, then forward to the
        // last, so `at` can be expressed against the joined text.
        let mut first = row;
        while first > 0 && is_full(first - 1) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < rows && is_full(last) {
            last += 1;
        }

        let mut joined = String::new();
        let mut at = 0usize;
        for r in first..=last {
            let t = text_of(r);
            if r == row {
                at = joined.chars().count() + col as usize;
            }
            // Only a *wrapped* row runs straight into the next; a short row
            // ended on its own, so keep them separated or two unrelated lines
            // would glue into one bogus link.
            joined.push_str(t.trim_end());
            if !is_full(r) {
                joined.push(' ');
            }
        }
        url_span_at(&joined, at)
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
                            parser.lock().unwrap_or_else(|e| e.into_inner()).process(&buf[..n]);
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
            // Check once as soon as the shell first prints its prompt.
            cwd_dirty: true,
            last_cwd_check: Instant::now() - CWD_POLL,
            parser,
            pending,
            stamp,
            writer,
            master: pair.master,
            child,
            rows,
            cols,
            scroll: 0,
            history_len: 0,
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
            if let Some(t) = self.stamp.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let lag = t.elapsed();
                if lag >= Duration::from_millis(250) {
                    tracing::warn!(
                        lag_ms = lag.as_millis() as u64,
                        "terminal output waited for a frame (UI loop was parked)"
                    );
                }
            }
            // Scroll lock: hold the *text* still while output streams past.
            //
            // The offset is measured from the live bottom, so simply re-applying
            // it (which is what this did) drags the viewport down one row for
            // every row that arrives — the line you were reading marches off the
            // top and you end up at the bottom within seconds. Holding the
            // offset is not holding the position. Growing it by however much
            // history arrived keeps the same text on screen; at offset 0 the
            // pane is following the bottom and should keep following it.
            let mut p = self.parser.lock().unwrap_or_else(|e| e.into_inner());
            let len = Self::history_len(&mut p);
            let arrived = len.saturating_sub(self.history_len);
            if self.scroll > 0 {
                self.scroll = (self.scroll + arrived).min(len);
            }
            self.history_len = len;
            // A mark is stored the same bottom-relative way, so it needs the
            // same correction — otherwise it stays a fixed distance from the
            // bottom while the bottom moves, and the highlight visibly travels
            // down the screen onto whatever unrelated output has arrived since.
            // Applied whether or not the view is scrolled: at the bottom the
            // marked text is sliding up into history, and the mark has to
            // follow it there.
            if arrived > 0 {
                let ceiling = len + self.rows as usize;
                for (lid, _) in [&mut self.sel_anchor, &mut self.sel_cursor].into_iter().flatten() {
                    *lid = (*lid + arrived).min(ceiling);
                }
            }
            p.screen_mut().set_scrollback(self.scroll);
            self.scroll = p.screen().scrollback();
            self.cwd_dirty = true;
        }
        self.refresh_foreground();
        self.refresh_folder();
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
        self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen_mut().set_scrollback(0);
    }

    /// Current scrollback offset (0 = at the live bottom).
    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Whether a full-screen program (vim, less, htop, …) owns the alternate
    /// screen. When it does, PageUp/PageDown belong to that program, not our
    /// scrollback — there's no scrollback on the alt screen anyway.
    pub fn on_alternate_screen(&self) -> bool {
        self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().alternate_screen()
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

    /// How many rows of history exist right now.
    ///
    /// vt100 has no getter for this, but `set_scrollback` clamps to it — so ask
    /// for an impossible offset and read back what you were given, then restore.
    fn history_len(p: &mut vt100::Parser) -> usize {
        let current = p.screen().scrollback();
        p.screen_mut().set_scrollback(usize::MAX);
        let len = p.screen().scrollback();
        p.screen_mut().set_scrollback(current);
        len
    }

    /// Set the scrollback offset directly (clamped to the real history depth).
    fn set_scroll(&mut self, s: usize) {
        let mut p = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        p.screen_mut().set_scrollback(s);
        self.scroll = p.screen().scrollback();
        // Re-baseline the history measurement at the moment the view moves.
        // The reader thread parses output continuously, so history can have
        // grown since the last pump; without this, that growth is counted as
        // "arrived while scrolled up" and the view jumps by however much had
        // landed *before* the user scrolled.
        self.history_len = Self::history_len(&mut p);
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

    /// Whether a non-empty selection is currently held. A click that produced
    /// a selection was a drag, not a click on a link.
    pub fn selection_active(&self) -> bool {
        matches!((self.sel_anchor, self.sel_cursor), (Some(a), Some(c)) if a != c)
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
            let (r, c) = self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().cursor_position();
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
        let guard = self.parser.lock().unwrap_or_else(|e| e.into_inner());
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
        let (r, c) = self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().cursor_position();
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
                let guard = self.parser.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Re-read the shell's working directory so the tab follows `cd`.
    ///
    /// The label used to be frozen at spawn: `cd`ing to another repo left the
    /// tab still naming the folder the terminal started in, which is exactly
    /// backwards for a tab strip whose whole job is telling you which project
    /// each terminal is in.
    ///
    /// Skipped while a command is running, for two reasons: the process-group
    /// leader is then the command rather than the shell, so its cwd needn't be
    /// the shell's, and a build streaming output would otherwise re-fork `lsof`
    /// on every poll. The label catches up the moment the prompt returns.
    fn refresh_folder(&mut self) {
        if !self.cwd_dirty || self.is_running() {
            return;
        }
        if self.last_cwd_check.elapsed() < CWD_POLL {
            return;
        }
        self.last_cwd_check = Instant::now();
        self.cwd_dirty = false;
        if let Some(name) = self.current_dir().map(|d| crate::app::folder_name(&d)) {
            if name != self.folder {
                self.folder = name;
            }
        }
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
        self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
    }

    /// Whether the program reports motion (drag) events, not just press/release.
    pub fn wants_mouse_motion(&self) -> bool {
        use vt100::MouseProtocolMode as M;
        matches!(
            self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().mouse_protocol_mode(),
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
            let guard = self.parser.lock().unwrap_or_else(|e| e.into_inner());
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
        let bracketed = self.parser.lock().unwrap_or_else(|e| e.into_inner()).screen().bracketed_paste();
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
        let mut p = self.parser.lock().unwrap_or_else(|e| e.into_inner());
        p.screen_mut().set_size(rows, cols);
        // A resize can change the scrollback bounds; keep our offset valid.
        p.screen_mut().set_scrollback(self.scroll);
        self.scroll = p.screen().scrollback();
    }

    pub fn screen(&self) -> ScreenGuard<'_> {
        ScreenGuard(self.parser.lock().unwrap_or_else(|e| e.into_inner()))
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

/// Find the URL covering character index `at` in `line`, if any.
///
/// Only `http://` and `https://` are recognised. That's a security choice, not
/// a shortcut: terminal output is untrusted — any repo you build, any log you
/// tail, can print whatever it likes — and honouring schemes like `file://`,
/// `javascript:` or a custom app handler would turn "click a link" into "run
/// something the output chose". A confirmation prompt guards the click; the
/// scheme allow-list guards what a confirmation can even be for.
pub fn url_span_at(line: &str, at: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    for (start, _) in line.char_indices() {
        // Work in char positions, not byte offsets, so a line with any
        // non-ASCII in it doesn't shift every URL's bounds.
        let cstart = line[..start].chars().count();
        if !starts_scheme(&chars[cstart..]) {
            continue;
        }
        let mut end = cstart;
        while end < chars.len() && !is_url_terminator(chars[end]) {
            end += 1;
        }
        let end = trim_trailing(&chars[cstart..end]) + cstart;
        if at >= cstart && at < end && end - cstart > SCHEME_MIN {
            return Some(chars[cstart..end].iter().collect());
        }
    }
    None
}

/// Shortest thing that could still be a URL: `http://` plus one character.
const SCHEME_MIN: usize = 8;

fn starts_scheme(rest: &[char]) -> bool {
    let s: String = rest.iter().take(8).collect();
    s.starts_with("http://") || s.starts_with("https://")
}

/// A URL ends at whitespace, or at a character a terminal would never carry
/// inside one (quotes and angle brackets, which shells and logs wrap URLs in).
fn is_url_terminator(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|')
}

/// Drop trailing punctuation that belongs to the surrounding prose rather than
/// the URL — "see https://example.com/x." ends in a full stop, not a path.
/// A closing bracket is only dropped when it has no opener inside the URL, so
/// a real `…/Foo_(bar)` link survives.
fn trim_trailing(url: &[char]) -> usize {
    let mut end = url.len();
    while end > 0 {
        let c = url[end - 1];
        let drop = match c {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => url[..end - 1].iter().filter(|&&c| c == '(').count()
                <= url[..end - 1].iter().filter(|&&c| c == ')').count(),
            ']' => url[..end - 1].iter().filter(|&&c| c == '[').count()
                <= url[..end - 1].iter().filter(|&&c| c == ']').count(),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= 1;
    }
    end
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

    /// The tab has to follow `cd`. It used to name the folder the terminal was
    /// spawned in, forever.
    #[test]
    fn the_folder_label_follows_the_shells_working_directory() {
        // A real shell, told to cd somewhere else, then left at a prompt.
        let start = std::env::temp_dir();
        let target = start.join(format!("oxru-cwd-test-{}", std::process::id()));
        std::fs::create_dir_all(&target).unwrap();

        // The user's real shell, as `App::new_terminal` spawns: the pane
        // recognises `$SHELL` as "idle", and anything else as a running
        // command — which would keep the cwd read permanently deferred.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(&start);
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();
        assert_eq!(term.folder, "t", "starts with the label it was given");

        term.send_input(format!("cd '{}'\n", target.display()).as_bytes());

        // Pump until the label catches up (the read is throttled and only
        // happens once the shell has printed its prompt back).
        let expect = target.file_name().unwrap().to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && term.folder != expect {
            term.pump();
            std::thread::sleep(Duration::from_millis(50));
        }
        std::fs::remove_dir_all(&target).ok();

        // `process_cwd` is a no-op off macOS, where there's nothing to assert.
        if cfg!(target_os = "macos") {
            assert_eq!(term.folder, expect, "the tab should name where the shell now is");
        }
    }

    /// The read forks `lsof`, so it must not fire for a terminal that hasn't
    /// printed anything, nor while a command is running.
    #[test]
    fn the_working_directory_is_only_re_read_when_it_could_have_changed() {
        let mut term = idle_term();

        term.cwd_dirty = false;
        term.last_cwd_check = Instant::now() - CWD_POLL * 2;
        term.refresh_folder();
        assert!(!term.cwd_dirty, "no output since the last read: nothing to do");

        // Fresh output while a command runs: still skipped, so a build
        // streaming output can't re-fork `lsof` on every poll.
        term.cwd_dirty = true;
        term.set_proc("cargo".to_string());
        term.refresh_folder();
        assert!(term.cwd_dirty, "deferred while a command holds the terminal");

        // Back at the prompt: now it reads, and clears the flag.
        term.set_proc(String::new());
        term.refresh_folder();
        assert!(!term.cwd_dirty, "read once the shell is idle again");
    }

    #[test]
    fn finds_the_url_under_the_clicked_column() {
        let line = "see https://example.com/a/b for details";
        let at = line.find("example").unwrap();
        assert_eq!(url_span_at(line, at).as_deref(), Some("https://example.com/a/b"));
        // Clicking outside the span finds nothing.
        assert_eq!(url_span_at(line, 0), None);
        assert_eq!(url_span_at(line, line.chars().count() - 1), None);
    }

    #[test]
    fn trailing_prose_punctuation_is_not_part_of_the_link() {
        for (line, want) in [
            ("go to https://example.com/x.", "https://example.com/x"),
            ("https://example.com/x, then", "https://example.com/x"),
            ("(https://example.com/x)", "https://example.com/x"),
            ("[https://example.com/x]", "https://example.com/x"),
        ] {
            let at = line.find("http").unwrap() + 10;
            assert_eq!(url_span_at(line, at).as_deref(), Some(want), "{line}");
        }
    }

    #[test]
    fn brackets_that_belong_to_the_url_survive() {
        // A wiki-style link really does end in ')'.
        let line = "https://en.wikipedia.org/wiki/Foo_(bar)";
        assert_eq!(url_span_at(line, 10).as_deref(), Some(line));
    }

    #[test]
    fn quotes_and_angle_brackets_delimit_it() {
        for line in ["\"https://example.com/x\"", "<https://example.com/x>"] {
            assert_eq!(url_span_at(line, 10).as_deref(), Some("https://example.com/x"), "{line}");
        }
    }

    /// Terminal output is untrusted: anything you build or tail can print a
    /// line. Only http(s) is offered, so a click can never be turned into
    /// "open this local file" or "hand this to some app's URL handler".
    #[test]
    fn only_http_and_https_are_recognised() {
        for line in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "vscode://file/etc/passwd",
            "ftp://example.com/x",
            "data:text/html;base64,AAAA",
        ] {
            assert_eq!(url_span_at(line, 6), None, "{line} must not be offered");
        }
    }

    #[test]
    fn a_line_with_two_urls_picks_the_one_clicked() {
        let line = "https://one.example/a  https://two.example/b";
        let first = line.find("one").unwrap();
        let second = line.find("two").unwrap();
        assert_eq!(url_span_at(line, first).as_deref(), Some("https://one.example/a"));
        assert_eq!(url_span_at(line, second).as_deref(), Some("https://two.example/b"));
        // The gap between them isn't a link.
        assert_eq!(url_span_at(line, line.find("  ").unwrap()), None);
    }

    #[test]
    fn non_ascii_earlier_in_the_line_does_not_shift_the_span() {
        // Byte offsets and char positions diverge here; the scanner works in
        // chars, which is what a click's column is.
        let line = "→ ✓ https://example.com/x done";
        let at = line.chars().position(|c| c == 'e').unwrap();
        assert_eq!(url_span_at(line, at).as_deref(), Some("https://example.com/x"));
    }

    /// A broad table of real-world lines, each clicked at *every* column: the
    /// answer must be the same wherever inside a URL you click, and `None`
    /// everywhere outside it. The per-case tests above pin individual rules;
    /// this pins them all holding at once, which is what actually clicking
    /// around in a terminal exercises.
    #[test]
    fn every_column_of_a_line_resolves_consistently() {
        let cases: &[(&str, Option<&str>)] = &[
            ("https://example.com", Some("https://example.com")),
            ("http://example.com/a/b/c", Some("http://example.com/a/b/c")),
            ("https://example.com/a?x=1&y=2#f", Some("https://example.com/a?x=1&y=2#f")),
            ("Docs live at https://example.com/guide.", Some("https://example.com/guide")),
            ("See https://example.com/guide, then run.", Some("https://example.com/guide")),
            ("Ends in a colon: https://example.com/x:", Some("https://example.com/x")),
            ("(https://example.com/inside-parens)", Some("https://example.com/inside-parens")),
            ("[https://example.com/inside-brackets]", Some("https://example.com/inside-brackets")),
            ("\"https://example.com/inside-quotes\"", Some("https://example.com/inside-quotes")),
            ("<https://example.com/inside-angles>", Some("https://example.com/inside-angles")),
            ("https://en.wikipedia.org/wiki/Rust_(language)", Some("https://en.wikipedia.org/wiki/Rust_(language)")),
            ("ERROR  build failed, see https://example.com/logs/42 for output", Some("https://example.com/logs/42")),
            ("→ ✓ https://example.com/after-non-ascii is still correct", Some("https://example.com/after-non-ascii")),
            ("file:///etc/passwd", None),
            ("javascript:alert(document.cookie)", None),
            ("vscode://file/etc/passwd", None),
            ("ftp://example.com/pub/file.txt", None),
            ("data:text/html;base64,PHNjcmlwdD4=", None),
            ("example.com/no-scheme", None),
            ("ssh://git@example.com/repo.git", None),
            ("plain text with no link at all", None),
        ];
        let mut bad = 0;
        for (line, want) in cases {
            // Click every column; the span must be `want` inside it, None outside.
            let hits: Vec<String> = (0..line.chars().count())
                .filter_map(|i| url_span_at(line, i))
                .collect();
            let got = hits.first().map(String::as_str);
            let uniform = hits.iter().all(|h| Some(h.as_str()) == got);
            if got != *want || !uniform {
                println!("MISMATCH {line:?}\n   want {want:?}  got {got:?} uniform={uniform}");
                bad += 1;
            }
        }
        assert_eq!(bad, 0, "{bad} lines didn't resolve as expected");
    }

    /// Scrolled up, the text must stay put while output streams past — the
    /// whole point of scrolling up in a log. It used to slide one row per
    /// arriving row, so a busy terminal dragged you back to the bottom within
    /// seconds.
    #[test]
    fn scrolling_up_holds_the_text_still_while_output_arrives() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(std::env::temp_dir());
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();

        // A block of numbered lines to scroll back into.
        term.send_input(b"for i in $(seq 1 60); do echo \"LINE-$i\"; done\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !screen_contains(&term, "LINE-60") {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(screen_contains(&term, "LINE-60"), "seed output never arrived");

        // Scroll up into the history and remember exactly what's on screen.
        term.scroll_lines(15);
        assert!(term.scroll_offset() > 0, "should be scrolled up");
        let before = visible_text(&term);
        let offset_before = term.scroll_offset();

        // Now stream a lot more output past it.
        term.send_input(b"for i in $(seq 1 80); do echo \"NOISE-$i\"; done\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(
            visible_text(&term),
            before,
            "the view moved while the user was scrolled up"
        );
        assert!(
            term.scroll_offset() > offset_before,
            "the offset must grow as history arrives, or the text can't have held still"
        );
    }

    /// …and at the bottom it must still follow, or the terminal would freeze.
    #[test]
    fn at_the_bottom_the_view_keeps_following_new_output() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(std::env::temp_dir());
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();
        term.send_input(b"echo FIRST-MARKER\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !screen_contains(&term, "FIRST-MARKER") {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(term.scroll_offset(), 0, "starts at the bottom");

        term.send_input(b"echo SECOND-MARKER\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !screen_contains(&term, "SECOND-MARKER") {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(screen_contains(&term, "SECOND-MARKER"), "following the bottom");
        assert_eq!(term.scroll_offset(), 0, "and still pinned there");
    }

    fn visible_text(t: &TerminalPane) -> String {
        let screen = t.screen();
        let (rows, cols) = screen.size();
        (0..rows)
            .map(|r| {
                (0..cols)
                    .map(|c| screen.cell(r, c).map(|x| x.contents()).unwrap_or_default())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen_contains(t: &TerminalPane, needle: &str) -> bool {
        visible_text(t).contains(needle)
    }

    /// Marking text while scrolled up must not restart the auto-scroll.
    #[test]
    fn a_selection_does_not_break_scroll_lock() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(std::env::temp_dir());
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();
        term.send_input(b"for i in $(seq 1 60); do echo \"LINE-$i\"; done\n");
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }

        term.scroll_lines(15);
        // Mark a couple of rows, the way a mouse drag does.
        term.begin_selection(5, 0);
        term.update_selection(7, 20);
        let before = visible_text(&term);
        assert!(term.selection_text().is_some(), "something is selected");
        let after_copy = visible_text(&term);
        assert_eq!(after_copy, before, "reading the selection moved the view");

        term.send_input(b"for i in $(seq 1 80); do echo \"NOISE-$i\"; done\n");
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            visible_text(&term),
            before,
            "the view scrolled while text was selected and output arrived"
        );
    }

    /// A mark must stay on the text it was made on. Both the mark and the
    /// scroll offset are stored as a distance from the live bottom, so output
    /// arriving moves them both off their content — the highlight slides down
    /// the screen onto unrelated output, which reads as the terminal scrolling
    /// even though the view itself is holding still.
    #[test]
    fn a_mark_stays_on_the_text_it_was_made_on() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(std::env::temp_dir());
        let mut term = TerminalPane::spawn("t", 24, 80, cmd, None).unwrap();
        term.send_input(b"for i in $(seq 1 60); do echo \"LINE-$i\"; done\n");
        let d = Instant::now() + Duration::from_secs(4);
        while Instant::now() < d {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }

        term.scroll_lines(15);
        term.begin_selection(5, 0);
        term.update_selection(5, 30);
        let before = term.selection_text().unwrap_or_default();
        assert!(before.starts_with("LINE-"), "seeded on a known line, got {before:?}");

        term.send_input(b"for i in $(seq 1 80); do echo \"NOISE-$i\"; done\n");
        let d = Instant::now() + Duration::from_secs(4);
        while Instant::now() < d {
            term.pump();
            std::thread::sleep(Duration::from_millis(20));
        }

        assert_eq!(
            term.selection_text().unwrap_or_default(),
            before,
            "the mark drifted onto different text as output arrived"
        );
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
