//! The file dialog — the app's single entry point into everything file-related.
//! Open it with `Option+F`. With an empty query it shows a browsable, collapsible
//! **tree** (and can scope the search into a folder); typing switches to a flat,
//! VSCode-quick-open-style fuzzy list. It opens the selected file and (via the
//! actions in its footer) creates, renames, and deletes files/folders.
//!
//! This is just the flat-search selection state; the owning [`crate::app::App`]
//! owns the browse tree, the folder scope, and performs the actual operations.

use std::cmp::Ordering;

/// Order two MRU positions: a recent file (`Some`, lower = more recent) sorts
/// before a non-recent one (`None`); two recent files keep MRU order.
fn cmp_recent(a: Option<usize>, b: Option<usize>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[derive(Default)]
pub struct FileDialog {
    pub active: bool,
    pub query: String,
    /// Cursor and selection anchor within `query` (char indices) — the search
    /// box is a full single-line input, edited via [`crate::editline`].
    pub cursor: usize,
    pub anchor: Option<usize>,
    /// Indices into the caller's entry list, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
    /// Entry-list indices the user ticked for multi-open (Tab). Kept across
    /// query changes so you can gather files from several searches.
    pub checked: std::collections::HashSet<usize>,
}

impl FileDialog {
    pub fn open(&mut self) {
        self.active = true;
        self.query.clear();
        self.cursor = 0;
        self.anchor = None;
        self.matches.clear();
        self.selected = 0;
        self.checked.clear();
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
        self.cursor = 0;
        self.anchor = None;
        self.matches.clear();
        self.selected = 0;
        self.checked.clear();
    }

    /// Toggle the tick on the highlighted result (`src` = its entry-list index).
    pub fn toggle_check(&mut self, src: usize) {
        if !self.checked.remove(&src) {
            self.checked.insert(src);
        }
    }

    /// Recompute matches against `entries` (their display strings), VSCode
    /// quick-open style. `recent[i]` is the file's most-recently-used position
    /// (`Some(0)` = most recent), which is the **primary** ranking signal — the
    /// file you just had open sorts first — exactly like VSCode. An empty query
    /// lists every file, recent ones first.
    pub fn refilter(&mut self, entries: &[String], recent: &[Option<usize>]) {
        let rank = |i: usize| recent.get(i).copied().flatten();

        if self.query.is_empty() {
            let mut idx: Vec<usize> = (0..entries.len()).collect();
            idx.sort_by(|&a, &b| cmp_recent(rank(a), rank(b)).then(a.cmp(&b)));
            self.matches = idx;
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len().saturating_sub(1);
            }
            return;
        }

        let by_path = self.query.contains('/');
        // (index, score, path length, label_match). `label_match` = the query
        // matched the *filename* (at a boundary, a camelCase hump, OR mid-word —
        // any subsequence), as opposed to only scattering across the folder path.
        let mut scored: Vec<(usize, i64, usize, bool)> = entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let label = e.rsplit('/').next().unwrap_or(e);
                score_item(label, e, &self.query, by_path)
                    .map(|(s, _, label_match)| (i, s, e.chars().count(), label_match))
            })
            .collect();
        // Show EVERY file whose name fuzzy-matches — exactly like VSCode, which
        // lists mid-word matches too, just ranked low (the boundary/prefix bonuses
        // already push real matches to the top). We only drop pure path-scatter
        // matches (query found only by hopping across deep folder names) when any
        // real filename match exists — that was the random-looking noise. A '/'
        // query opts into path matching, so nothing is dropped then.
        if scored.iter().any(|x| x.3) {
            scored.retain(|x| x.3);
        }
        // Highest score wins (recency nudges recent files up within their tier);
        // then the shorter path; then original order for stability.
        scored.sort_by(|a, b| {
            let sa = a.1 + recency_boost(rank(a.0));
            let sb = b.1 + recency_boost(rank(b.0));
            sb.cmp(&sa).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0))
        });
        self.matches = scored.into_iter().map(|(i, _, _, _)| i).collect();

        // Keep the selection in range (reset to top after a query change).
        self.selected = 0;
    }

    /// Move the selection up, wrapping from the first result to the last.
    pub fn move_up(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.matches.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move the selection down, wrapping from the last result to the first.
    pub fn move_down(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.matches.len();
    }

    /// The entry-list index currently highlighted, if any.
    pub fn selected_source(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }
}

