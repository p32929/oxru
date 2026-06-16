//! File explorer model: a flat, lazily-expanded list of entries that mirrors a
//! collapsible directory tree (VSCode Explorer style).

use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

/// Hidden from BOTH the tree and search, always — pure VCS / tool / OS internals
/// that nobody opens or edits.
const ALWAYS_HIDDEN: &[&str] = &[".git", ".hg", ".svn", ".oxru", ".DS_Store"];

/// The universal heavy build / dependency / cache dirs. **Shown in the explorer
/// tree** (like VSCode — you can still browse into `node_modules`), but pruned
/// from **search** by default so Quick Open shows your source, not 30k build
/// artifacts. A fixed, well-known list applied regardless of `.gitignore` (so it
/// works even outside a git repo). The user can flip these back into search with
/// the in-dialog toggle (⌥H) — see `App::dialog_show_junk`.
const SEARCH_PRUNE: &[&str] = &[
    // IDE metadata
    ".idea",
    // JS / TS / web build, deps & caches
    "node_modules", "bower_components", "dist", "build", "out", ".next",
    ".nuxt", ".svelte-kit", ".turbo", ".parcel-cache", ".cache", ".output",
    "coverage",
    // Rust
    "target",
    // Python
    "__pycache__", ".venv", "venv", ".mypy_cache", ".pytest_cache",
    ".ruff_cache", ".tox",
    // Dart / Flutter
    ".dart_tool",
    // JVM / Gradle / Android
    ".gradle",
    // Apple / CocoaPods
    "Pods", "DerivedData",
    // Infra
    ".terraform",
];

/// Cap on files collected for Quick Open. With [`SEARCH_PRUNE`] removing the
/// heavy dirs this is rarely approached; if it ever is, we log a warning rather
/// than silently dropping files (which would look like "search can't find it").
const MAX_FILES: usize = 50_000;

/// Decides which entries to show, and which to fade. Built once per root and
/// consulted while walking the tree. Like VSCode's Explorer: VCS / tool
/// internals and the heavy build dirs are hidden outright, while files the
/// project `.gitignore`s (e.g. `.env`, `dist/`) are still **shown but dimmed**.
pub struct Filter {
    gitignore: Option<Gitignore>,
}

impl Filter {
    pub fn new(root: &Path) -> Self {
        Filter {
            gitignore: build_gitignore(root),
        }
    }

    /// Whether `name` should be hidden from the explorer entirely — only VCS /
    /// tool internals. The heavy build / dependency dirs (`node_modules`, …) are
    /// shown in the tree like VSCode (and only pruned from *search*); gitignored
    /// files are shown dimmed (see [`Filter::ignored`]).
    fn hidden(&self, name: &str) -> bool {
        ALWAYS_HIDDEN.contains(&name)
    }

    /// Whether `path` is gitignored — shown, but faded in the explorer (matching
    /// VSCode, which greys out ignored files rather than hiding them).
    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        self.gitignore
            .as_ref()
            .is_some_and(|gi| gi.matched_path_or_any_parents(path, is_dir).is_ignore())
    }
}

/// Build a gitignore matcher from the project's root `.gitignore` (if any).
fn build_gitignore(root: &Path) -> Option<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    // add() returns Some(error) on failure; ignore it (a missing file is fine).
    b.add(root.join(".gitignore"));
    b.build().ok()
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// Gitignored — shown but rendered dimmed (VSCode Explorer behavior).
    pub ignored: bool,
}

pub struct FileTree {
    #[allow(dead_code)] // retained for future "reveal in explorer" / refresh
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    filter: Filter,
}

