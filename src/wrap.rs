//! Soft word-wrap: pure text-layout math, kept separate from `Buffer` so the
//! wrapping algorithm itself is testable without a rope.
//!
//! VSCode-style: greedily pack whole words onto a row, breaking at a run of
//! whitespace when possible; a single word (or whitespace run) longer than
//! the width gets a hard character break as a last resort, since there's no
//! other way to show it at all.

/// Word-wrap `chars` (one rope line's content, already stripped of its
/// trailing newline) to `width` character cells, returning `[start, end)`
/// index ranges relative to the start of `chars`. Always returns at least one
/// range — an empty line yields `[(0, 0)]`, and a line that already fits
/// yields a single range covering it all (so callers don't need a special
/// case for "didn't need to wrap").
pub fn wrap_line_ranges(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    if width == 0 || n <= width {
        return vec![(0, n)];
    }

    // Tokenize into alternating whitespace / non-whitespace runs so a break
    // only ever lands at a token boundary (never mid-word) unless a single
    // token itself doesn't fit on an empty row.
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        let start = i;
        let is_ws = chars[i].is_whitespace();
        while i < n && chars[i].is_whitespace() == is_ws {
            i += 1;
        }
        tokens.push((start, i));
    }

    let mut ranges = Vec::new();
    let mut row_start = 0usize;
    let mut row_end = 0usize; // exclusive end of what's placed on the current row so far

    let hard_break = |ranges: &mut Vec<(usize, usize)>, mut s: usize, e: usize| -> usize {
        // Split a token that alone exceeds `width` into `width`-sized chunks,
        // pushing all but the last chunk (the caller keeps building on that).
        while e - s > width {
            ranges.push((s, s + width));
            s += width;
        }
        s
    };

    for &(ts, te) in &tokens {
        let tok_len = te - ts;
        let is_ws = chars[ts].is_whitespace();
        let cur_len = row_end - row_start;
        if cur_len == 0 {
            // First token on an otherwise-empty row: place it unconditionally
            // (hard-breaking it first if it alone is too wide).
            row_start = hard_break(&mut ranges, ts, te);
            row_end = te;
            continue;
        }
        if cur_len + tok_len <= width {
            row_end = te;
        } else if is_ws {
            // A whitespace run that doesn't fit stays attached to the row
            // it's closing out (even if that makes the row a little wider
            // than `width`) rather than starting the next row with leading
            // whitespace — the same convention VSCode uses.
            ranges.push((row_start, te));
            row_start = te;
            row_end = te;
        } else {
            // A word that doesn't fit: close the row exactly where it stood,
            // then start a new row with this word.
            ranges.push((row_start, row_end));
            row_start = hard_break(&mut ranges, ts, te);
            row_end = te;
        }
    }
    ranges.push((row_start, row_end.max(row_start)));
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn fits_on_one_row_unchanged() {
        let c = chars("hello world");
        assert_eq!(wrap_line_ranges(&c, 80), vec![(0, 11)]);
    }

    #[test]
    fn empty_line_yields_one_empty_range() {
        let c: Vec<char> = Vec::new();
        assert_eq!(wrap_line_ranges(&c, 10), vec![(0, 0)]);
    }

    #[test]
    fn zero_width_never_wraps() {
        let c = chars("hello world");
        assert_eq!(wrap_line_ranges(&c, 0), vec![(0, 11)]);
    }

    #[test]
    fn breaks_at_word_boundary_not_mid_word() {
        // "hello world foo" at width 11: "hello world" (11) exactly fills the
        // row; the following space doesn't fit and stays attached to it
        // (trailing-whitespace convention), so "foo" starts the next row.
        let c = chars("hello world foo");
        let ranges = wrap_line_ranges(&c, 11);
        assert_eq!(ranges.len(), 2);
        let row1: String = c[ranges[0].0..ranges[0].1].iter().collect();
        let row2: String = c[ranges[1].0..ranges[1].1].iter().collect();
        assert_eq!(row1, "hello world ");
        assert_eq!(row2, "foo");
    }

    #[test]
    fn trailing_whitespace_stays_with_the_row_it_closes() {
        // "aa bb" at width 3: "aa " fits exactly (3), "bb" goes to next row.
        let c = chars("aa bb");
        let ranges = wrap_line_ranges(&c, 3);
        let row1: String = c[ranges[0].0..ranges[0].1].iter().collect();
        let row2: String = c[ranges[1].0..ranges[1].1].iter().collect();
        assert_eq!(row1, "aa ");
        assert_eq!(row2, "bb");
    }

    #[test]
    fn hard_breaks_a_single_word_longer_than_width() {
        let c = chars("supercalifragilisticexpialidocious");
        let ranges = wrap_line_ranges(&c, 10);
        assert!(ranges.len() > 1);
        for &(s, e) in &ranges {
            assert!(e - s <= 10, "no row should exceed the width");
        }
        // Ranges are contiguous and cover the whole line.
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, c.len());
        for w in ranges.windows(2) {
            assert_eq!(w[0].1, w[1].0, "ranges must be contiguous");
        }
    }

    #[test]
    fn ranges_always_cover_the_whole_line_contiguously() {
        for text in ["", "x", "a b c d e f g h i j k", "    leading spaces", "trailing   "] {
            let c = chars(text);
            for width in [1, 2, 3, 5, 10, 100] {
                let ranges = wrap_line_ranges(&c, width);
                assert!(!ranges.is_empty());
                assert_eq!(ranges[0].0, 0, "text={text:?} width={width}");
                assert_eq!(ranges.last().unwrap().1, c.len(), "text={text:?} width={width}");
                for w in ranges.windows(2) {
                    assert_eq!(w[0].1, w[1].0, "text={text:?} width={width}");
                }
            }
        }
    }
}