// ---------------------------------------------------------------------------
// VSCode-faithful fuzzy scoring — a direct port of the relevant parts of
// microsoft/vscode `src/vs/base/common/fuzzyScorer.ts`. The filename (label) is
// scored separately from the full path, with large tier gaps so a filename match
// always outranks a path match, and a filename *prefix* match outranks the rest.
// ---------------------------------------------------------------------------

const PATH_IDENTITY_SCORE: i64 = 1 << 18;
const LABEL_PREFIX_SCORE_THRESHOLD: i64 = 1 << 17;
const LABEL_SCORE_THRESHOLD: i64 = 1 << 16;

/// Per-step recency boost added to a match's score (most-recent first). Small
/// relative to the tier thresholds, so it only reorders within a match tier.
const RECENCY_BOOST_STEP: i64 = 96;

/// Recency boost for MRU position `rank` (0 = most recent), or 0 if not recent.
fn recency_boost(rank: Option<usize>) -> i64 {
    match rank {
        Some(r) if r < 256 => (256 - r as i64) * RECENCY_BOOST_STEP,
        _ => 0,
    }
}

fn is_path_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// VSCode `scoreSeparatorAtPos`: bonus for a query char landing right after a
/// separator (path separators are preferred over the rest).
fn separator_bonus(prev: char) -> i64 {
    match prev {
        '/' | '\\' => 5,
        '_' | '-' | '.' | ' ' | '\'' | '"' | ':' => 4,
        _ => 0,
    }
}

/// VSCode `computeCharScore`.
fn compute_char_score(
    q_char: char,
    q_lower: char,
    target: &[char],
    target_lower: &[char],
    ti: usize,
    seq_len: i64,
) -> i64 {
    // considerAsEqual (path separators are interchangeable).
    if q_lower != target_lower[ti] && !(is_path_sep(q_lower) && is_path_sep(target_lower[ti])) {
        return 0;
    }
    let mut score = 1; // character match
    if seq_len > 0 {
        // consecutive: up to 3 get +6 each, the remainder +3 each.
        score += seq_len.min(3) * 6 + (seq_len - 3).max(0) * 3;
    }
    if q_char == target[ti] {
        score += 1; // same case
    }
    if ti == 0 {
        score += 8; // start of word
    } else {
        let sep = separator_bonus(target[ti - 1]);
        if sep != 0 {
            score += sep;
        } else if target[ti].is_uppercase() && seq_len == 0 {
            score += 2; // camelCase hump
        }
    }
    score
}

/// VSCode `doScoreFuzzy` DP. Returns (score, matched char positions in target).
fn score_fuzzy(
    target: &[char],
    target_lower: &[char],
    query: &[char],
    query_lower: &[char],
) -> (i64, Vec<usize>) {
    let tl = target.len();
    let ql = query.len();
    if tl == 0 || ql == 0 || tl < ql {
        return (0, Vec::new());
    }
    let mut scores = vec![0i64; ql * tl];
    let mut matches = vec![0i64; ql * tl];
    for qi in 0..ql {
        let q_off = qi * tl;
        for ti in 0..tl {
            let cur = q_off + ti;
            let left_score = if ti > 0 { scores[cur - 1] } else { 0 };
            let (diag_score, seq_len) = if qi > 0 && ti > 0 {
                let d = q_off - tl + ti - 1;
                (scores[d], matches[d])
            } else {
                (0, 0)
            };
            // Once past the first query char, only score if the previous diagonal
            // had a score — keeps the match in sequence.
            let score = if diag_score == 0 && qi > 0 {
                0
            } else {
                compute_char_score(query[qi], query_lower[qi], target, target_lower, ti, seq_len)
            };
            if score != 0 && diag_score + score >= left_score {
                matches[cur] = seq_len + 1;
                scores[cur] = diag_score + score;
            } else {
                matches[cur] = 0;
                scores[cur] = left_score;
            }
        }
    }
    // Backtrack to recover positions.
    let mut positions = Vec::new();
    let mut qi = ql as isize - 1;
    let mut ti = tl as isize - 1;
    while qi >= 0 && ti >= 0 {
        let cur = qi as usize * tl + ti as usize;
        if matches[cur] == 0 {
            ti -= 1;
        } else {
            positions.push(ti as usize);
            qi -= 1;
            ti -= 1;
        }
    }
    positions.reverse();
    (scores[ql * tl - 1], positions)
}

