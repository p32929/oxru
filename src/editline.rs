//! Shared single-line text editing for the app's small inputs — the find bar,
//! the file-dialog query, and the name prompt. Each of those keeps its own
//! `String`; this module supplies one consistent set of cursor / word / selection
//! behaviors over a borrowed `(text, cursor, anchor)` triple so editing those
//! one-line fields feels the same as editing the main buffer:
//!
//!   * arrow / Home / End motion with Shift extending a selection,
//!   * Option/Alt + Left/Right word motion (and Option+Backspace word delete),
//!   * forward Delete, select-all, and selection-aware insert / backspace / paste.
//!
//! `cursor` and `anchor` are **char** indices into `text` (not byte offsets), so
//! callers index columns directly; the byte conversions stay in here.

/// Words are runs of alphanumerics / underscores; everything else is a gap —
/// the same rule the editor buffer uses for Option+Arrow.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Number of chars in `text` (the max valid cursor position).
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// Byte offset of char index `i`, clamped to the end of the string.
fn byte_at(text: &str, i: usize) -> usize {
    text.char_indices().nth(i).map(|(b, _)| b).unwrap_or(text.len())
}

/// The selected `(start, end)` char range in reading order, or `None` when the
/// anchor is unset or collapsed onto the cursor.
pub fn selection(cursor: usize, anchor: Option<usize>) -> Option<(usize, usize)> {
    let a = anchor?;
    if a == cursor {
        return None;
    }
    Some((a.min(cursor), a.max(cursor)))
}

/// The selected substring, if any.
pub fn selected_text(text: &str, cursor: usize, anchor: Option<usize>) -> Option<String> {
    let (s, e) = selection(cursor, anchor)?;
    Some(text[byte_at(text, s)..byte_at(text, e)].to_string())
}

/// Delete the active selection (collapsing the cursor onto its start). Returns
/// whether anything was removed.
pub fn delete_selection(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>) -> bool {
    if let Some((s, e)) = selection(*cursor, *anchor) {
        let (bs, be) = (byte_at(text, s), byte_at(text, e));
        text.replace_range(bs..be, "");
        *cursor = s;
        *anchor = None;
        true
    } else {
        false
    }
}

/// Insert a char at the cursor, replacing any selection first.
pub fn insert(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>, c: char) {
    delete_selection(text, cursor, anchor);
    let b = byte_at(text, *cursor);
    text.insert(b, c);
    *cursor += 1;
    *anchor = None;
}

/// Insert a string at the cursor, replacing any selection first (paste).
pub fn insert_str(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>, s: &str) {
    delete_selection(text, cursor, anchor);
    let b = byte_at(text, *cursor);
    text.insert_str(b, s);
    *cursor += s.chars().count();
    *anchor = None;
}

/// Backspace: delete the selection, or the char before the cursor.
pub fn backspace(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>) {
    if delete_selection(text, cursor, anchor) || *cursor == 0 {
        return;
    }
    let start = *cursor - 1;
    let (bs, be) = (byte_at(text, start), byte_at(text, *cursor));
    text.replace_range(bs..be, "");
    *cursor = start;
}

/// Forward Delete: delete the selection, or the char at the cursor.
pub fn delete_fwd(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>) {
    if delete_selection(text, cursor, anchor) {
        return;
    }
    if *cursor >= char_len(text) {
        return;
    }
    let (bs, be) = (byte_at(text, *cursor), byte_at(text, *cursor + 1));
    text.replace_range(bs..be, "");
}

/// Prepare a cursor move: with Shift, anchor the selection here if not already;
/// without Shift, drop any selection.
fn pre_move(cursor: usize, anchor: &mut Option<usize>, shift: bool) {
    if shift {
        if anchor.is_none() {
            *anchor = Some(cursor);
        }
    } else {
        *anchor = None;
    }
}

