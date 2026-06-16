//! Tree-sitter syntax highlighting, over a **drop-in language registry**.
//!
//! Each entry in [`LANGUAGES`] reuses an existing community grammar crate (the
//! same grammars every other tree-sitter editor uses) plus its bundled
//! highlight query. Adding a language is a two-step, no-glue change:
//!
//! 1. add the `tree-sitter-<lang>` crate to `Cargo.toml`;
//! 2. add one [`Lang`] entry below pointing at its `LANGUAGE` and query consts.
//!
//! Built [`HighlightConfiguration`]s are cached per language (building one parses
//! the query, which we only want to do once), and highlighting is best-effort:
//! anything unsupported or malformed falls back to plain, unstyled lines so the
//! editor never breaks because of the highlighter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use ratatui::style::Style;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

use crate::theme::Theme;

/// A registered language: its display name, the file extensions that select it,
/// and a builder that produces its (cached) highlight configuration.
#[derive(Debug)]
pub struct Lang {
    pub name: &'static str,
    extensions: &'static [&'static str],
    build: fn() -> Option<HighlightConfiguration>,
}

impl Lang {
    /// Find the language for a path by extension, if any is registered.
    pub fn detect(path: &Path) -> Option<&'static Lang> {
        let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
        LANGUAGES.iter().find(|l| l.extensions.contains(&ext.as_str()))
    }
}

/// The shipped languages. Extend this list to teach Oxru a new grammar.
pub static LANGUAGES: &[Lang] = &[
    Lang { name: "Rust", extensions: &["rs"], build: cfg_rust },
    Lang { name: "JavaScript", extensions: &["js", "mjs", "cjs", "jsx"], build: cfg_javascript },
    Lang { name: "TypeScript", extensions: &["ts", "mts", "cts"], build: cfg_typescript },
    Lang { name: "TSX", extensions: &["tsx"], build: cfg_tsx },
    Lang { name: "Python", extensions: &["py", "pyw", "pyi"], build: cfg_python },
    Lang { name: "JSON", extensions: &["json"], build: cfg_json },
    Lang { name: "Go", extensions: &["go"], build: cfg_go },
    Lang { name: "C", extensions: &["c", "h"], build: cfg_c },
    Lang { name: "HTML", extensions: &["html", "htm"], build: cfg_html },
    Lang { name: "CSS", extensions: &["css"], build: cfg_css },
    Lang { name: "Shell", extensions: &["sh", "bash", "zsh"], build: cfg_bash },
    Lang { name: "TOML", extensions: &["toml"], build: cfg_toml },
];

/// Capture names we care about, in priority order. The index into this slice is
/// the `Highlight` id handed back by tree-sitter-highlight, and the name is what
/// [`Theme::syntax_style`] maps to a colour.
const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "function",
    "function.macro",
    "function.method",
    "keyword",
    "label",
    "operator",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

/// One styled run of text. `text` never contains a newline.
pub type Span = (String, Style);

/// Highlight `source` for `lang` using `theme` for colours, returning one
/// `Vec<Span>` per line. `None` language (or any failure) yields plain lines.
pub fn highlight(source: &str, lang: Option<&'static Lang>, theme: &Theme) -> Vec<Vec<Span>> {
    let Some(lang) = lang else {
        return plain(source);
    };
    CONFIGS.with(|cache| {
        let mut map = cache.borrow_mut();
        if !map.contains_key(lang.name) {
            match (lang.build)() {
                Some(cfg) => {
                    map.insert(lang.name, cfg);
                }
                None => return plain(source),
            }
        }
        let cfg = map.get(lang.name).expect("just inserted");
        highlight_with(cfg, source, theme).unwrap_or_else(|| plain(source))
    })
}

thread_local! {
    /// Per-language highlight configs, built lazily on first use.
    static CONFIGS: RefCell<HashMap<&'static str, HighlightConfiguration>> =
        RefCell::new(HashMap::new());
}

fn plain(source: &str) -> Vec<Vec<Span>> {
    let mut lines: Vec<Vec<Span>> = source
        .split('\n')
        .map(|l| {
            let l = l.strip_suffix('\r').unwrap_or(l);
            vec![(l.to_string(), Style::default())]
        })
        .collect();
    if lines.is_empty() {
        lines.push(vec![(String::new(), Style::default())]);
    }
    lines
}

/// Build a configured `HighlightConfiguration` from a grammar + its queries.
fn make(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
    injections: &str,
    locals: &str,
) -> Option<HighlightConfiguration> {
    let mut cfg = HighlightConfiguration::new(language, name, highlights, injections, locals).ok()?;
    cfg.configure(HIGHLIGHT_NAMES);
    Some(cfg)
}

// --- per-grammar builders (the only place crate-specific const names live) ---

fn cfg_rust() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_rust::LANGUAGE.into(),
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn cfg_javascript() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_javascript::LANGUAGE.into(),
        "javascript",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::INJECTIONS_QUERY,
        tree_sitter_javascript::LOCALS_QUERY,
    )
}

fn cfg_typescript() -> Option<HighlightConfiguration> {
    // The TypeScript highlights are an *addition* to the JavaScript ones.
    let highlights = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    );
    make(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "typescript",
        &highlights,
        "",
        tree_sitter_typescript::LOCALS_QUERY,
    )
}