/// Score one query piece against an item, VSCode `doScoreItemFuzzySingle`.
/// `label` is the filename, `full` the relative path. Returns (score, positions
/// in `full`, label_match) where `label_match` = the piece matched the filename
/// (any subsequence), as opposed to only the folder path. Word-boundary and
/// prefix matches still score far higher (so they rank first); the flag just
/// separates "real filename hit" from "path-scatter" for filtering.
fn score_piece(label: &str, full: &str, query: &str, prefer_label: bool) -> Option<(i64, Vec<usize>, bool)> {
    let q: Vec<char> = query.chars().collect();
    let q_lower: Vec<char> = query.to_lowercase().chars().collect();

    if prefer_label {
        let lab: Vec<char> = label.chars().collect();
        let lab_lower: Vec<char> = label.to_lowercase().chars().collect();
        let (ls, lpos) = score_fuzzy(&lab, &lab_lower, &q, &q_lower);
        if ls > 0 {
            let offset = full.chars().count() - lab.len();
            let base = if starts_with_ci(&lab_lower, &q_lower) {
                // Prefix match: big boost, plus more for a shorter filename so
                // "window.ts" beats "windowActions.ts" for query "window".
                let prefix_boost = ((q.len() as f64 / lab.len().max(1) as f64) * 100.0).round() as i64;
                LABEL_PREFIX_SCORE_THRESHOLD + prefix_boost
            } else {
                LABEL_SCORE_THRESHOLD
            };
            let positions = lpos.into_iter().map(|p| p + offset).collect();
            return Some((base + ls, positions, true)); // matched the filename
        }
    }

    // Fall back to matching the full path (folder + filename) — a weak match.
    let f: Vec<char> = full.chars().collect();
    let f_lower: Vec<char> = full.to_lowercase().chars().collect();
    let (ps, ppos) = score_fuzzy(&f, &f_lower, &q, &q_lower);
    if ps > 0 {
        return Some((ps, ppos, false)); // path-only (scatter) match
    }
    None
}

fn starts_with_ci(target_lower: &[char], query_lower: &[char]) -> bool {
    target_lower.len() >= query_lower.len() && target_lower[..query_lower.len()] == *query_lower
}

/// Score a whole item (VSCode `doScoreItemFuzzy`): identity, then per-piece
/// scoring for space-separated queries (every piece must match). `by_path`
/// (query contains `/`) makes it match the full path rather than the filename.
/// The returned bool is `label_match` = every piece matched the filename (not
/// just the path) — see [`score_piece`].
fn score_item(label: &str, full: &str, query: &str, by_path: bool) -> Option<(i64, Vec<usize>, bool)> {
    if query.eq_ignore_ascii_case(full) {
        return Some((PATH_IDENTITY_SCORE, (0..full.chars().count()).collect(), true));
    }
    let prefer_label = !by_path;
    let mut total = 0i64;
    let mut positions = Vec::new();
    let mut label_match = true;
    let mut any = false;
    for piece in query.split_whitespace() {
        any = true;
        let (s, p, lm) = score_piece(label, full, piece, prefer_label)?;
        total += s;
        positions.extend(p);
        label_match &= lm;
    }
    if !any {
        return None;
    }
    positions.sort_unstable();
    positions.dedup();
    Some((total, positions, label_match))
}

