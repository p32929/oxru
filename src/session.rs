//! Per-project editing session: which files were open (and which was active),
//! so reopening a folder restores your tabs. Stored under the config dir, keyed
//! by a hash of the project root, separate from the project itself.

use std::path::{Path, PathBuf};

/// `~/.config/oxru/sessions/<root-hash>` — where one project's open tabs live.
fn session_path(root: &Path) -> Option<PathBuf> {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    dirs::config_dir().map(|d| d.join(format!("oxru/sessions/{:016x}", hash(&canon))))
}

fn hash(p: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// Save the open `files` (and the `active` index) for `root`.
pub fn save(root: &Path, files: &[PathBuf], active: usize) {
    if let Some(path) = session_path(root) {
        save_to(&path, files, active);
    }
}

/// Load the previously-open files for `root` (skipping any that no longer exist)
/// and the active index.
pub fn load(root: &Path) -> (Vec<PathBuf>, usize) {
    session_path(root).map(|p| load_from(&p)).unwrap_or_default()
}

// ---- testable cores (path-injected) -----------------------------------

fn save_to(path: &Path, files: &[PathBuf], active: usize) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Line 0 is the active index; the rest are file paths, in tab order.
    let mut body = format!("{active}\n");
    for f in files {
        body.push_str(&f.to_string_lossy());
        body.push('\n');
    }
    let _ = std::fs::write(path, body);
}

fn load_from(path: &Path) -> (Vec<PathBuf>, usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let mut lines = text.lines();
    let active: usize = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0);
    let files: Vec<PathBuf> = lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect();
    let active = active.min(files.len().saturating_sub(1));
    (files, active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_load_roundtrips_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("sess");
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        save_to(&store, &[a.clone(), b.clone()], 1);
        let (files, active) = load_from(&store);
        assert_eq!(files, vec![a, b]);
        assert_eq!(active, 1);
    }

    #[test]
    fn missing_files_are_dropped_and_active_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("sess");
        let real = dir.path().join("real.rs");
        std::fs::write(&real, "").unwrap();
        let ghost = dir.path().join("ghost.rs");

        save_to(&store, &[ghost, real.clone()], 1);
        let (files, active) = load_from(&store);
        assert_eq!(files, vec![real]);
        assert_eq!(active, 0, "active index clamped to surviving files");
    }
}
