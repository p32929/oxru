//! Text buffer backed by a `ropey::Rope`.
//!
//! The cursor is stored as a single character index into the rope, which keeps
//! editing operations simple; row/column are derived on demand. A `revision`
//! counter lets the syntax layer cache highlight results and recompute only
//! when the text actually changes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use ropey::Rope;

use crate::syntax::Lang;

/// The line-ending convention detected in the file this buffer was loaded
/// from (checked once at load, not re-derived every render — nothing in this
/// editor normalizes endings on save, so it stays accurate for the buffer's
/// lifetime). Shown in the status bar so a stray CRLF file is obvious before
/// you `git diff` it and get a wall of whole-file changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            Self::Crlf
        } else {
            Self::Lf
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
        }
    }
}

/// How the file on disk compares to what this buffer last loaded or saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskStatus {
    /// Same as we last read/wrote (or we have no baseline to compare against).
    Unchanged,
    /// The file's contents changed underneath us (different mtime or length).
    Modified,
    /// The file is gone (deleted or moved).
    Deleted,
}

/// The kind of the last edit, so runs of typing (or deleting) coalesce into a
/// single undo step instead of one-per-character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
}

/// One caret in the buffer. The buffer always has a primary caret (stored
/// directly in `Buffer::cursor`/`anchor`/`goal_col`); multi-cursor editing adds
/// *secondary* carets in `Buffer::carets`, each with its own selection.
#[derive(Debug, Clone, Copy)]
pub struct Caret {
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub goal_col: usize,
}

impl Caret {
    /// The caret's selection as `(start, end)` — an empty `(pos, pos)` when there
    /// is no selection.
    pub fn bounds(&self) -> (usize, usize) {
        match self.anchor {
            Some(a) if a != self.cursor => (a.min(self.cursor), a.max(self.cursor)),
            _ => (self.cursor, self.cursor),
        }
    }

    fn lower(&self) -> usize {
        self.bounds().0
    }
}

/// A point-in-time copy of the editable state, for undo/redo. `Rope::clone` is
/// cheap (the underlying tree is shared copy-on-write), so snapshots are light.
#[derive(Debug, Clone)]
struct Snapshot {
    rope: Rope,
    cursor: usize,
    anchor: Option<usize>,
    carets: Vec<Caret>,
}

/// How many undo steps to keep.
const UNDO_CAP: usize = 1000;

#[derive(Debug)]
pub struct Buffer {
    /// Stable id (assigned by the app), so per-buffer caches (syntax highlight)
    /// survive tab reordering and grid layouts.
    pub id: u64,
    pub path: Option<PathBuf>,
    pub rope: Rope,
    /// The content as last saved to (or loaded from) disk. The buffer is
    /// "modified" exactly when `rope` differs from this — so editing then undoing
    /// back to the saved text clears the dirty flag, like every real editor.
    saved: Rope,
    /// Cursor position as a character index into the rope.
    pub cursor: usize,
    /// Selection anchor: the fixed end of a selection. `None` means no
    /// selection; the live end is always `cursor`.
    pub anchor: Option<usize>,
    /// Secondary carets for multi-cursor editing (the primary is the fields
    /// above). Empty in the common single-cursor case; kept sorted by `cursor`
    /// and free of duplicates. Every edit/movement is applied to these too.
    carets: Vec<Caret>,
    /// Preferred column for vertical movement (sticky column).
    pub goal_col: usize,
    /// First visible row (vertical scroll offset).
    pub scroll_row: usize,
    /// True when there are unsaved edits.
    pub modified: bool,
    pub lang: Option<&'static Lang>,
    pub line_ending: LineEnding,
    /// Disk metadata `(modified-time, byte-length)` as of the last load/save, so
    /// we can detect when something else changes the file. `None` = no file yet
    /// (or we've intentionally stopped tracking it).
    disk: Option<(SystemTime, u64)>,
    revision: u64,
    /// Undo history (newest last) and the redo stack.
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Kind + resulting cursor of the last edit, for coalescing.
    last_kind: Option<EditKind>,
    last_edit_cursor: Option<usize>,
}

impl Buffer {
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let line_ending = LineEnding::detect(&text);
        let rope = Rope::from_str(&text);
        Ok(Buffer {
            id: 0,
            path: Some(path.to_path_buf()),
            saved: rope.clone(),
            rope,
            cursor: 0,
            anchor: None,
            carets: Vec::new(),
            goal_col: 0,
            scroll_row: 0,
            modified: false,
            lang: Lang::detect(path),
            line_ending,
            disk: Self::disk_meta(path),
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_kind: None,
            last_edit_cursor: None,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The file name shown on the tab.
    pub fn name(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
            None => "[new]".to_string(),
        }
    }

    /// Write the buffer back to its file, clearing the modified flag.
    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().context("buffer has no path to save to")?;
        let mut writer = std::io::BufWriter::new(
            fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?,
        );
        self.rope.write_to(&mut writer)?;
        use std::io::Write;
        writer.flush()?;
        drop(writer);
        // This is the new on-disk content; the buffer is now clean. Re-baseline
        // the disk snapshot so our own write isn't seen as an external change.
        self.saved = self.rope.clone();
        self.modified = false;
        self.disk = Self::disk_meta(&path);
        Ok(())
    }

    /// Read a file's `(modified-time, length)`, or `None` if it can't be stat'd.
    fn disk_meta(path: &Path) -> Option<(SystemTime, u64)> {
        let m = fs::metadata(path).ok()?;
        Some((m.modified().ok()?, m.len()))
    }

    /// How the file on disk now compares to what we last loaded/saved. Cheap: a
    /// single `stat`, no read. `Unchanged` when there's no path or baseline.
    pub fn disk_status(&self) -> DiskStatus {
        let Some(path) = &self.path else {
            return DiskStatus::Unchanged;
        };
        let Some(recorded) = self.disk else {
            return DiskStatus::Unchanged;
        };
        match Self::disk_meta(path) {
            Some(cur) if cur == recorded => DiskStatus::Unchanged,
            Some(_) => DiskStatus::Modified,
            None => DiskStatus::Deleted,
        }
    }

