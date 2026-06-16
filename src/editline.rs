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