impl FileTree {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let filter = Filter::new(&root);
        let entries = build_root_entries(&root, &filter);
        FileTree {
            root,
            entries,
            selected: 0,
            filter,
        }
    }

    /// An empty tree for the no-folder (welcome) state.
    pub fn empty() -> Self {
        FileTree {
            root: PathBuf::new(),
            entries: Vec::new(),
            selected: 0,
            filter: Filter::new(Path::new("")),
        }
    }

    /// The selected entry as `(path, is_dir)`.
    pub fn selected(&self) -> Option<(PathBuf, bool)> {
        self.entries.get(self.selected).map(|e| (e.path.clone(), e.is_dir))
    }

    /// Expand the selected directory (no-op for files / already expanded).
    pub fn expand_selected(&mut self) {
        let i = self.selected;
        if let Some(e) = self.entries.get(i) {
            if e.is_dir && !e.expanded {
                self.expand(i);
            }
        }
    }

    /// Collapse the selected directory (no-op for files / already collapsed).
    pub fn collapse_selected(&mut self) {
        let i = self.selected;
        if let Some(e) = self.entries.get(i) {
            if e.is_dir && e.expanded {
                self.collapse(i);
            }
        }
    }

    /// Re-read the tree from disk, preserving which folders are expanded and
    /// (by path) the current selection. Call after creating / renaming /
    /// deleting entries.
    pub fn refresh(&mut self) {
        let expanded: std::collections::HashSet<PathBuf> = self
            .entries
            .iter()
            .filter(|e| e.is_dir && e.expanded)
            .map(|e| e.path.clone())
            .collect();
        let selected_path = self.entries.get(self.selected).map(|e| e.path.clone());

        self.entries = build_root_entries(&self.root, &self.filter);
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].is_dir
                && !self.entries[i].expanded
                && expanded.contains(&self.entries[i].path)
            {
                self.expand(i);
            }
            i += 1;
        }
        self.selected = selected_path
            .and_then(|p| self.entries.iter().position(|e| e.path == p))
            .unwrap_or(0)
            .min(self.entries.len().saturating_sub(1));
    }

    /// Move up, wrapping from the first row to the last.
    pub fn move_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.entries.len() - 1
        } else {
            self.selected - 1
        };
    }

    /// Move down, wrapping from the last row to the first.
    pub fn move_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.entries.len();
    }

    fn expand(&mut self, idx: usize) {
        let (path, depth) = {
            let e = &self.entries[idx];
            (e.path.clone(), e.depth)
        };
        let children = read_dir_entries(&path, depth + 1, &self.filter);
        self.entries[idx].expanded = true;
        // Insert children right after the directory entry.
        for (offset, child) in children.into_iter().enumerate() {
            self.entries.insert(idx + 1 + offset, child);
        }
    }

    fn collapse(&mut self, idx: usize) {
        let depth = self.entries[idx].depth;
        self.entries[idx].expanded = false;
        let mut end = idx + 1;
        while end < self.entries.len() && self.entries[end].depth > depth {
            end += 1;
        }
        self.entries.drain(idx + 1..end);
    }
}

/// The initial entry list: the root folder itself as an expanded node at the
/// top, with its contents nested beneath it at depth 1. Selecting the root node
/// makes "create at the project root" obvious.
fn build_root_entries(root: &Path, filter: &Filter) -> Vec<Entry> {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let mut entries = vec![Entry {
        path: root.to_path_buf(),
        name,
        depth: 0,
        is_dir: true,
        expanded: true,
        ignored: false,
    }];
    entries.extend(read_dir_entries(root, 1, filter));
    entries
}