    /// Re-read the file from disk, replacing the buffer's contents. Keeps the
    /// cursor/scroll where they were (clamped to the new bounds), resets the
    /// saved baseline and dirty flag, and drops undo history (the text is no
    /// longer ours to undo). Bumps the revision so syntax re-highlights.
    pub fn reload_from_disk(&mut self) -> Result<()> {
        let path = self.path.clone().context("buffer has no path to reload")?;
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let rope = Rope::from_str(&text);
        let max_char = rope.len_chars();
        let max_row = rope.len_lines().saturating_sub(1);
        self.rope = rope.clone();
        self.saved = rope;
        self.cursor = self.cursor.min(max_char);
        self.anchor = None;
        self.carets.clear();
        self.scroll_row = self.scroll_row.min(max_row);
        self.modified = false;
        self.undo.clear();
        self.redo.clear();
        self.last_kind = None;
        self.last_edit_cursor = None;
        self.revision = self.revision.wrapping_add(1);
        self.disk = Self::disk_meta(&path);
        Ok(())
    }

    /// Accept the current on-disk state as the new baseline *without* changing
    /// the buffer — "keep my version", so we stop re-prompting about this change.
    pub fn rebaseline_disk(&mut self) {
        if let Some(path) = &self.path {
            self.disk = Self::disk_meta(path);
        }
    }

    /// Stop tracking the file on disk (used after we've reported a deletion, so
    /// we don't keep re-reporting it every poll).
    pub fn forget_disk(&mut self) {
        self.disk = None;
    }

    /// Whether the buffer matches its last-saved content (cheap: lengths first,
    /// then a content compare only when they're equal).
    fn is_clean(&self) -> bool {
        self.rope.len_bytes() == self.saved.len_bytes() && self.rope == self.saved
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Number of characters on `line`, excluding the trailing newline.
    pub fn line_len_chars(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return 0;
        }
        let slice = self.rope.line(line);
        let mut len = slice.len_chars();
        if len > 0 && slice.char(len - 1) == '\n' {
            len -= 1;
            if len > 0 && slice.char(len - 1) == '\r' {
                len -= 1;
            }
        }
        len
    }

    pub fn cursor_row(&self) -> usize {
        self.rope.char_to_line(self.cursor)
    }

    pub fn cursor_col(&self) -> usize {
        let line = self.cursor_row();
        self.cursor - self.rope.line_to_char(line)
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        // Re-derive dirtiness from the content so undoing back to the saved text
        // (or deleting an edit) clears the flag instead of staying "unsaved".
        self.modified = !self.is_clean();
    }

