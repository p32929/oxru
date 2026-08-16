//! A global to-do list, stored as an ordinary markdown file.
//!
//! The file is the source of truth, not a cache of some in-memory model: the
//! dialog reads it when opened and writes it on every change, and `⌃E` opens
//! that same file as a normal editor tab. That's what makes "edit it as plain
//! text" free — the whole editor (undo, multi-cursor, find, save) already works
//! on a buffer, so there's no second, weaker editor to build and maintain here.
//!
//! Parsing is deliberately forgiving and writing is normalising: you can paste
//! a bare list of lines, and they come back as checkboxes. What parsing must
//! never do is *lose* something — headings and blank lines round-trip
//! untouched, so a file you've organised yourself survives being toggled from
//! the dialog.

use std::path::{Path, PathBuf};

/// One line of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A checkbox line.
    Task { done: bool, text: String },
    /// Anything else — a heading, a blank line, a note. Kept verbatim so the
    /// user's own structure isn't rewritten out from under them.
    Raw(String),
}

impl Entry {
    pub fn is_task(&self) -> bool {
        matches!(self, Entry::Task { .. })
    }

    pub fn is_done(&self) -> bool {
        matches!(self, Entry::Task { done: true, .. })
    }
}

/// `~/.config/oxru/todos.md` (or the platform config dir). Global, not
/// per-project: it's a list of what *you* are doing, which rarely lines up with
/// which repo happens to be open.
pub fn todos_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxru/todos.md"))
}

/// Read `text` into entries. Never fails and never drops a line.
///
/// Recognised, in order: a blank line or a `#` heading stays [`Entry::Raw`];
/// otherwise an optional list marker (`-`, `*`, `+`, `1.`) is stripped, then an
/// optional checkbox (`[x]`, `[X]`, `[ ]`, `[]`); whatever remains is the task
/// text. A line with no marker and no checkbox is still a task — that's the
/// "type a plain list, get checkboxes" behaviour.
pub fn parse(text: &str) -> Vec<Entry> {
    text.lines().map(parse_line).collect()
}

fn parse_line(line: &str) -> Entry {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Entry::Raw(line.trim_end().to_string());
    }

    let body = strip_list_marker(trimmed);
    let (done, rest) = match strip_checkbox(body) {
        Some((done, rest)) => (done, rest),
        None => (false, body),
    };
    Entry::Task { done, text: rest.trim().to_string() }
}

/// Remove a leading `-`, `*`, `+` or `12.` bullet, if present.
fn strip_list_marker(s: &str) -> &str {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
            return rest.trim_start();
        }
    }
    // A bare marker on its own line ("-") is still a marker.
    if s == "-" || s == "*" || s == "+" {
        return "";
    }
    // Ordered-list markers: any run of digits followed by '.' or ')'.
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &s[digits.len()..];
        for m in [". ", ") "] {
            if let Some(r) = rest.strip_prefix(m) {
                return r.trim_start();
            }
        }
    }
    s
}

/// Read a leading `[x]` / `[ ]` / `[]` checkbox, returning `(done, rest)`.
fn strip_checkbox(s: &str) -> Option<(bool, &str)> {
    let rest = s.strip_prefix('[')?;
    let (marker, rest) = rest.split_at(rest.find(']')?);
    let rest = &rest[1..]; // drop the ']'
    match marker.trim() {
        "" => Some((false, rest)),
        m if m.eq_ignore_ascii_case("x") => Some((true, rest)),
        _ => None, // `[TODO]` and friends are prose, not a checkbox
    }
}

/// Render entries back to file text. Every task is normalised to `- [ ]` /
/// `- [x]`; raw lines are written exactly as they were read.
pub fn to_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        match e {
            Entry::Task { done, text } => {
                out.push_str(if *done { "- [x] " } else { "- [ ] " });
                out.push_str(text);
            }
            Entry::Raw(s) => out.push_str(s),
        }
        out.push('\n');
    }
    out
}

/// Load the list, or an empty one if the file doesn't exist yet.
///
/// Path-injected rather than resolving `todos_path()` internally: a free
/// `save()` that reached for the real config dir meant every test touching the
/// dialog wrote into the developer's own to-do list. Making the path an
/// argument moves that decision to [`crate::app::App`], which holds `None`
/// under `cfg!(test)`.
pub fn load_from(path: &Path) -> Vec<Entry> {
    std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default()
}

/// Persist the list. Best-effort: a write failure is reported by the caller,
/// never fatal.
pub fn save_to(path: &Path, entries: &[Entry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, to_text(entries))
}

// ---- list operations --------------------------------------------------

/// Append a task to the end of the list.
pub fn add(entries: &mut Vec<Entry>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    entries.push(Entry::Task { done: false, text: text.to_string() });
}

/// Flip the checkbox at `idx`, if it's a task.
pub fn toggle(entries: &mut [Entry], idx: usize) {
    if let Some(Entry::Task { done, .. }) = entries.get_mut(idx) {
        *done = !*done;
    }
}

/// Drop the entry at `idx`.
pub fn remove(entries: &mut Vec<Entry>, idx: usize) {
    if idx < entries.len() {
        entries.remove(idx);
    }
}

/// Drop every completed task, leaving headings and notes alone.
pub fn clear_done(entries: &mut Vec<Entry>) -> usize {
    let before = entries.len();
    entries.retain(|e| !e.is_done());
    before - entries.len()
}

/// `(done, total)` task counts — the header line's summary.
pub fn counts(entries: &[Entry]) -> (usize, usize) {
    let total = entries.iter().filter(|e| e.is_task()).count();
    let done = entries.iter().filter(|e| e.is_done()).count();
    (done, total)
}