fn read_dir_entries(dir: &Path, depth: usize, filter: &Filter) -> Vec<Entry> {
    let mut entries: Vec<Entry> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|de| {
                let path = de.path();
                let name = de.file_name().to_string_lossy().into_owned();
                let is_dir = de.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if filter.hidden(&name) {
                    return None;
                }
                let ignored = filter.ignored(&path, is_dir);
                Some(Entry {
                    path,
                    name,
                    depth,
                    is_dir,
                    expanded: false,
                    ignored,
                })
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    // Directories first, then alphabetical (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
}

/// Collect all files under `root` for Quick Open. Individual gitignored files
/// (`.env`, …) are **included** so they're findable — only the well-known junk
/// dirs in [`SEARCH_PRUNE`] (`node_modules`, `build`, `.dart_tool`, …) are dropped
/// outright, so search shows your source and not a sea of build artifacts. The
/// browse/search UI fades the gitignored ones; see [`collect_files_marked`].
///
/// `include_junk` keeps the [`SEARCH_PRUNE`] dirs (`node_modules`, `build`, …)
/// in the result — the ⌥H "show everything" toggle; the default `false` prunes
/// them. Bounded by [`MAX_FILES`] to keep huge trees snappy.
pub fn collect_files(root: &Path, include_junk: bool) -> Vec<PathBuf> {
    collect_walk(root, false, include_junk)
}

/// Like [`collect_files`], but each file is flagged `true` when it is gitignored
/// — so search results can fade them the way the explorer fades ignored entries.
/// Computed by diffing the full walk against the gitignore-respecting walk (which
/// handles nested `.gitignore`s correctly).
pub fn collect_files_marked(root: &Path, include_junk: bool) -> Vec<(PathBuf, bool)> {
    use std::collections::HashSet;
    let tracked: HashSet<PathBuf> = collect_walk(root, true, include_junk).into_iter().collect();
    collect_walk(root, false, include_junk)
        .into_iter()
        .map(|p| {
            let ignored = !tracked.contains(&p);
            (p, ignored)
        })
        .collect()
}

/// Walk `root` for files. With `respect_gitignore` the project's `.gitignore`s
/// (root + nested) and `.ignore` / `.git/info/exclude` are honoured; without it,
/// every file is yielded. Either way: dotfiles are shown, the user's *global* and
/// any *parent* repo's ignores are not consulted, and VCS / tool internals plus
/// the heavy `node_modules` / `target` dirs are always pruned.
fn collect_walk(root: &Path, respect_gitignore: bool, include_junk: bool) -> Vec<PathBuf> {
    use ignore::WalkBuilder;

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false) // show dotfiles (.gitignore, .env, …) like VSCode does
        .git_ignore(respect_gitignore) // honour each project's .gitignore (root + nested)
        .git_global(false) // …but never the user's global gitignore
        .git_exclude(respect_gitignore) // …and the repo's .git/info/exclude
        .ignore(respect_gitignore) // …and .ignore files
        .parents(false) // …and never climb into a parent repo's ignore rules
        .require_git(false) // apply .gitignore even outside a git repo
        .filter_entry(move |e| {
            let name = e.file_name().to_str().unwrap_or("");
            // Always drop VCS / tool internals. Drop the heavy build / dep / cache
            // dirs too, unless the user asked to include them (the ⌥H toggle).
            if ALWAYS_HIDDEN.contains(&name) {
                return false;
            }
            include_junk || !SEARCH_PRUNE.contains(&name)
        });

    let mut out = Vec::new();
    let mut truncated = false;
    for entry in wb.build().flatten() {
        if out.len() >= MAX_FILES {
            truncated = true;
            break;
        }
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
        }
    }
    if truncated {
        // Don't let a silent cap masquerade as "search can't find my file".
        tracing::warn!(
            root = %root.display(),
            cap = MAX_FILES,
            "file walk hit the cap; some files are omitted from search"
        );
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("README.md"), "# hi").unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("target/junk"), "x").unwrap();
        dir
    }

    #[test]
    fn lists_root_and_shows_build_dirs_in_tree() {
        let dir = make_tree();
        let tree = FileTree::new(dir.path());
        let names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"src"));
        assert!(names.contains(&"README.md"));
        // Heavy build dirs are SHOWN in the tree (VSCode-style) — they're only
        // pruned from search. (.git etc. stay hidden; "target" is not.)
        assert!(names.contains(&"target"), "build dirs are browsable in the tree");
        // Entry 0 is the expanded root folder; its contents start at depth 1.
        assert_eq!(tree.entries[0].depth, 0);
        assert!(tree.entries[0].is_dir && tree.entries[0].expanded);
        assert_eq!(tree.entries[1].name, "src"); // dir sorts before file
        assert_eq!(tree.entries[1].depth, 1);
    }

    #[test]
    fn expand_and_collapse() {
        let dir = make_tree();
        let mut tree = FileTree::new(dir.path());
        // Select "src" (index 1, under the root node at index 0).
        tree.selected = tree.entries.iter().position(|e| e.name == "src").unwrap();
        tree.expand_selected();
        let names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"lib.rs"));
        // src's children sit one level deeper.
        let src_idx = tree.selected;
        assert_eq!(tree.entries[src_idx + 1].depth, 2);
        // Collapse removes them again.
        tree.collapse_selected();
        let names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"main.rs"));
    }

    #[test]
    fn collect_files_skips_ignored() {
        let dir = make_tree();
        let files = collect_files(dir.path(), false);
        assert!(files.iter().any(|p| p.ends_with("src/main.rs")));
        assert!(files.iter().any(|p| p.ends_with("README.md")));
        assert!(!files.iter().any(|p| p.to_string_lossy().contains("target")));
    }

    #[test]
    fn prunes_universal_junk_dirs_from_search() {
        // The well-known build / dep / cache dirs are dropped from search even
        // without a .gitignore — that's the "ignore the obvious junk" rule.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("main.rs"), "").unwrap();
        let junk = ["node_modules", "dist", "build", ".next", ".dart_tool", "target"];
        for d in junk {
            fs::create_dir_all(root.join(d)).unwrap();
            fs::write(root.join(d).join("artifact.js"), "").unwrap();
        }
        // Default (toggle off): the junk dirs are pruned from search.
        let files = collect_files(root, false);
        assert!(files.iter().any(|p| p.ends_with("main.rs")), "real source is found");
        for d in junk {
            assert!(
                !files.iter().any(|p| p.components().any(|c| c.as_os_str() == d)),
                "{d}/ should be pruned from search by default"
            );
        }

        // Toggle on (⌥H, include_junk = true): they come back.
        let all = collect_files(root, true);
        for d in junk {
            assert!(
                all.iter().any(|p| p.components().any(|c| c.as_os_str() == d)),
                "{d}/ should appear in search when junk is toggled on"
            );
        }
    }

    #[test]
    fn gitignored_files_show_but_are_flagged() {
        // New policy ("show everywhere, dimmed"): gitignored files are INCLUDED in
        // search so they're findable, but flagged so the UI can fade them.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Use a non-junk gitignored dir ("private/"), since the well-known build
        // dirs (build/, dist/, …) are now pruned outright regardless of gitignore.
        fs::write(root.join(".gitignore"), "secret.txt\nprivate/\n").unwrap();
        fs::write(root.join("keep.rs"), "").unwrap();
        fs::write(root.join("secret.txt"), "").unwrap();
        fs::create_dir(root.join("private")).unwrap();
        fs::write(root.join("private/out.dat"), "").unwrap();

        let files = collect_files(root, false);
        assert!(files.iter().any(|p| p.ends_with("keep.rs")));
        assert!(files.iter().any(|p| p.ends_with("secret.txt")), "gitignored file is findable");
        assert!(files.iter().any(|p| p.to_string_lossy().contains("private/")));

        // …and the marked variant flags the gitignored ones (and not the rest).
        let marked = collect_files_marked(root, false);
        let ig = |needle: &str| marked.iter().find(|(p, _)| p.ends_with(needle)).map(|(_, b)| *b);
        assert_eq!(ig("keep.rs"), Some(false), "tracked file not flagged");
        assert_eq!(ig("secret.txt"), Some(true), "gitignored file flagged");
        assert_eq!(ig("private/out.dat"), Some(true), "file under gitignored dir flagged");
    }

    #[test]
    fn gitignored_entry_shows_dimmed_in_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), ".env\n").unwrap();
        fs::write(root.join(".env"), "").unwrap();
        fs::write(root.join("main.rs"), "").unwrap();

        let tree = FileTree::new(root);
        let env = tree.entries.iter().find(|e| e.name == ".env").expect(".env is shown in the tree");
        assert!(env.ignored, ".env is flagged ignored (dimmed)");
        let main = tree.entries.iter().find(|e| e.name == "main.rs").unwrap();
        assert!(!main.ignored, "tracked file not flagged");
    }

    #[test]
    fn untracked_dotfiles_show_in_search() {
        // A dotfile that isn't gitignored (e.g. a `.env` the project tracks or
        // simply never ignored) must be findable — VSCode lists dotfiles.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::write(root.join(".env"), "").unwrap();
        fs::write(root.join("main.rs"), "").unwrap();

        let files = collect_files(root, false);
        assert!(files.iter().any(|p| p.ends_with(".env")));
        assert!(files.iter().any(|p| p.ends_with("main.rs")));
    }

    #[test]
    fn nested_gitignore_flags_subtree() {
        // A nested `.gitignore` (e.g. a Flutter `pkg/.gitignore` hiding `build/`)
        // still applies to its subtree — but now to FLAG, not hide: the files show
        // and are marked ignored so the UI fades them.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("app_constants.dart"), "").unwrap();
        fs::create_dir_all(root.join("pkg")).unwrap();
        // "gen/" rather than "build/": build/ is now a pruned junk dir, so use a
        // non-pruned name to test the nested-gitignore *flagging* path itself.
        fs::write(root.join("pkg/.gitignore"), "gen/\n").unwrap();
        fs::write(root.join("pkg/source.dart"), "").unwrap();
        fs::create_dir_all(root.join("pkg/gen")).unwrap();
        fs::write(root.join("pkg/gen/Generated.dart"), "").unwrap();

        let marked = collect_files_marked(root, false);
        let ig = |needle: &str| marked.iter().find(|(p, _)| p.ends_with(needle)).map(|(_, b)| *b);
        assert_eq!(ig("app_constants.dart"), Some(false));
        assert_eq!(ig("pkg/source.dart"), Some(false));
        assert_eq!(
            ig("pkg/gen/Generated.dart"),
            Some(true),
            "file under nested-gitignored pkg/gen/ is flagged, got {marked:?}"
        );
    }
}