fn cfg_tsx() -> Option<HighlightConfiguration> {
    let highlights = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    );
    make(
        tree_sitter_typescript::LANGUAGE_TSX.into(),
        "tsx",
        &highlights,
        "",
        tree_sitter_typescript::LOCALS_QUERY,
    )
}

fn cfg_python() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_python::LANGUAGE.into(),
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn cfg_json() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_json::LANGUAGE.into(),
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn cfg_go() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_go::LANGUAGE.into(),
        "go",
        tree_sitter_go::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn cfg_c() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_c::LANGUAGE.into(),
        "c",
        tree_sitter_c::HIGHLIGHT_QUERY,
        "",
        "",
    )
}

fn cfg_html() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_html::LANGUAGE.into(),
        "html",
        tree_sitter_html::HIGHLIGHTS_QUERY,
        tree_sitter_html::INJECTIONS_QUERY,
        "",
    )
}

fn cfg_css() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_css::LANGUAGE.into(),
        "css",
        tree_sitter_css::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn cfg_bash() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_bash::LANGUAGE.into(),
        "bash",
        tree_sitter_bash::HIGHLIGHT_QUERY,
        "",
        "",
    )
}

fn cfg_toml() -> Option<HighlightConfiguration> {
    make(
        tree_sitter_toml_ng::LANGUAGE.into(),
        "toml",
        tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        "",
        "",
    )
}

fn highlight_with(
    cfg: &HighlightConfiguration,
    source: &str,
    theme: &Theme,
) -> Option<Vec<Vec<Span>>> {
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(cfg, source.as_bytes(), None, |_| None)
        .ok()?;

    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut stack: Vec<usize> = Vec::new();

    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(h) => stack.push(h.0),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                let style = stack
                    .last()
                    .map(|&i| theme.syntax_style(HIGHLIGHT_NAMES[i]))
                    .unwrap_or_default();
                let text = &source[start..end];
                push_text(&mut lines, text, style);
            }
        }
    }
    Some(lines)
}

/// Append `text` (which may contain newlines) to `lines`, splitting into new
/// rows on each `\n` and carrying `style`.
fn push_text(lines: &mut Vec<Vec<Span>>, text: &str, style: Style) {
    let mut first = true;
    for piece in text.split('\n') {
        if !first {
            lines.push(Vec::new());
        }
        first = false;
        let piece = piece.strip_suffix('\r').unwrap_or(piece);
        if !piece.is_empty() {
            lines.last_mut().unwrap().push((piece.to_string(), style));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_extensions() {
        assert_eq!(Lang::detect(Path::new("a.rs")).map(|l| l.name), Some("Rust"));
        assert_eq!(Lang::detect(Path::new("a.py")).map(|l| l.name), Some("Python"));
        assert_eq!(Lang::detect(Path::new("a.ts")).map(|l| l.name), Some("TypeScript"));
        assert_eq!(Lang::detect(Path::new("a.json")).map(|l| l.name), Some("JSON"));
        assert!(Lang::detect(Path::new("a.txt")).is_none());
        assert!(Lang::detect(Path::new("README")).is_none());
    }

    #[test]
    fn plain_line_count_matches() {
        let out = highlight("one\ntwo\nthree", None, &Theme::default());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0][0].0, "one");
        assert_eq!(out[2][0].0, "three");
    }

    #[test]
    fn rust_highlights_and_preserves_text() {
        let src = "fn main() {\n    let x = 1;\n}\n";
        let lang = Lang::detect(Path::new("m.rs"));
        let out = highlight(src, lang, &Theme::default());
        let rebuilt: String = out
            .iter()
            .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(rebuilt, src);
        let has_keyword = out
            .iter()
            .flatten()
            .any(|(t, s)| t == "fn" && *s != Style::default());
        assert!(has_keyword, "expected `fn` to be highlighted as a keyword");
    }

    #[test]
    fn every_registered_grammar_builds_and_preserves_text() {
        // A grammar that fails to build (e.g. ABI mismatch) would silently fall
        // back to plain; assert each actually highlights *and* round-trips text.
        let samples = [
            ("x.py", "def f():\n    return 1\n"),
            ("x.js", "const x = 1;\n"),
            ("x.ts", "let x: number = 1;\n"),
            ("x.go", "package main\nfunc main() {}\n"),
            ("x.c", "int main(void) { return 0; }\n"),
            ("x.json", "{\n  \"a\": 1\n}\n"),
            ("x.css", "a { color: red; }\n"),
            ("x.html", "<p>hi</p>\n"),
            ("x.sh", "echo hi\n"),
            ("x.toml", "a = 1\n"),
        ];
        for (file, src) in samples {
            let lang = Lang::detect(Path::new(file)).unwrap_or_else(|| panic!("no lang for {file}"));
            let out = highlight(src, Some(lang), &Theme::default());
            let rebuilt: String = out
                .iter()
                .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(rebuilt, src, "{} text must round-trip", lang.name);
            let styled = out.iter().flatten().any(|(_, s)| *s != Style::default());
            assert!(styled, "{} should produce at least one styled span", lang.name);
        }
    }
}