/// The char indices in `text` (a relative path) that `query` matched, for
/// bolding them in a result. Agrees with the scoring above.
pub fn fuzzy_positions(query: &str, text: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let label = text.rsplit('/').next().unwrap_or(text);
    score_item(label, text, query, query.contains('/'))
        .map(|(_, pos, _)| pos)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_cover_the_match() {
        // Highlights land on the matched characters (filename-first).
        let pos = fuzzy_positions("man", "src/domain.rs");
        assert!(!pos.is_empty());
        // The chars at those indices spell a supersequence of the query.
        let text: Vec<char> = "src/domain.rs".chars().collect();
        let hit: String = pos.iter().map(|&i| text[i]).collect();
        assert_eq!(hit, "man");
        // No match -> no highlights.
        assert!(fuzzy_positions("zzz", "src/domain.rs").is_empty());
        assert!(fuzzy_positions("", "anything").is_empty());
    }

    #[test]
    fn filename_match_ranks_above_path_only_match() {
        let mut d = FileDialog::default();
        d.query = "main".to_string();
        d.refilter(
            &[
                "main/lib.rs".into(), // "main" only in the path
                "src/main.rs".into(), // "main" in the filename — should win
            ],
            &[],
        );
        assert_eq!(d.selected_source(), Some(1));
    }

    #[test]
    fn non_matches_are_filtered_out() {
        let mut d = FileDialog::default();
        d.query = "xyz".to_string();
        d.refilter(&["main.rs".into(), "lib.rs".into()], &[]);
        assert!(d.matches.is_empty());
    }

    #[test]
    fn path_scatter_noise_is_excluded() {
        // A plain query must NOT match characters scattered across deep folder
        // names — only the filename. "sess dto" should find the session DTO and
        // nothing else (not, say, app_button.dart via s/e/s/s/.../d/t/o in the
        // path), which is what made results feel random before.
        let entries: Vec<String> = vec![
            "server_nestjs/src/sessions/create-session.dto.ts".into(),
            "thrive_shared/lib/src/widgets/app_button.dart".into(),
            "thrive_shared/lib/src/constants/colors.dart".into(),
        ];
        let mut d = FileDialog::default();
        d.query = "sess dto".to_string();
        d.refilter(&entries, &[]);
        assert_eq!(d.matches.len(), 1, "only the real filename match survives");
        assert_eq!(entries[d.matches[0]], "server_nestjs/src/sessions/create-session.dto.ts");
    }

    #[test]
    fn slash_query_matches_the_path() {
        // Typing a '/' opts into path matching (narrow by folder).
        let entries: Vec<String> = vec![
            "src/components/Button.tsx".into(),
            "src/widgets/Button.tsx".into(),
        ];
        let mut d = FileDialog::default();
        d.query = "comp/button".to_string();
        d.refilter(&entries, &[]);
        assert_eq!(d.matches.len(), 1);
        assert_eq!(entries[d.matches[0]], "src/components/Button.tsx");
    }

    #[test]
    fn boundary_matches_rank_above_mid_word() {
        // Like VSCode: every filename fuzzy-match for "app" is listed, but the
        // word-boundary / prefix matches (app.rs, AppBar.tsx, application.ts) rank
        // ABOVE the mid-word coincidences (wr-app-er, m-app-er, h-app-y) — the
        // real file is at the top, the noise sinks to the bottom (never hidden,
        // so nothing the user is looking for can silently vanish).
        let entries: Vec<String> = [
            "src/app.rs",
            "src/wrapper.rs",
            "src/mapper.rs",
            "lib/application.ts",
            "lib/happy_path.ts",
            "components/AppBar.tsx",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut d = FileDialog::default();
        d.query = "app".to_string();
        d.refilter(&entries, &[]);
        let shown: Vec<&str> = d.matches.iter().map(|&i| entries[i].as_str()).collect();
        let pos = |s: &str| shown.iter().position(|&x| x == s).expect("file is listed");
        // The boundary matches are present and each ranks ahead of every mid-word one.
        for boundary in ["src/app.rs", "lib/application.ts", "components/AppBar.tsx"] {
            for mid in ["src/wrapper.rs", "src/mapper.rs", "lib/happy_path.ts"] {
                assert!(pos(boundary) < pos(mid), "{boundary} should rank above {mid}");
            }
        }
    }

    #[test]
    fn camelcase_hump_ranks_above_mid_word() {
        // "bar": bar.rs (prefix) and AppBar.tsx (camelCase hump) outrank the
        // mid-word coincidence crowbar.rs — which still appears, VSCode-style.
        let entries: Vec<String> = ["AppBar.tsx", "crowbar.rs", "bar.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut d = FileDialog::default();
        d.query = "bar".to_string();
        d.refilter(&entries, &[]);
        let shown: Vec<&str> = d.matches.iter().map(|&i| entries[i].as_str()).collect();
        let pos = |s: &str| shown.iter().position(|&x| x == s).expect("file is listed");
        assert!(pos("bar.rs") < pos("crowbar.rs"));
        assert!(pos("AppBar.tsx") < pos("crowbar.rs"));
    }

    #[test]
    fn recent_file_ranks_first() {
        let entries: Vec<String> = ["src/user.rs", "src/user_service.rs", "src/user_controller.rs"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // No recency → best name match (user.rs) is first.
        let mut d = FileDialog::default();
        d.query = "user".to_string();
        d.refilter(&entries, &[]);
        assert_eq!(entries[d.matches[0]], "src/user.rs");

        // Mark user_service.rs (index 1) most-recently-used → it jumps to the top,
        // even though user.rs is a "better" name match.
        let recent = vec![None, Some(0), None];
        let mut d = FileDialog::default();
        d.query = "user".to_string();
        d.refilter(&entries, &recent);
        assert_eq!(entries[d.matches[0]], "src/user_service.rs");
    }

    #[test]
    fn prefix_beats_mid_name_beats_path() {
        // VSCode's own example: "window" ranks window.ts > windowActions.ts >
        // createWindow.ts (prefix > shorter prefix > camelCase-not-prefix).
        let entries: Vec<String> = ["window.ts", "windowActions.ts", "lib/createWindow.ts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut d = FileDialog::default();
        d.query = "window".to_string();
        d.refilter(&entries, &[]);
        let order: Vec<&str> = d.matches.iter().map(|&i| entries[i].as_str()).collect();
        assert_eq!(order, vec!["window.ts", "windowActions.ts", "lib/createWindow.ts"]);
    }

    #[test]
    fn nav_wraps_both_directions() {
        let mut d = FileDialog::default();
        d.matches = vec![0, 1, 2];
        d.selected = 0;
        d.move_up(); // up from the top wraps to the bottom
        assert_eq!(d.selected, 2);
        d.move_down(); // down from the bottom wraps to the top
        assert_eq!(d.selected, 0);
        d.move_down();
        assert_eq!(d.selected, 1);
        d.move_up();
        assert_eq!(d.selected, 0);
    }

    #[test]
    fn ranks_best_first() {
        let mut d = FileDialog::default();
        d.query = "main".to_string();
        d.refilter(&["lib.rs".into(), "domain.rs".into(), "main.rs".into()], &[]);
        // Exact filename "main.rs" ranks above "domain.rs".
        assert_eq!(d.selected_source(), Some(2));
    }

    #[test]
    fn vscode_constants_ordering() {
        // Mirrors the real screenshot: query "constants" over a mixed tree. Only
        // files whose *name* matches appear (folder-only files like colors.dart
        // are excluded), exact "constants.dart" first with the shorter path
        // winning the tie, then "app_constants.dart" — exactly like VSCode.
        let entries: Vec<String> = vec![
            "thrive_shared/lib/src/constants/colors.dart".into(),     // folder-only → excluded
            "trainer_flutter/lib/config/constants.dart".into(),       // filename
            "thrive_shared/lib/src/constants/app_constants.dart".into(), // filename (mid)
            "client_flutter/lib/config/constants.dart".into(),        // filename, shorter path
            "thrive_shared/lib/src/constants/text_styles.dart".into(), // folder-only → excluded
        ];
        let mut d = FileDialog::default();
        d.query = "constants".to_string();
        d.refilter(&entries, &[]);
        let order: Vec<&str> = d.matches.iter().map(|&i| entries[i].as_str()).collect();
        assert_eq!(
            order,
            vec![
                "client_flutter/lib/config/constants.dart",
                "trainer_flutter/lib/config/constants.dart",
                "thrive_shared/lib/src/constants/app_constants.dart",
            ],
            "only filename matches, best/closest first; folder-only files excluded"
        );
    }
}
