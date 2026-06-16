//! Tracks which folders are currently open across all running Oxru windows.
//!
//! Each window is its own process, so to know what's already open we keep a
//! directory of marker files — one per live process, named by PID, containing
//! that window's folder. Liveness is checked with `kill(pid, 0)`, so a window
//! that crashed without cleaning up is pruned automatically.

use std::path::{Path, PathBuf};

/// `~/.config/oxru/running/` (platform config dir).
fn running_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("oxru/running"))
}

/// Whether process `pid` is still alive (so its marker isn't stale).
fn pid_alive(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // signal 0 performs error checking without sending a signal: 0 => alive.
    pid > 0 && unsafe { kill(pid, 0) } == 0
}

/// Mark this process as having `folder` open.
pub fn register(folder: &Path) {
    if let Some(dir) = running_dir() {
        let canon = folder.canonicalize().unwrap_or_else(|_| folder.to_path_buf());
        register_in(&dir, std::process::id(), &canon);
    }
}

/// Remove this process's marker (called on a clean exit).
pub fn unregister() {
    if let Some(dir) = running_dir() {
        let _ = std::fs::remove_file(dir.join(std::process::id().to_string()));
    }
}

/// Folders open in any live Oxru window (stale markers pruned in passing).
pub fn open_folders() -> Vec<PathBuf> {
    running_dir().map(|d| open_folders_in(&d)).unwrap_or_default()
}

// ---- testable cores (dir-injected) ------------------------------------

fn register_in(dir: &Path, pid: u32, folder: &Path) {
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(pid.to_string()), folder.to_string_lossy().as_bytes());
}

fn open_folders_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(e.path()); // prune dead window
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(e.path()) {
            let p = PathBuf::from(content.trim());
            if !p.as_os_str().is_empty() {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_process_is_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("proj");
        std::fs::create_dir(&folder).unwrap();
        register_in(tmp.path(), std::process::id(), &folder);
        assert_eq!(open_folders_in(tmp.path()), vec![folder]);
    }

    #[test]
    fn dead_process_marker_is_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        // A PID that's essentially never alive.
        register_in(tmp.path(), 0x7fff_fffe, Path::new("/somewhere"));
        assert!(open_folders_in(tmp.path()).is_empty());
        // The stale marker file was removed.
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }
}