    // ---- undo / redo ---------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            rope: self.rope.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            carets: self.carets.clone(),
        }
    }

    /// Record a pre-edit checkpoint. Consecutive edits of the same `kind` that
    /// pick up where the last one left off coalesce into one undo step (so a run
    /// of typing undoes together); a different kind or a moved cursor starts a
    /// new step. Always clears the redo stack.
    fn checkpoint(&mut self, kind: EditKind) {
        let coalesce = !self.undo.is_empty()
            && self.last_kind == Some(kind)
            && self.last_edit_cursor == Some(self.cursor);
        if !coalesce {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_CAP {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
    }

    /// Note the cursor/kind after an edit, so the next edit can decide to merge.
    fn note_edit(&mut self, kind: EditKind) {
        self.last_kind = Some(kind);
        self.last_edit_cursor = Some(self.cursor);
    }

    fn restore(&mut self, s: Snapshot) {
        self.rope = s.rope;
        self.cursor = s.cursor.min(self.rope.len_chars());
        self.anchor = s.anchor;
        self.carets = s.carets;
        self.goal_col = self.cursor_col();
        self.last_kind = None;
        self.last_edit_cursor = None;
        self.bump();
    }

    /// Undo the most recent edit (or run of edits). Returns whether anything was
    /// undone.
    pub fn undo(&mut self) -> bool {
        if let Some(prev) = self.undo.pop() {
            let cur = self.snapshot();
            self.restore(prev);
            self.redo.push(cur);
            true
        } else {
            false
        }
    }

    /// Redo the last undone edit. Returns whether anything was redone.
    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo.pop() {
            let cur = self.snapshot();
            self.restore(next);
            self.undo.push(cur);
            true
        } else {
            false
        }
    }

    // ---- multi-cursor --------------------------------------------------

    /// Whether any secondary carets are active.
    pub fn has_extra_carets(&self) -> bool {
        !self.carets.is_empty()
    }

    /// Total number of carets (primary + secondaries).
    pub fn caret_count(&self) -> usize {
        1 + self.carets.len()
    }

    /// The secondary carets, for rendering their cursors / selections.
    pub fn extra_carets(&self) -> &[Caret] {
        &self.carets
    }

    /// Collapse back to the single primary caret (Esc). Returns whether anything
    /// was removed.
    pub fn clear_extra_carets(&mut self) -> bool {
        let had = !self.carets.is_empty();
        self.carets.clear();
        had
    }

    /// The primary caret as a `Caret`.
    fn this_caret(&self) -> Caret {
        Caret { cursor: self.cursor, anchor: self.anchor, goal_col: self.goal_col }
    }

    /// Make `c` the primary caret (writes the primary fields).
    fn load_caret(&mut self, c: Caret) {
        self.cursor = c.cursor;
        self.anchor = c.anchor;
        self.goal_col = c.goal_col;
    }

    /// Column (chars from line start) of an arbitrary char index.
    fn col_of(&self, pos: usize) -> usize {
        let row = self.rope.char_to_line(pos);
        pos - self.rope.line_to_char(row)
    }

    /// All carets paired with a flag marking the primary one. Order: primary
    /// first, then the secondaries.
    fn carets_flagged(&self) -> Vec<(Caret, bool)> {
        let mut v = Vec::with_capacity(1 + self.carets.len());
        v.push((self.this_caret(), true));
        v.extend(self.carets.iter().map(|c| (*c, false)));
        v
    }

    /// Re-seat the carets from a `(caret, is_primary)` list: sort, merge any that
    /// landed on the same position (keeping the primary flag), then split the
    /// primary back into the dedicated fields.
    fn install_carets(&mut self, mut carets: Vec<(Caret, bool)>) {
        carets.sort_by_key(|(c, _)| c.cursor);
        let mut deduped: Vec<(Caret, bool)> = Vec::with_capacity(carets.len());
        for (c, p) in carets {
            if let Some(last) = deduped.last_mut() {
                if last.0.cursor == c.cursor {
                    last.1 = last.1 || p;
                    continue;
                }
            }
            deduped.push((c, p));
        }
        let pi = deduped.iter().position(|(_, p)| *p).unwrap_or(0);
        let (prim, _) = deduped.remove(pi);
        self.load_caret(prim);
        self.carets = deduped.into_iter().map(|(c, _)| c).collect();
    }

    /// Apply a pure (text-preserving) movement `op` to every caret, then merge
    /// any that collided. Used to make all the `move_*` methods multi-aware for
    /// free.
    fn each_caret_move(&mut self, op: fn(&mut Buffer)) {
        if self.carets.is_empty() {
            op(self);
            return;
        }
        let mut out: Vec<(Caret, bool)> = Vec::with_capacity(1 + self.carets.len());
        op(self);
        out.push((self.this_caret(), true));
        let secs = std::mem::take(&mut self.carets);
        for c in secs {
            self.load_caret(c);
            op(self);
            out.push((self.this_caret(), false));
        }
        self.install_carets(out);
    }

    /// Apply a text edit at every caret in one undoable step. `plan(buf, caret)`
    /// returns `(remove_start, remove_end, insert_text)` in the *original*
    /// coordinate space (or `None` to leave that caret's text alone). Edits are
    /// applied left-to-right with a running offset so later carets stay aligned,
    /// and never overlap. Returns whether any edit happened.
    fn apply_multi_edit(
        &mut self,
        kind: EditKind,
        plan: impl Fn(&Buffer, &Caret) -> Option<(usize, usize, String)>,
    ) -> bool {
        let mut all = self.carets_flagged();
        all.sort_by_key(|(c, _)| c.lower());
        // Compute every edit against the pristine rope before mutating it.
        let plans: Vec<Option<(usize, usize, String)>> =
            all.iter().map(|(c, _)| plan(self, c)).collect();
        if plans.iter().all(Option::is_none) {
            return false;
        }
        self.checkpoint(kind);
        let mut shift = 0isize;
        let mut guard = 0usize; // high-water mark (new coords) so edits never overlap
        let mut out: Vec<(Caret, bool)> = Vec::with_capacity(all.len());
        for ((caret, primary), plan) in all.into_iter().zip(plans) {
            match plan {
                Some((rs, re, ins)) => {
                    let a = (((rs as isize) + shift).max(0) as usize).max(guard);
                    let mut b = (((re as isize) + shift).max(0) as usize).max(a);
                    b = b.min(self.rope.len_chars());
                    if b > a {
                        self.rope.remove(a..b);
                    }
                    if !ins.is_empty() {
                        self.rope.insert(a, &ins);
                    }
                    let inslen = ins.chars().count();
                    let newpos = a + inslen;
                    shift += inslen as isize - (b as isize - a as isize);
                    guard = newpos;
                    out.push((Caret { cursor: newpos, anchor: None, goal_col: 0 }, primary));
                }
                None => {
                    let np = (((caret.cursor as isize) + shift).max(0) as usize).max(guard);
                    out.push((Caret { cursor: np, anchor: None, goal_col: 0 }, primary));
                }
            }
        }
        for (c, _) in &mut out {
            c.goal_col = self.col_of(c.cursor);
        }
        self.install_carets(out);
        self.note_edit(kind);
        self.bump();
        true
    }

    /// Make a brand-new caret (selection `start..end`, cursor at `end`) the
    /// primary, demoting the old primary to a secondary. Drives the viewport to
    /// follow the newest caret, like VS Code's Cmd+D.
    fn promote_new_caret(&mut self, start: Option<usize>, end: usize, goal: usize) {
        let old = self.this_caret();
        self.carets.push(old);
        self.cursor = end;
        self.anchor = start;
        self.goal_col = goal;
        // Drop any secondary that now coincides with the new primary.
        self.carets.retain(|c| c.cursor != end);
    }

    /// Drop a new caret at char position `pos` (column `goal`), making it the
    /// primary and demoting the old primary to a secondary — the mouse
    /// equivalent of Cmd+Alt+↑/↓ (Alt+Click).
    pub fn add_caret_at(&mut self, pos: usize, goal: usize) {
        self.promote_new_caret(None, pos, goal);
    }

    /// Add a caret one line below the lowest caret, at the same column (Cmd+Alt+↓).
    pub fn add_caret_below(&mut self) {
        let base = self
            .carets_flagged()
            .into_iter()
            .map(|(c, _)| c)
            .max_by_key(|c| c.cursor)
            .unwrap_or_else(|| self.this_caret());
        let row = self.rope.char_to_line(base.cursor);
        if row + 1 >= self.rope.len_lines() {
            return;
        }
        let col = base.goal_col.max(self.col_of(base.cursor));
        let target = row + 1;
        let pos = self.rope.line_to_char(target) + col.min(self.line_len_chars(target));
        self.promote_new_caret(None, pos, col);
    }

    /// Add a caret one line above the highest caret, at the same column (Cmd+Alt+↑).
    pub fn add_caret_above(&mut self) {
        let base = self
            .carets_flagged()
            .into_iter()
            .map(|(c, _)| c)
            .min_by_key(|c| c.cursor)
            .unwrap_or_else(|| self.this_caret());
        let row = self.rope.char_to_line(base.cursor);
        if row == 0 {
            return;
        }
        let col = base.goal_col.max(self.col_of(base.cursor));
        let target = row - 1;
        let pos = self.rope.line_to_char(target) + col.min(self.line_len_chars(target));
        self.promote_new_caret(None, pos, col);
    }

    /// The primary caret's selected text (not the multi-caret join).
    fn primary_selection_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        Some(self.rope.slice(s..e).to_string())
    }

    /// Cmd+D: select the next occurrence of the current selection, adding a caret
    /// there. With no selection yet, the first press selects the word under the
    /// cursor. Returns whether something was selected/added.
    pub fn add_next_occurrence(&mut self) -> bool {
        // First press with nothing selected: select the word under the cursor.
        if self.carets.is_empty() && self.selection().is_none() {
            let n = self.rope.len_chars();
            if n == 0 {
                return false;
            }
            let i = self.cursor.min(n - 1);
            if !is_word_char(self.rope.char(i)) {
                return false;
            }
            self.select_word_at(i);
            return self.selection().is_some();
        }
        let needle = match self.primary_selection_text() {
            Some(t) if !t.is_empty() => t,
            _ => return false,
        };
        let nlen = needle.chars().count();
        // Ranges already covered by a caret — skip them when searching.
        let taken: Vec<(usize, usize)> = self
            .carets_flagged()
            .iter()
            .map(|(c, _)| c.bounds())
            .collect();
        let from = taken.iter().map(|(_, e)| *e).max().unwrap_or(self.cursor);
        let Some(start) = self.find_from(&needle, from, &taken) else {
            return false;
        };
        let end = start + nlen;
        let goal = self.col_of(end);
        self.promote_new_caret(Some(start), end, goal);
        true
    }

    /// Find `needle` starting at char index `from`, wrapping around once, skipping
    /// any range already in `taken`. Returns the match start.
    fn find_from(&self, needle: &str, from: usize, taken: &[(usize, usize)]) -> Option<usize> {
        let hay: Vec<char> = self.rope.chars().collect();
        let nee: Vec<char> = needle.chars().collect();
        let n = hay.len();
        if nee.is_empty() || nee.len() > n {
            return None;
        }
        for off in 0..n {
            let start = (from + off) % n;
            if start + nee.len() > n {
                continue;
            }
            if hay[start..start + nee.len()] == nee[..]
                && !taken.iter().any(|&(s, _)| s == start)
            {
                return Some(start);
            }
        }
        None
    }

    // ---- selection -----------------------------------------------------

    /// The selected character range as `(start, end)`, or `None` when empty.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.cursor {
            return None;
        }
        Some((a.min(self.cursor), a.max(self.cursor)))
    }

    /// The selected text, or `None` when nothing is selected. With multiple
    /// carets, each caret's selection is joined top-to-bottom with newlines.
    pub fn selected_text(&self) -> Option<String> {
        if self.carets.is_empty() {
            let (s, e) = self.selection()?;
            return Some(self.rope.slice(s..e).to_string());
        }
        let mut parts: Vec<(usize, String)> = self
            .carets_flagged()
            .iter()
            .filter_map(|(c, _)| {
                let (a, e) = c.bounds();
                (e > a).then(|| (a, self.rope.slice(a..e).to_string()))
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        parts.sort_by_key(|(a, _)| *a);
        Some(parts.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join("\n"))
    }

    /// Select the whole buffer.
    pub fn select_all(&mut self) {
        self.carets.clear();
        self.anchor = Some(0);
        self.cursor = self.rope.len_chars();
        self.goal_col = self.cursor_col();
    }

    /// Select the character range `[start, end)` and put the cursor at `end`
    /// (clamped). Used by find to highlight and scroll to a match.
    pub fn select_range(&mut self, start: usize, end: usize) {
        self.carets.clear();
        let len = self.rope.len_chars();
        self.anchor = Some(start.min(len));
        self.cursor = end.min(len);
        self.goal_col = self.cursor_col();
    }

    /// Drop any active selection (on every caret).
    pub fn clear_selection(&mut self) {
        self.anchor = None;
        for c in &mut self.carets {
            c.anchor = None;
        }
    }

    /// Select the word (or run of separators) under char index `idx` — the
    /// double-click gesture. Never crosses a line break.
    pub fn select_word_at(&mut self, idx: usize) {
        self.carets.clear();
        let n = self.rope.len_chars();
        if n == 0 {
            self.anchor = Some(0);
            self.cursor = 0;
            return;
        }
        let i = idx.min(n - 1);
        // Group by the same class as the clicked char (word vs. non-word), so a
        // click in whitespace selects the whitespace run.
        let wordy = is_word_char(self.rope.char(i));
        let mut s = i;
        while s > 0 {
            let prev = self.rope.char(s - 1);
            if prev == '\n' || is_word_char(prev) != wordy {
                break;
            }
            s -= 1;
        }
        let mut e = i;
        while e < n {
            let ch = self.rope.char(e);
            if ch == '\n' || is_word_char(ch) != wordy {
                break;
            }
            e += 1;
        }
        self.anchor = Some(s);
        self.cursor = e;
        self.goal_col = self.cursor_col();
    }

    /// Select the whole line containing char index `idx`, including its trailing
    /// newline — the triple-click gesture.
    pub fn select_line_at(&mut self, idx: usize) {
        self.carets.clear();
        let n = self.rope.len_chars();
        let line = self.rope.char_to_line(idx.min(n));
        let start = self.rope.line_to_char(line);
        let end = if line + 1 < self.rope.len_lines() {
            self.rope.line_to_char(line + 1)
        } else {
            n
        };
        self.anchor = Some(start);
        self.cursor = end;
        self.goal_col = self.cursor_col();
    }

    /// Begin (or keep) a selection anchored at the current cursor — call before
    /// a shift+movement so the move extends the selection. Applies to every caret.
    pub fn begin_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        for c in &mut self.carets {
            if c.anchor.is_none() {
                c.anchor = Some(c.cursor);
            }
        }
    }

    /// Delete the current selection if any (an **undoable** edit on its own —
    /// used by Cut). Returns whether something was removed.
    pub fn delete_selection(&mut self) -> bool {
        if !self.carets.is_empty() {
            return self.apply_multi_edit(EditKind::Delete, |_b, c| {
                let (a, e) = c.bounds();
                (e > a).then(|| (a, e, String::new()))
            });
        }
        if self.selection().is_none() {
            return false;
        }
        self.checkpoint(EditKind::Delete);
        let removed = self.delete_selection_inner();
        self.note_edit(EditKind::Delete);
        removed
    }

    /// Remove the selection without recording a checkpoint — for callers that
    /// already took one (typing/backspace over a selection).
    fn delete_selection_inner(&mut self) -> bool {
        if let Some((s, e)) = self.selection() {
            self.rope.remove(s..e);
            self.cursor = s;
            self.anchor = None;
            self.goal_col = self.cursor_col();
            self.bump();
            true
        } else {
            false
        }
    }

    // ---- editing -------------------------------------------------------

    pub fn insert_char(&mut self, c: char) {
        if !self.carets.is_empty() {
            let s = c.to_string();
            self.apply_multi_edit(EditKind::Insert, |_b, caret| {
                let (a, e) = caret.bounds();
                Some((a, e, s.clone()))
            });
            return;
        }
        self.checkpoint(EditKind::Insert);
        self.delete_selection_inner();
        self.rope.insert_char(self.cursor, c);
        self.cursor += 1;
        self.goal_col = self.cursor_col();
        self.note_edit(EditKind::Insert);
        self.bump();
    }

    pub fn insert_str(&mut self, s: &str) {
        if !self.carets.is_empty() {
            self.apply_multi_edit(EditKind::Insert, |_b, caret| {
                let (a, e) = caret.bounds();
                Some((a, e, s.to_string()))
            });
            return;
        }
        self.checkpoint(EditKind::Insert);
        self.delete_selection_inner();
        self.rope.insert(self.cursor, s);
        self.cursor += s.chars().count();
        self.goal_col = self.cursor_col();
        self.note_edit(EditKind::Insert);
        self.bump();
    }

    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        if !self.carets.is_empty() {
            self.apply_multi_edit(EditKind::Delete, |_b, c| {
                let (a, e) = c.bounds();
                if e > a {
                    Some((a, e, String::new()))
                } else if c.cursor > 0 {
                    Some((c.cursor - 1, c.cursor, String::new()))
                } else {
                    None
                }
            });
            return;
        }
        if self.selection().is_some() {
            self.checkpoint(EditKind::Delete);
            self.delete_selection_inner();
            self.note_edit(EditKind::Delete);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.rope.remove(self.cursor - 1..self.cursor);
        self.cursor -= 1;
        self.goal_col = self.cursor_col();
        self.note_edit(EditKind::Delete);
        self.bump();
    }

    pub fn delete(&mut self) {
        if !self.carets.is_empty() {
            self.apply_multi_edit(EditKind::Delete, |b, c| {
                let (a, e) = c.bounds();
                if e > a {
                    Some((a, e, String::new()))
                } else if c.cursor < b.rope.len_chars() {
                    Some((c.cursor, c.cursor + 1, String::new()))
                } else {
                    None
                }
            });
            return;
        }
        if self.selection().is_some() {
            self.checkpoint(EditKind::Delete);
            self.delete_selection_inner();
            self.note_edit(EditKind::Delete);
            return;
        }
        if self.cursor >= self.rope.len_chars() {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.rope.remove(self.cursor..self.cursor + 1);
        self.note_edit(EditKind::Delete);
        self.bump();
    }

    // ---- movement ------------------------------------------------------

    pub fn move_left(&mut self) {
        self.each_caret_move(Self::move_left_solo);
    }
    fn move_left_solo(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.goal_col = self.cursor_col();
        }
    }

    pub fn move_right(&mut self) {
        self.each_caret_move(Self::move_right_solo);
    }
    fn move_right_solo(&mut self) {
        if self.cursor < self.rope.len_chars() {
            self.cursor += 1;
            self.goal_col = self.cursor_col();
        }
    }

    pub fn move_up(&mut self) {
        self.each_caret_move(Self::move_up_solo);
    }
    fn move_up_solo(&mut self) {
        let row = self.cursor_row();
        if row == 0 {
            // Already on the first line — jump to the very start.
            self.cursor = 0;
            self.goal_col = 0;
            return;
        }
        self.move_to_line(row - 1);
    }

    pub fn move_down(&mut self) {
        self.each_caret_move(Self::move_down_solo);
    }
    fn move_down_solo(&mut self) {
        let row = self.cursor_row();
        if row + 1 >= self.rope.len_lines() {
            // Already on the last line — jump to the end of it.
            self.move_end_solo();
            return;
        }
        self.move_to_line(row + 1);
    }

    fn move_to_line(&mut self, target: usize) {
        let col = self.goal_col.min(self.line_len_chars(target));
        self.cursor = self.rope.line_to_char(target) + col;
    }

    pub fn move_home(&mut self) {
        self.each_caret_move(Self::move_home_solo);
    }
    fn move_home_solo(&mut self) {
        self.cursor = self.rope.line_to_char(self.cursor_row());
        self.goal_col = 0;
    }

    pub fn move_end(&mut self) {
        self.each_caret_move(Self::move_end_solo);
    }
    fn move_end_solo(&mut self) {
        let row = self.cursor_row();
        self.cursor = self.rope.line_to_char(row) + self.line_len_chars(row);
        self.goal_col = self.cursor_col();
    }

    // ---- word / document movement -------------------------------------

    /// The start of the word at/just before char index `i` (skips any run of
    /// separators, then the word itself) — where Option+Left lands.
    fn prev_word_start(&self, mut i: usize) -> usize {
        while i > 0 && !is_word_char(self.rope.char(i - 1)) {
            i -= 1;
        }
        while i > 0 && is_word_char(self.rope.char(i - 1)) {
            i -= 1;
        }
        i
    }

    /// The end of the word at/just after char index `i` — where Option+Right
    /// lands.
    fn next_word_end(&self, mut i: usize) -> usize {
        let n = self.rope.len_chars();
        while i < n && !is_word_char(self.rope.char(i)) {
            i += 1;
        }
        while i < n && is_word_char(self.rope.char(i)) {
            i += 1;
        }
        i
    }

    pub fn move_word_left(&mut self) {
        self.each_caret_move(Self::move_word_left_solo);
    }
    fn move_word_left_solo(&mut self) {
        self.cursor = self.prev_word_start(self.cursor);
        self.goal_col = self.cursor_col();
    }

    pub fn move_word_right(&mut self) {
        self.each_caret_move(Self::move_word_right_solo);
    }
    fn move_word_right_solo(&mut self) {
        self.cursor = self.next_word_end(self.cursor);
        self.goal_col = self.cursor_col();
    }

    pub fn move_doc_start(&mut self) {
        self.each_caret_move(Self::move_doc_start_solo);
    }
    fn move_doc_start_solo(&mut self) {
        self.cursor = 0;
        self.goal_col = 0;
    }

    pub fn move_doc_end(&mut self) {
        self.each_caret_move(Self::move_doc_end_solo);
    }
    fn move_doc_end_solo(&mut self) {
        self.cursor = self.rope.len_chars();
        self.goal_col = self.cursor_col();
    }

    /// Delete the word to the left of the cursor (Option+Backspace), or the
    /// selection if there is one.
    pub fn delete_word_left(&mut self) {
        if !self.carets.is_empty() {
            self.apply_multi_edit(EditKind::Delete, |b, c| {
                let (a, e) = c.bounds();
                if e > a {
                    return Some((a, e, String::new()));
                }
                let start = b.prev_word_start(c.cursor);
                (start < c.cursor).then(|| (start, c.cursor, String::new()))
            });
            return;
        }
        if self.selection().is_some() {
            self.checkpoint(EditKind::Delete);
            self.delete_selection_inner();
            self.note_edit(EditKind::Delete);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_word_start(self.cursor);
        if start == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.rope.remove(start..self.cursor);
        self.cursor = start;
        self.goal_col = self.cursor_col();
        self.note_edit(EditKind::Delete);
        self.bump();
    }

    /// Delete from the start of the line to the cursor (Cmd+Backspace), or the
    /// selection if there is one.
    pub fn delete_to_line_start(&mut self) {
        if !self.carets.is_empty() {
            self.apply_multi_edit(EditKind::Delete, |b, c| {
                let (a, e) = c.bounds();
                if e > a {
                    return Some((a, e, String::new()));
                }
                let start = b.rope.line_to_char(b.rope.char_to_line(c.cursor));
                (start < c.cursor).then(|| (start, c.cursor, String::new()))
            });
            return;
        }
        if self.selection().is_some() {
            self.checkpoint(EditKind::Delete);
            self.delete_selection_inner();
            self.note_edit(EditKind::Delete);
            return;
        }
        let start = self.rope.line_to_char(self.cursor_row());
        if start == self.cursor {
            return;
        }
        self.checkpoint(EditKind::Delete);
        self.rope.remove(start..self.cursor);
        self.cursor = start;
        self.goal_col = 0;
        self.note_edit(EditKind::Delete);
        self.bump();
    }

    // ---- block indent / outdent ---------------------------------------

    /// The inclusive line range a Tab/Shift+Tab should affect: every line the
    /// selection touches (but not a trailing line the selection only reaches at
    /// column 0), or just the cursor's line when there's no selection.
    fn indent_line_range(&self) -> (usize, usize) {
        let sel = self.selection();
        let (s, e) = sel.unwrap_or((self.cursor, self.cursor));
        let first = self.rope.char_to_line(s);
        let mut last = self.rope.char_to_line(e);
        if sel.is_some() && last > first && e == self.rope.line_to_char(last) {
            last -= 1;
        }
        (first, last)
    }

    /// Re-cover `first..=last` whole lines with the selection (so a Tab/Shift+Tab
    /// run keeps operating on the same block).
    fn select_lines(&mut self, first: usize, last: usize) {
        self.anchor = Some(self.rope.line_to_char(first));
        self.cursor = self.rope.line_to_char(last) + self.line_len_chars(last);
        self.goal_col = self.cursor_col();
    }

    /// Indent every line in the range by four spaces (Tab on a multi-line
    /// selection).
    pub fn indent_selection(&mut self) {
        const PAD: &str = "    ";
        self.carets.clear();
        let (first, last) = self.indent_line_range();
        let had_sel = self.selection().is_some();
        let oc = self.cursor;
        self.checkpoint(EditKind::Insert);
        for line in (first..=last).rev() {
            let at = self.rope.line_to_char(line);
            self.rope.insert(at, PAD);
        }
        if had_sel {
            self.select_lines(first, last);
        } else {
            self.cursor = oc + PAD.chars().count();
            self.goal_col = self.cursor_col();
        }
        self.note_edit(EditKind::Insert);
        self.bump();
    }

    /// Remove up to four leading spaces (or one leading tab) from every line in
    /// the range (Shift+Tab; works on the cursor's line with no selection).
    pub fn outdent_selection(&mut self) {
        self.carets.clear();
        let (first, last) = self.indent_line_range();
        let had_sel = self.selection().is_some();
        let oc = self.cursor;
        let oc_row = self.rope.char_to_line(oc);
        let oc_col = oc - self.rope.line_to_char(oc_row);

        // Decide per-line removals first, so we only checkpoint on a real change.
        let mut removals: Vec<(usize, usize)> = Vec::new();
        for line in first..=last {
            let start = self.rope.line_to_char(line);
            let mut n = 0;
            while n < 4 {
                match self.rope.get_char(start + n) {
                    Some(' ') => n += 1,
                    Some('\t') if n == 0 => {
                        n = 1;
                        break;
                    }
                    _ => break,
                }
            }
            if n > 0 {
                removals.push((start, n));
            }
        }
        if removals.is_empty() {
            return;
        }
        let own_line_removed = removals
            .iter()
            .find(|(start, _)| self.rope.char_to_line(*start) == oc_row)
            .map(|(_, n)| *n)
            .unwrap_or(0);

        self.checkpoint(EditKind::Delete);
        for &(start, n) in removals.iter().rev() {
            self.rope.remove(start..start + n);
        }
        if had_sel {
            self.select_lines(first, last);
        } else {
            let new_col = oc_col.saturating_sub(own_line_removed);
            self.cursor = self.rope.line_to_char(oc_row) + new_col;
            self.goal_col = self.cursor_col();
        }
        self.note_edit(EditKind::Delete);
        self.bump();
    }
}

/// Whether `c` is part of a "word" for Option+Arrow movement (letters, digits,
/// underscore). Everything else — spaces, punctuation — is a separator.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Buffer {
        Buffer {
            id: 0,
            path: None,
            rope: Rope::new(),
            saved: Rope::new(),
            cursor: 0,
            anchor: None,
            carets: Vec::new(),
            goal_col: 0,
            scroll_row: 0,
            modified: false,
            lang: None,
            line_ending: LineEnding::Lf,
            disk: None,
            revision: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_kind: None,
            last_edit_cursor: None,
        }
    }

    #[test]
    fn save_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "").unwrap();
        let mut b = Buffer::from_path(&path).unwrap();
        b.insert_str("save me\n");
        assert!(b.modified);
        b.save().unwrap();
        assert!(!b.modified);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "save me\n");
    }

    #[test]
    fn insert_and_text() {
        let mut b = scratch();
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(b.rope.to_string(), "hello");
        assert_eq!(b.cursor, 5);
        assert_eq!(b.cursor_col(), 5);
    }

    #[test]
    fn down_on_last_line_jumps_to_line_end() {
        let mut b = scratch();
        b.insert_str("first\nlast");
        b.cursor = 0;
        b.goal_col = 0;
        b.move_down(); // -> line 1, col 0
        assert_eq!(b.cursor_row(), 1);
        assert_eq!(b.cursor_col(), 0);
        b.move_down(); // already last line -> end of "last"
        assert_eq!(b.cursor_row(), 1);
        assert_eq!(b.cursor_col(), 4);
    }

    #[test]
    fn up_on_first_line_jumps_to_start() {
        let mut b = scratch();
        b.insert_str("hello\nworld");
        b.cursor = 3; // line 0, col 3
        b.goal_col = 3;
        b.move_up(); // already first line -> start
        assert_eq!(b.cursor, 0);
        assert_eq!(b.cursor_col(), 0);
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut b = scratch();
        b.insert_str("hello");
        assert!(b.undo());
        assert_eq!(b.rope.to_string(), "");
        assert!(b.redo());
        assert_eq!(b.rope.to_string(), "hello");
        // Nothing left to redo.
        assert!(!b.redo());
    }

    #[test]
    fn typing_run_undoes_together() {
        let mut b = scratch();
        for c in "abc".chars() {
            b.insert_char(c);
        }
        // One undo clears the whole contiguous typing run.
        assert!(b.undo());
        assert_eq!(b.rope.to_string(), "");
        assert!(!b.undo());
    }

    #[test]
    fn moving_cursor_breaks_the_undo_group() {
        let mut b = scratch();
        b.insert_str("hello"); // cursor at 5
        b.cursor = 0; // simulate an arrow key / click between edits
        b.insert_char('X'); // "Xhello"
        assert_eq!(b.rope.to_string(), "Xhello");
        b.undo(); // just the 'X'
        assert_eq!(b.rope.to_string(), "hello");
        b.undo(); // the "hello"
        assert_eq!(b.rope.to_string(), "");
    }

    #[test]
    fn undo_back_to_saved_clears_modified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "hello").unwrap();
        let mut b = Buffer::from_path(&path).unwrap();
        assert!(!b.modified);
        b.insert_char('X'); // "Xhello"
        assert!(b.modified, "an edit marks it modified");
        b.undo(); // back to "hello"
        assert_eq!(b.rope.to_string(), "hello");
        assert!(!b.modified, "undoing back to the saved content clears modified");
        // Redoing the edit makes it dirty again.
        b.redo();
        assert!(b.modified);
    }

    #[test]
    fn deleting_typed_text_back_to_saved_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "abc").unwrap();
        let mut b = Buffer::from_path(&path).unwrap();
        b.move_end();
        b.insert_char('d'); // "abcd"
        assert!(b.modified);
        b.backspace(); // manually back to "abc"
        assert!(!b.modified, "reverting by hand also clears modified");
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let mut b = scratch();
        b.insert_str("ab");
        b.undo();
        assert_eq!(b.rope.to_string(), "");
        b.insert_str("cd"); // diverging edit drops the redo history
        assert!(!b.redo());
        assert_eq!(b.rope.to_string(), "cd");
    }

    #[test]
    fn undo_after_cut_restores_text() {
        let mut b = scratch();
        b.insert_str("hello world");
        b.anchor = Some(0);
        b.cursor = 5; // select "hello"
        assert!(b.delete_selection());
        assert_eq!(b.rope.to_string(), " world");
        b.undo();
        assert_eq!(b.rope.to_string(), "hello world");
    }

    #[test]
    fn newline_and_rows() {
        let mut b = scratch();
        b.insert_str("ab\ncd");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.cursor_row(), 1);
        assert_eq!(b.cursor_col(), 2);
        assert_eq!(b.line_len_chars(0), 2);
    }

    #[test]
    fn backspace_and_delete() {
        let mut b = scratch();
        b.insert_str("abc");
        b.backspace();
        assert_eq!(b.rope.to_string(), "ab");
        b.cursor = 0;
        b.delete();
        assert_eq!(b.rope.to_string(), "b");
    }

    #[test]
    fn vertical_movement_keeps_goal_col() {
        let mut b = scratch();
        b.insert_str("long line\nx\nanother");
        b.cursor = 0;
        b.move_end();
        assert_eq!(b.cursor_col(), 9);
        b.move_down();
        assert_eq!(b.cursor_col(), 1);
        b.move_down();
        assert_eq!(b.cursor_col(), 7);
    }

    #[test]
    fn select_all_and_text() {
        let mut b = scratch();
        b.insert_str("abc\ndef");
        b.select_all();
        assert_eq!(b.selection(), Some((0, 7)));
        assert_eq!(b.selected_text().as_deref(), Some("abc\ndef"));
    }

    #[test]
    fn double_click_selects_word_not_across_lines() {
        let mut b = scratch();
        b.insert_str("foo bar\nbaz");
        // Click inside "bar" (index 5) selects the whole word.
        b.select_word_at(5);
        assert_eq!(b.selected_text().as_deref(), Some("bar"));
        // A click in the leading-space run selects just that run, not the words.
        b.select_word_at(3);
        assert_eq!(b.selected_text().as_deref(), Some(" "));
        // Word select never crosses the newline.
        b.select_word_at(8); // "baz" on line 2
        assert_eq!(b.selected_text().as_deref(), Some("baz"));
    }

    #[test]
    fn triple_click_selects_line_with_newline() {
        let mut b = scratch();
        b.insert_str("abc\ndef\nghi");
        b.select_line_at(5); // on the "def" line
        assert_eq!(b.selected_text().as_deref(), Some("def\n"));
        b.select_line_at(9); // last line has no trailing newline
        assert_eq!(b.selected_text().as_deref(), Some("ghi"));
    }

    #[test]
    fn shift_move_extends_then_delete() {
        let mut b = scratch();
        b.insert_str("hello");
        b.cursor = 0;
        b.begin_selection();
        b.move_right();
        b.move_right();
        assert_eq!(b.selected_text().as_deref(), Some("he"));
        assert!(b.delete_selection());
        assert_eq!(b.rope.to_string(), "llo");
        assert_eq!(b.cursor, 0);
        assert!(b.selection().is_none());
    }

    #[test]
    fn typing_replaces_selection() {
        let mut b = scratch();
        b.insert_str("abc");
        b.select_all();
        b.insert_char('X');
        assert_eq!(b.rope.to_string(), "X");
    }

    #[test]
    fn word_movement_lands_on_boundaries() {
        let mut b = scratch();
        b.insert_str("foo bar_baz  qux");
        b.cursor = 0;
        b.move_word_right(); // end of "foo"
        assert_eq!(b.cursor, 3);
        b.move_word_right(); // end of "bar_baz" (underscore is a word char)
        assert_eq!(b.cursor, 11);
        b.move_word_left(); // back to start of "bar_baz"
        assert_eq!(b.cursor, 4);
        b.move_word_left(); // start of "foo"
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn delete_word_left_removes_one_word() {
        let mut b = scratch();
        b.insert_str("hello world");
        b.delete_word_left(); // removes "world"
        assert_eq!(b.rope.to_string(), "hello ");
        let r = b.rope.to_string();
        b.delete_word_left(); // removes "hello " up to start of that word
        assert!(b.rope.to_string().len() < r.len());
    }

    #[test]
    fn delete_to_line_start_clears_to_bol() {
        let mut b = scratch();
        b.insert_str("    indented");
        // cursor at end; delete to start of line removes everything before it.
        b.delete_to_line_start();
        assert_eq!(b.rope.to_string(), "");
    }

    #[test]
    fn indent_and_outdent_a_block() {
        let mut b = scratch();
        b.insert_str("a\nb\nc");
        b.anchor = Some(0);
        b.cursor = b.rope.len_chars(); // select all three lines
        b.indent_selection();
        assert_eq!(b.rope.to_string(), "    a\n    b\n    c");
        b.outdent_selection();
        assert_eq!(b.rope.to_string(), "a\nb\nc");
    }

    #[test]
    fn outdent_single_line_without_selection() {
        let mut b = scratch();
        b.insert_str("    x");
        b.cursor = b.rope.len_chars();
        b.outdent_selection(); // no selection -> dedents the cursor's line
        assert_eq!(b.rope.to_string(), "x");
    }

    #[test]
    fn revision_changes_on_edit() {
        let mut b = scratch();
        let r0 = b.revision();
        b.insert_char('a');
        assert_ne!(r0, b.revision());
    }

    // ---- multi-cursor -------------------------------------------------

    #[test]
    fn add_caret_below_then_insert_hits_every_line() {
        let mut b = scratch();
        b.insert_str("aaa\nbbb\nccc");
        b.cursor = 0;
        b.anchor = None;
        b.goal_col = 0;
        b.add_caret_below();
        b.add_caret_below();
        assert_eq!(b.caret_count(), 3);
        b.insert_char('X');
        assert_eq!(b.rope.to_string(), "Xaaa\nXbbb\nXccc");
        assert_eq!(b.caret_count(), 3, "carets survive the edit");
    }

    #[test]
    fn cmd_d_selects_word_then_following_occurrences() {
        let mut b = scratch();
        b.insert_str("foo bar foo baz foo");
        b.cursor = 0;
        b.anchor = None;
        assert!(b.add_next_occurrence()); // first press selects the word
        assert_eq!(b.selected_text().as_deref(), Some("foo"));
        assert_eq!(b.caret_count(), 1);
        assert!(b.add_next_occurrence()); // second "foo"
        assert!(b.add_next_occurrence()); // third "foo"
        assert_eq!(b.caret_count(), 3);
        b.insert_str("X"); // replace all three at once
        assert_eq!(b.rope.to_string(), "X bar X baz X");
    }

    #[test]
    fn multi_caret_backspace_deletes_at_each() {
        let mut b = scratch();
        b.insert_str("xa\nxb");
        b.cursor = 1; // just after the first 'x'
        b.anchor = None;
        b.goal_col = 1;
        b.add_caret_below();
        assert_eq!(b.caret_count(), 2);
        b.backspace();
        assert_eq!(b.rope.to_string(), "a\nb");
    }

    #[test]
    fn carets_merge_when_they_collide() {
        let mut b = scratch();
        b.insert_str("ab\ncd");
        b.cursor = 0;
        b.anchor = None;
        b.goal_col = 0;
        b.add_caret_below();
        assert_eq!(b.caret_count(), 2);
        b.move_doc_start(); // both collapse onto index 0
        assert_eq!(b.caret_count(), 1);
        assert_eq!(b.cursor, 0);
    }

    #[test]
    fn undo_restores_a_multi_caret_edit() {
        let mut b = scratch();
        b.insert_str("a\na\na");
        b.cursor = 0;
        b.anchor = None;
        b.goal_col = 0;
        b.add_caret_below();
        b.add_caret_below();
        b.insert_char('Z');
        assert_eq!(b.rope.to_string(), "Za\nZa\nZa");
        b.undo();
        assert_eq!(b.rope.to_string(), "a\na\na");
    }

    #[test]
    fn escape_collapses_to_one_caret() {
        let mut b = scratch();
        b.insert_str("a\nb\nc");
        b.cursor = 0;
        b.anchor = None;
        b.goal_col = 0;
        b.add_caret_below();
        assert!(b.has_extra_carets());
        assert!(b.clear_extra_carets());
        assert!(!b.has_extra_carets());
        assert_eq!(b.caret_count(), 1);
    }
}