pub fn left(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    let _ = text;
    pre_move(*cursor, anchor, shift);
    if *cursor > 0 {
        *cursor -= 1;
    }
}

pub fn right(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    pre_move(*cursor, anchor, shift);
    if *cursor < char_len(text) {
        *cursor += 1;
    }
}

pub fn home(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    let _ = text;
    pre_move(*cursor, anchor, shift);
    *cursor = 0;
}

pub fn end(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    pre_move(*cursor, anchor, shift);
    *cursor = char_len(text);
}

/// Char index of the start of the word at/just before `i` (Option+Left).
fn prev_word(chars: &[char], mut i: usize) -> usize {
    while i > 0 && !is_word_char(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word_char(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// Char index of the end of the word at/just after `i` (Option+Right).
fn next_word(chars: &[char], mut i: usize) -> usize {
    let n = chars.len();
    while i < n && !is_word_char(chars[i]) {
        i += 1;
    }
    while i < n && is_word_char(chars[i]) {
        i += 1;
    }
    i
}

pub fn word_left(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    pre_move(*cursor, anchor, shift);
    let chars: Vec<char> = text.chars().collect();
    *cursor = prev_word(&chars, (*cursor).min(chars.len()));
}

pub fn word_right(text: &str, cursor: &mut usize, anchor: &mut Option<usize>, shift: bool) {
    pre_move(*cursor, anchor, shift);
    let chars: Vec<char> = text.chars().collect();
    *cursor = next_word(&chars, (*cursor).min(chars.len()));
}

/// Option+Backspace: delete the selection, or the word to the left.
pub fn delete_word_left(text: &mut String, cursor: &mut usize, anchor: &mut Option<usize>) {
    if delete_selection(text, cursor, anchor) {
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let start = prev_word(&chars, (*cursor).min(chars.len()));
    if start == *cursor {
        return;
    }
    let (bs, be) = (byte_at(text, start), byte_at(text, *cursor));
    text.replace_range(bs..be, "");
    *cursor = start;
}

/// Select the whole field (cursor to the end, anchor at the start).
pub fn select_all(text: &str, cursor: &mut usize, anchor: &mut Option<usize>) {
    *anchor = Some(0);
    *cursor = char_len(text);
}

/// Clamp the cursor/anchor back into range after `text` was replaced wholesale
/// (e.g. the find bar prefilled from a selection).
pub fn clamp(text: &str, cursor: &mut usize, anchor: &mut Option<usize>) {
    let n = char_len(text);
    if *cursor > n {
        *cursor = n;
    }
    if let Some(a) = *anchor {
        if a > n {
            *anchor = Some(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(s: &str, cur: usize) -> (String, usize, Option<usize>) {
        (s.to_string(), cur, None)
    }

    #[test]
    fn insert_and_backspace_mid_string() {
        let (mut t, mut c, mut a) = st("helo", 3);
        insert(&mut t, &mut c, &mut a, 'l');
        assert_eq!(t, "hello");
        assert_eq!(c, 4);
        left(&t, &mut c, &mut a, false); // -> 3
        backspace(&mut t, &mut c, &mut a); // remove the 'l' before cursor
        assert_eq!(t, "helo");
        assert_eq!(c, 2);
    }

    #[test]
    fn word_motion_and_delete() {
        let (mut t, mut c, mut a) = st("foo bar baz", 11);
        word_left(&t, &mut c, &mut a, false);
        assert_eq!(c, 8); // start of "baz"
        word_left(&t, &mut c, &mut a, false);
        assert_eq!(c, 4); // start of "bar"
        // Option+Backspace here deletes the word to the LEFT of the cursor ("foo ").
        delete_word_left(&mut t, &mut c, &mut a);
        assert_eq!(t, "bar baz");
        assert_eq!(c, 0);
    }

    #[test]
    fn shift_selection_then_type_replaces() {
        let (mut t, mut c, mut a) = st("hello", 0);
        end(&t, &mut c, &mut a, true); // select all via shift+end
        assert_eq!(selection(c, a), Some((0, 5)));
        insert(&mut t, &mut c, &mut a, 'x');
        assert_eq!(t, "x");
        assert_eq!(c, 1);
    }

    #[test]
    fn select_all_and_delete_forward() {
        let (mut t, mut c, mut a) = st("abc", 1);
        select_all(&t, &mut c, &mut a);
        assert_eq!(selected_text(&t, c, a).as_deref(), Some("abc"));
        delete_fwd(&mut t, &mut c, &mut a);
        assert_eq!(t, "");
        assert_eq!(c, 0);
    }

    #[test]
    fn unicode_is_char_indexed() {
        let (mut t, mut c, mut a) = st("café", 4);
        backspace(&mut t, &mut c, &mut a);
        assert_eq!(t, "caf");
        assert_eq!(c, 3);
    }
}

// ---------------------------------------------------------------------------
// One key handler for every one-line field in the app.
// ---------------------------------------------------------------------------

/// A borrowed one-line field: the three pieces every text box in the app keeps.
///
/// Borrowed rather than owned so the fields can stay where they live (on
/// `find`, `prompt`, `file_dialog`, …) — this exists to share the *behaviour*,
/// not to move the storage.
pub struct FieldRef<'a> {
    pub text: &'a mut String,
    pub cursor: &'a mut usize,
    pub anchor: &'a mut Option<usize>,
}

/// What a key did to a field. The clipboard variants are requests the caller
/// services: [`handle_key`] can't reach the system clipboard, and pushing that
/// dependency down here would drag `App` into this module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edit {
    /// Not a field key — the caller should keep looking.
    Ignored,
    /// The cursor or selection moved; the text is unchanged.
    Moved,
    /// The text changed, so the caller should re-run its search / filter.
    Changed,
    /// Copy the selection.
    Copy,
    /// Copy the selection, then delete it.
    Cut,
    /// Insert the clipboard at the cursor.
    Paste,
}

/// Run one key through a one-line field.
///
/// This is the whole editing behaviour of every text box in Oxru, in one place:
/// word / line / char motion, selection extension with Shift, the deletes, and
/// select-all. It used to be a twenty-arm `match` copy-pasted into six key
/// handlers, differing only in which three fields they named — so a fix to one
/// text box reached one text box.
///
/// Keys the caller wants for itself (Enter, the arrows that drive a list, a
/// Backspace that means "leave this folder") must be matched *before* this is
/// reached; it deliberately claims everything it recognises.
pub fn handle_key(
    f: FieldRef<'_>,
    key: ratatui::crossterm::event::KeyEvent,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Edit {
    use ratatui::crossterm::event::KeyCode;
    let FieldRef { text, cursor, anchor } = f;
    match key.code {
        // Motion: Option+Arrow by word, ⌘/Ctrl+Arrow to the ends, plain arrows
        // by char, Home/End to the ends — Shift extends the selection on all.
        KeyCode::Left if alt => { word_left(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Right if alt => { word_right(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Left if ctrl => { home(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Right if ctrl => { end(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Left => { left(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Right => { right(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Home => { home(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::End => { end(text, cursor, anchor, shift); Edit::Moved }
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            select_all(text, cursor, anchor);
            Edit::Moved
        }
        // Clipboard — the caller owns the actual clipboard.
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => Edit::Copy,
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => Edit::Cut,
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => Edit::Paste,
        // Editing.
        KeyCode::Backspace if alt => { delete_word_left(text, cursor, anchor); Edit::Changed }
        KeyCode::Backspace => { backspace(text, cursor, anchor); Edit::Changed }
        KeyCode::Delete => { delete_fwd(text, cursor, anchor); Edit::Changed }
        KeyCode::Char(c) if !ctrl && !alt => { insert(text, cursor, anchor, c); Edit::Changed }
        _ => Edit::Ignored,
    }
}
