//! Project-wide "Search in Files" (⌘⇧F / Ctrl+Alt+⇧F in TUI mode): one query
//! searched across every file the project's Quick Open would show (same
//! gitignore / junk-dir rules as [`crate::fstree::collect_files`]), grouped
//! by file with a line preview per match — VSCode's "Search" view.
//!
//! The actual scan is powered by ripgrep's own search engine
//! (`grep-searcher` + `grep-regex`, the same crates the `rg` binary is built
//! on) walked in parallel via [`ignore::WalkBuilder::build_parallel`] — a
//! literal, case-insensitive, multi-threaded, memory-mapped search rather
//! than a single-threaded per-line `Vec<char>` scan. `App::run_project_search`
//! additionally runs this on a background thread (mirroring the syntax
//! highlighter's async job pattern), so even a slow search never blocks
//! typing or rendering.
//!
//! Re-running the scan on every single keystroke would still be wasteful (and
//! would flash results away while typing), so it's debounced: a search
//! auto-runs [`SEARCH_DEBOUNCE`] after the query stops changing (see
//! `App::poll_pending_search`) — short enough to feel live, long enough that
//! a fast typist doesn't trigger a search per keystroke. Enter opens the
//! selected result rather than forcing a search.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;

use crate::fstree;

/// Cap on total matches collected, across every file — keeps a query that
/// matches something on nearly every line (e.g. a single common letter) from
/// building an unbounded results list.
const MAX_MATCHES: usize = 5_000;

/// How long the query must sit still before a search auto-runs — the usual
/// "search as you type" debounce window (VSCode/most editors land around
/// 200-300ms).
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Skip files bigger than this — almost certainly not source you're searching
/// (a bundled asset, a lockfile, a binary that slipped past the extension
/// check), and reading it would dominate the search's cost for no benefit.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Longest preview line kept verbatim before windowing around the match —
/// long enough for ordinary source, short enough that one huge minified line
/// can't blow up the results list.
const MAX_PREVIEW_CHARS: usize = 200;
/// How much context to keep on each side of the match once windowing kicks in.
const PREVIEW_CONTEXT_CHARS: usize = 60;

/// One match within a file. `line`/`col`/`end_col` are the *real* 0-based
/// line/char position in the file (what jumping to the match uses); `preview`
/// plus `preview_col`/`preview_end_col` are a separately-windowed copy of the
/// line for display, so a single absurdly long line doesn't get rendered (or
/// stored) in full.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchMatch {
    pub line: usize,
    pub col: usize,
    pub end_col: usize,
    pub preview: String,
    pub preview_col: usize,
    pub preview_end_col: usize,
}

#[derive(Debug, Clone)]
pub struct SearchFileResult {
    pub path: PathBuf,
    pub matches: Vec<SearchMatch>,
}

/// Window `line` around the char range `[col, end_col)` so a preview is never
/// wildly longer than a normal line, keeping `col`/`end_col` valid offsets
/// into the *returned* string.
fn build_preview(line: &str, col: usize, end_col: usize) -> (String, usize, usize) {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= MAX_PREVIEW_CHARS {
        return (line.to_string(), col, end_col);
    }
    let start = col.saturating_sub(PREVIEW_CONTEXT_CHARS);
    let end = (end_col + PREVIEW_CONTEXT_CHARS).min(chars.len());
    let mut out = String::new();
    let mut new_col = col - start;
    let mut new_end = end_col - start;
    if start > 0 {
        out.push_str("\u{2026} ");
        new_col += 2;
        new_end += 2;
    }
    out.push_str(&chars[start..end].iter().collect::<String>());
    if end < chars.len() {
        out.push_str(" \u{2026}");
    }
    (out, new_col, new_end)
}

