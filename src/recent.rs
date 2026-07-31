//! Recently-opened project folders, persisted across launches.
//!
//! Each Oxru process opens exactly one folder; this list accumulates those
//! folders (most-recent first) so the "Recent folders" dialog can offer them
//! again — including opening several at once, each in its own window.

use std::path::{Path, PathBuf};

/// How many recent folders to remember.
const MAX: usize = 20;

/// `~/.config/oxru/recent` (or the platform config dir), where the list lives.
fn recent_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxru/recent"))
}

/// Record `folder` as the most-recently-opened (de-duplicated, capped). Silent
/// on any I/O error — a missing recents file just means an empty list.
pub fn record(folder: &Path) {
    if let Some(path) = recent_path() {
        let canon = folder.canonicalize().unwrap_or_else(|_| folder.to_path_buf());
        record_to(&path, &canon);
    }
}

/// The current recent list, newest first, with entries that no longer exist on
/// disk filtered out.
pub fn list() -> Vec<PathBuf> {
    recent_path().map(|p| list_from(&p)).unwrap_or_default()
}

/// Drop `folder` from the list (it stays untouched on disk — this only edits
/// the recents entry). Silent on any I/O error, same as `record`.
pub fn remove(folder: &Path) {
    if let Some(path) = recent_path() {
        let canon = folder.canonicalize().unwrap_or_else(|_| folder.to_path_buf());
        remove_from(&path, &canon);
    }
}

// ---- testable cores (path-injected) -----------------------------------

fn list_from(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

fn record_to(path: &Path, folder: &Path) {
    let mut list = list_from(path);
    // Move-to-front: drop any existing copy, then prepend.
    list.retain(|p| p != folder);
    list.insert(0, folder.to_path_buf());
    list.truncate(MAX);
    write_list(path, &list);
}

fn remove_from(path: &Path, folder: &Path) {
    let mut list = list_from(path);
    list.retain(|p| p != folder);
    write_list(path, &list);
}

fn write_list(path: &Path, list: &[PathBuf]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = list
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_moves_to_front_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("recent");
        // Two real dirs to record.
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        record_to(&store, &a);
        record_to(&store, &b);
        record_to(&store, &a); // a again -> should jump to front, no dupe

        let got = list_from(&store);
        assert_eq!(got, vec![a.clone(), b.clone()]);
    }

    #[test]
    fn remove_drops_only_the_named_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("recent");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();

        record_to(&store, &a);
        record_to(&store, &b);
        remove_from(&store, &a);

        assert_eq!(list_from(&store), vec![b]);
    }

    #[test]
    fn stale_paths_are_filtered_out() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("recent");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(
            &store,
            format!("{}\n{}\n", real.display(), tmp.path().join("ghost").display()),
        )
        .unwrap();
        assert_eq!(list_from(&store), vec![real]);
    }
}