/// The rows the dialog actually draws: tasks and headings, in file order.
/// Blank lines are spacing in the file, not content, so they're skipped here —
/// they're still written back untouched.
pub fn visible_rows(entries: &[Entry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| match e {
            Entry::Task { .. } => true,
            Entry::Raw(s) => !s.trim().is_empty(),
        })
        .map(|(i, _)| i)
        .collect()
}

/// The next selectable entry at or after `from`, searching in `dir` (+1/-1).
/// Only tasks are selectable — you can't check off a heading.
pub fn next_task(entries: &[Entry], from: usize, dir: i32) -> Option<usize> {
    let n = entries.len();
    if n == 0 {
        return None;
    }
    let mut i = from as i64;
    for _ in 0..n {
        i += dir as i64;
        if i < 0 || i >= n as i64 {
            return None;
        }
        if entries[i as usize].is_task() {
            return Some(i as usize);
        }
    }
    None
}

/// The first task in the list, for placing the cursor when the dialog opens.
pub fn first_task(entries: &[Entry]) -> Option<usize> {
    entries.iter().position(Entry::is_task)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(done: bool, text: &str) -> Entry {
        Entry::Task { done, text: text.to_string() }
    }

    #[test]
    fn parses_every_checkbox_spelling() {
        let src = "- [x] a\n* [X] b\n[x] c\n- [ ] d\n- [] e\n[ ] f\n";
        assert_eq!(
            parse(src),
            vec![
                task(true, "a"),
                task(true, "b"),
                task(true, "c"),
                task(false, "d"),
                task(false, "e"),
                task(false, "f"),
            ]
        );
    }

    /// The headline feature: paste a plain list, get checkboxes.
    #[test]
    fn bare_lines_and_list_markers_become_unchecked_tasks() {
        let src = "buy milk\n- call sam\n* ship it\n+ water plants\n1. first\n2) second\n";
        assert_eq!(
            parse(src),
            vec![
                task(false, "buy milk"),
                task(false, "call sam"),
                task(false, "ship it"),
                task(false, "water plants"),
                task(false, "first"),
                task(false, "second"),
            ]
        );
    }

    #[test]
    fn headings_and_blank_lines_survive_untouched() {
        let src = "# Today\n\n- [ ] a\n\n## Later\n- [x] b\n";
        let entries = parse(src);
        assert_eq!(entries[0], Entry::Raw("# Today".into()));
        assert_eq!(entries[1], Entry::Raw(String::new()));
        assert_eq!(entries[4], Entry::Raw("## Later".into()));
        // …and writing puts them back exactly, so a toggle can't eat structure.
        assert_eq!(to_text(&entries), src);
    }

    #[test]
    fn writing_normalises_tasks_but_keeps_order() {
        let entries = parse("buy milk\n# Later\n* [X] done thing\n");
        assert_eq!(to_text(&entries), "- [ ] buy milk\n# Later\n- [x] done thing\n");
    }

    #[test]
    fn a_bracketed_word_is_prose_not_a_checkbox() {
        // `[TODO]` must not be read as a checkbox with the text "ODO".
        assert_eq!(parse("[TODO] ship it\n"), vec![task(false, "[TODO] ship it")]);
    }

    #[test]
    fn parse_write_round_trips() {
        let src = "# Today\n\n- [ ] a\n- [x] b\n\n# Notes\nplain line\n";
        let once = to_text(&parse(src));
        let twice = to_text(&parse(&once));
        assert_eq!(once, twice, "writing is idempotent");
    }

    #[test]
    fn add_toggle_remove_and_clear() {
        let mut e = parse("# H\n- [ ] a\n- [x] b\n");
        add(&mut e, "  c  ");
        assert_eq!(e.last(), Some(&task(false, "c")));
        add(&mut e, "   "); // blank input adds nothing
        assert_eq!(e.len(), 4);

        toggle(&mut e, 1);
        assert!(e[1].is_done());

        assert_eq!(counts(&e), (2, 3));
        let cleared = clear_done(&mut e);
        assert_eq!(cleared, 2);
        assert_eq!(counts(&e), (0, 1));
        assert_eq!(e[0], Entry::Raw("# H".into()), "the heading stays");
    }

    #[test]
    fn navigation_only_lands_on_tasks() {
        let e = parse("# H\n- [ ] a\nnote-as-task\n- [ ] c\n");
        // "note-as-task" parses as a task, so use a heading to prove skipping.
        let e2 = parse("- [ ] a\n# H\n- [ ] c\n");
        assert_eq!(first_task(&e), Some(1));
        assert_eq!(next_task(&e2, 0, 1), Some(2), "skips the heading");
        assert_eq!(next_task(&e2, 2, 1), None, "stops at the end");
        assert_eq!(next_task(&e2, 2, -1), Some(0));
        assert_eq!(next_task(&e2, 0, -1), None);
    }

    #[test]
    fn visible_rows_skip_blank_lines_only() {
        let e = parse("# H\n\n- [ ] a\n");
        assert_eq!(visible_rows(&e), vec![0, 2]);
    }

    #[test]
    fn load_save_round_trip_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxru/todos.md");
        assert!(load_from(&path).is_empty(), "a missing file is an empty list");

        let mut e = parse("- [ ] a\n");
        add(&mut e, "b");
        save_to(&path, &e).unwrap();
        assert_eq!(load_from(&path), e);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "- [ ] a\n- [ ] b\n");
    }
}