/// Search every project file for `query` (case-insensitive, literal
/// substring — same semantics as before), grouped by file in path order.
/// `include_junk` mirrors the Files dialog's ⌥H toggle (include
/// `node_modules`/`build`/… in the walk). Returns the results plus whether
/// the match cap was hit.
///
/// The walk and the per-file search both run in parallel across every core
/// via ripgrep's own engine (`ignore::WalkBuilder::build_parallel` +
/// `grep-searcher`/`grep-regex`) — this is the expensive, synchronous-feeling
/// part of "Search in Files"; see `App::run_project_search` for how the
/// *caller* keeps this off the UI thread.
pub fn search_project(root: &Path, query: &str, include_junk: bool) -> (Vec<SearchFileResult>, bool) {
    if query.is_empty() {
        return (Vec::new(), false);
    }
    // `build_literals` treats the query as a plain string (no regex
    // metacharacters), matching Find's literal-substring semantics.
    let Ok(matcher) = RegexMatcherBuilder::new().case_insensitive(true).build_literals(&[query]) else {
        return (Vec::new(), false);
    };

    let results: Arc<Mutex<Vec<SearchFileResult>>> = Arc::new(Mutex::new(Vec::new()));
    let total = Arc::new(AtomicUsize::new(0));
    // Set once the match cap is hit, so every worker thread stops picking up
    // new files as soon as any one of them notices — without this, a query
    // that matches almost everywhere would keep every core busy walking the
    // *entire* project just to throw away results past the cap.
    let stop = Arc::new(AtomicBool::new(false));

    let walker = fstree::walk_builder(root, false, include_junk).build_parallel();
    walker.run(|| {
        let matcher = matcher.clone();
        let results = Arc::clone(&results);
        let total = Arc::clone(&total);
        let stop = Arc::clone(&stop);
        let mut searcher = SearcherBuilder::new().line_number(true).build();
        Box::new(move |entry| {
            if stop.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else { return ignore::WalkState::Continue };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else { return ignore::WalkState::Continue };
            if meta.len() > MAX_FILE_BYTES {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();

            let mut matches: Vec<SearchMatch> = Vec::new();
            // A search failure (binary content, invalid UTF-8, a file that
            // vanished mid-walk, …) just means this file isn't searchable —
            // skip it rather than aborting the whole search. `keep_going`
            // tracks whether the *cap* (not a file error) ended the search.
            let mut keep_going = true;
            let _ = searcher.search_path(
                &matcher,
                path,
                UTF8(|line_number, line| {
                    let line_no = (line_number as usize).saturating_sub(1);
                    let line = line.strip_suffix('\n').unwrap_or(line);
                    let line = line.strip_suffix('\r').unwrap_or(line);

                    // Matches within a line arrive in increasing byte order,
                    // so the byte->char offset conversion each needs can be
                    // done incrementally in one left-to-right pass rather
                    // than re-scanning the line's prefix per match.
                    let mut last_byte = 0usize;
                    let mut last_char = 0usize;
                    let find_result = matcher.find_iter(line.as_bytes(), |m| {
                        // Reserve this match's slot *before* doing any work for
                        // it: `fetch_add` hands out a unique, strictly-increasing
                        // index across every worker thread, so gating the push on
                        // `slot < MAX_MATCHES` keeps the total pushed matches
                        // exactly at the cap even when many threads race past it
                        // at once (an after-the-fact check would let each thread
                        // push one extra match before noticing `stop`).
                        let slot = total.fetch_add(1, Ordering::Relaxed);
                        if slot >= MAX_MATCHES {
                            stop.store(true, Ordering::Relaxed);
                            keep_going = false;
                            return false;
                        }
                        last_char += line[last_byte..m.start()].chars().count();
                        last_byte = m.start();
                        let col = last_char;
                        last_char += line[last_byte..m.end()].chars().count();
                        last_byte = m.end();
                        let end_col = last_char;

                        let (preview, preview_col, preview_end_col) = build_preview(line, col, end_col);
                        matches.push(SearchMatch { line: line_no, col, end_col, preview, preview_col, preview_end_col });
                        if slot + 1 >= MAX_MATCHES {
                            stop.store(true, Ordering::Relaxed);
                            keep_going = false;
                        }
                        keep_going
                    });
                    Ok(find_result.is_ok() && keep_going)
                }),
            );

            if !matches.is_empty() {
                results.lock().unwrap_or_else(|e| e.into_inner()).push(SearchFileResult { path: path.to_path_buf(), matches });
            }
            if stop.load(Ordering::Relaxed) { ignore::WalkState::Quit } else { ignore::WalkState::Continue }
        })
    });

    let truncated = stop.load(Ordering::Relaxed);
    let mut results = Arc::try_unwrap(results)
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .unwrap_or_default();
    results.sort_by(|a, b| a.path.cmp(&b.path));
    (results, truncated)
}

/// UI/navigation state for the Search-in-Files dialog. Mirrors
/// [`crate::filedialog::FileDialog`]'s split of responsibility: this module
/// owns just the query text and result list; `App` performs the actual disk
/// search and opens the file a selected match points to.
#[derive(Default)]
pub struct ProjectSearch {
    pub active: bool,
    pub query: String,
    /// Cursor and selection anchor within `query` (char indices) — a full
    /// single-line input, edited via [`crate::editline`].
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub results: Vec<SearchFileResult>,
    /// Flat index over every match across every file, in display order.
    pub selected: usize,
    /// Whether `results` reflects the current `query` — false right after
    /// opening or editing the query (nothing has searched for *this* text
    /// yet), true once Enter actually runs a search.
    pub searched: bool,
    /// Whether the last search hit [`MAX_MATCHES`].
    pub truncated: bool,
    /// When set, a search auto-runs once `Instant::now()` passes this —
    /// (re)armed on every query edit, so it always reflects "1.5s after the
    /// *last* keystroke", not the first. See `App::poll_pending_search`.
    pub pending_search_at: Option<Instant>,
    /// A background search job is currently running (see
    /// `App::run_project_search`/`App::poll_search_results`) — distinct from
    /// `pending_search_at.is_some()`, which just means the debounce timer
    /// hasn't fired *yet*. The UI shows "Searching…" for either.
    pub in_flight: bool,
}

impl ProjectSearch {
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.anchor = None;
        self.results.clear();
        self.selected = 0;
        self.searched = false;
        self.truncated = false;
        self.pending_search_at = None;
        self.in_flight = false;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.cursor = 0;
        self.anchor = None;
        self.results.clear();
        self.selected = 0;
        self.searched = false;
        self.truncated = false;
        self.pending_search_at = None;
        self.in_flight = false;
    }

    /// The query changed — the existing `results` no longer reflect it, so
    /// drop them rather than leave a stale list on screen next to fresh text.
    /// Arms the debounce (unless the query is now empty — nothing to search
    /// for), so a search auto-runs once typing settles.
    pub fn invalidate(&mut self) {
        self.results.clear();
        self.selected = 0;
        self.searched = false;
        self.truncated = false;
        self.pending_search_at =
            if self.query.is_empty() { None } else { Some(Instant::now() + SEARCH_DEBOUNCE) };
    }

    pub fn total_matches(&self) -> usize {
        self.results.iter().map(|r| r.matches.len()).sum()
    }

    pub fn move_up(&mut self) {
        let n = self.total_matches();
        if n == 0 {
            return;
        }
        self.selected = if self.selected == 0 { n - 1 } else { self.selected - 1 };
    }

    pub fn move_down(&mut self) {
        let n = self.total_matches();
        if n == 0 {
            return;
        }
        self.selected = (self.selected + 1) % n;
    }

    /// Resolve the flat `selected` index into (file index, match index).
    pub fn selected_match(&self) -> Option<(usize, usize)> {
        let mut idx = self.selected;
        for (fi, r) in self.results.iter().enumerate() {
            if idx < r.matches.len() {
                return Some((fi, idx));
            }
            idx -= r.matches.len();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n").unwrap();
        fs::write(dir.path().join("src/lib.rs"), "pub fn needle_finder() {}\n").unwrap();
        fs::write(dir.path().join("README.md"), "no matches here\n").unwrap();
        dir
    }

    #[test]
    fn finds_matches_across_multiple_files_case_insensitively() {
        let dir = workspace();
        let (results, truncated) = search_project(dir.path(), "NEEDLE", false);
        assert!(!truncated);
        let paths: Vec<_> = results.iter().map(|r| r.path.file_name().unwrap().to_str().unwrap()).collect();
        assert!(paths.contains(&"main.rs"));
        assert!(paths.contains(&"lib.rs"));
        assert!(!paths.contains(&"README.md"));
        let main_matches = &results.iter().find(|r| r.path.ends_with("main.rs")).unwrap().matches;
        assert_eq!(main_matches.len(), 1);
        assert_eq!(main_matches[0].line, 1);
        assert_eq!(main_matches[0].col, 8); // "    let needle" -> 'n' at char 8
        assert_eq!(main_matches[0].end_col, 14);
    }

    #[test]
    fn finds_multiple_non_overlapping_matches_on_one_line() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f.txt"), "cat cat cat\n").unwrap();
        let (results, _) = search_project(dir.path(), "cat", false);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].matches.len(), 3);
    }

    #[test]
    fn empty_query_finds_nothing() {
        let dir = workspace();
        let (results, truncated) = search_project(dir.path(), "", false);
        assert!(results.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn respects_gitignore_and_junk_pruning_like_quick_open() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        fs::write(dir.path().join("node_modules/pkg.js"), "needle\n").unwrap();
        fs::write(dir.path().join("keep.txt"), "needle\n").unwrap();

        // Gitignored files ARE still searched (Quick Open finds them too,
        // just faded) — only the well-known junk dirs are pruned outright.
        let (results, _) = search_project(dir.path(), "needle", false);
        let names: Vec<_> = results.iter().map(|r| r.path.file_name().unwrap().to_str().unwrap()).collect();
        assert!(names.contains(&"keep.txt"));
        assert!(names.contains(&"ignored.txt"));
        assert!(!results.iter().any(|r| r.path.to_string_lossy().contains("node_modules")));
    }

    #[test]
    fn long_lines_are_windowed_around_the_match_for_preview() {
        let dir = tempfile::tempdir().unwrap();
        let padding = "x".repeat(300);
        let line = format!("{padding}NEEDLE{padding}\n");
        fs::write(dir.path().join("f.txt"), &line).unwrap();
        let (results, _) = search_project(dir.path(), "needle", false);
        let m = &results[0].matches[0];
        // Real file position is unaffected by windowing.
        assert_eq!(m.col, 300);
        assert_eq!(m.end_col, 306);
        // The rendered preview is much shorter than the real line, with the
        // match still resolvable at its own (preview-local) offset.
        assert!(m.preview.chars().count() < 300, "preview should be windowed down");
        let preview_chars: Vec<char> = m.preview.chars().collect();
        let matched: String = preview_chars[m.preview_col..m.preview_end_col].iter().collect();
        assert_eq!(matched.to_lowercase(), "needle");
    }

    #[test]
    fn caps_total_matches_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        // Many small files, each with several matches, comfortably over the cap.
        for i in 0..200 {
            let content = "needle needle needle needle\n".repeat(10);
            fs::write(dir.path().join(format!("f{i}.txt")), content).unwrap();
        }
        let (results, truncated) = search_project(dir.path(), "needle", false);
        assert!(truncated);
        let total: usize = results.iter().map(|r| r.matches.len()).sum();
        assert!(total <= MAX_MATCHES);
    }

    #[test]
    fn project_search_navigation_wraps_and_resolves_file_and_match_index() {
        let mut ps = ProjectSearch::default();
        ps.results = vec![
            SearchFileResult {
                path: PathBuf::from("a.txt"),
                matches: vec![
                    SearchMatch { line: 0, col: 0, end_col: 1, preview: "a".into(), preview_col: 0, preview_end_col: 1 },
                    SearchMatch { line: 1, col: 0, end_col: 1, preview: "a".into(), preview_col: 0, preview_end_col: 1 },
                ],
            },
            SearchFileResult {
                path: PathBuf::from("b.txt"),
                matches: vec![SearchMatch {
                    line: 0,
                    col: 0,
                    end_col: 1,
                    preview: "b".into(),
                    preview_col: 0,
                    preview_end_col: 1,
                }],
            },
        ];
        assert_eq!(ps.selected_match(), Some((0, 0)));
        ps.move_down();
        assert_eq!(ps.selected_match(), Some((0, 1)));
        ps.move_down();
        assert_eq!(ps.selected_match(), Some((1, 0)), "crosses into the second file's matches");
        ps.move_down();
        assert_eq!(ps.selected_match(), Some((0, 0)), "wraps back to the first match");
        ps.move_up();
        assert_eq!(ps.selected_match(), Some((1, 0)), "wraps backward too");
    }

    #[test]
    fn invalidate_arms_the_debounce_when_the_query_is_non_empty() {
        let mut ps = ProjectSearch::default();
        assert!(ps.pending_search_at.is_none());
        ps.query = "needle".to_string();
        ps.invalidate();
        let at = ps.pending_search_at.expect("a non-empty query should arm the debounce");
        assert!(at > Instant::now(), "the deadline should be in the future");
        assert!(at <= Instant::now() + SEARCH_DEBOUNCE, "shouldn't overshoot the debounce window");
    }

    #[test]
    fn invalidate_clears_the_debounce_when_the_query_is_empty() {
        let mut ps = ProjectSearch::default();
        ps.query = "needle".to_string();
        ps.invalidate();
        assert!(ps.pending_search_at.is_some());
        ps.query.clear();
        ps.invalidate();
        assert!(ps.pending_search_at.is_none(), "nothing to search for -> no pending auto-search");
    }
}
