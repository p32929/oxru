//! **The** map of where Oxru keeps its files. Every path the app reads or
//! writes outside a project is built here, so the app's directory name and the
//! choice of platform base directory are each stated once.
//!
//! Six modules used to build these themselves — `config.rs`, `recent.rs`,
//! `instances.rs`, `todo.rs`, `session.rs` and `logging.rs` — each repeating
//! `dirs::config_dir().map(|d| d.join("oxru/…"))`. They agreed by coincidence
//! rather than by construction: `logging.rs` picked a *different* base
//! directory (`data_local_dir`), which happens to resolve to the same
//! `~/Library/Application Support` on macOS and to a different place on Linux.
//! That's the sort of thing nobody notices until a user's log isn't where the
//! docs say it is.
//!
//! Everything lives under one directory now, named once in [`APP_DIR`].

use std::path::{Path, PathBuf};

/// The directory Oxru owns inside the platform's config location. One string,
/// so renaming the app is one edit rather than eight.
pub const APP_DIR: &str = "oxru";

/// `~/.config/oxru` (or the platform equivalent). `None` when the platform
/// can't tell us where config lives, in which case every caller degrades to
/// "this feature is unavailable" rather than guessing a location.
pub fn app_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join(APP_DIR))
}

/// The global `config.toml`.
pub fn config_file() -> Option<PathBuf> {
    app_dir().map(|d| d.join("config.toml"))
}

/// A project's own config, `<project>/.oxru/config.toml`.
pub fn project_config_file(root: &Path) -> PathBuf {
    root.join(".oxru").join("config.toml")
}

/// The recently-opened-folders list.
pub fn recent_file() -> Option<PathBuf> {
    app_dir().map(|d| d.join("recent"))
}

/// The global to-do list, kept as plain markdown so it's editable anywhere.
pub fn todos_file() -> Option<PathBuf> {
    app_dir().map(|d| d.join("todos.md"))
}

/// Marker files for the Oxru processes currently running.
pub fn running_dir() -> Option<PathBuf> {
    app_dir().map(|d| d.join("running"))
}

/// Saved open-tab sessions, one file per project root.
pub fn sessions_dir() -> Option<PathBuf> {
    app_dir().map(|d| d.join("sessions"))
}

/// The log file.
///
/// Unlike the rest, this has a fallback: logging has to work even on a machine
/// where no config directory can be determined, because the log is where we'd
/// report that fact. It follows the same base directory as everything else so
/// the log sits beside the config that describes it — it used to use
/// `data_local_dir`, which is the same place only on macOS.
pub fn log_file() -> PathBuf {
    app_dir()
        .unwrap_or_else(|| std::env::temp_dir().join(APP_DIR))
        .join("oxru.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything Oxru writes belongs under the one directory — a stray path
    /// somewhere else is a file the user can't find and we don't clean up.
    #[test]
    fn every_path_sits_under_the_app_directory() {
        let Some(base) = app_dir() else {
            return; // no config dir on this platform; nothing to check
        };
        let paths = [
            config_file(),
            recent_file(),
            todos_file(),
            running_dir(),
            sessions_dir(),
            Some(log_file()),
        ];
        for path in paths.into_iter().flatten() {
            assert!(
                path.starts_with(&base),
                "{} escapes the app directory {}",
                path.display(),
                base.display()
            );
        }
    }

    /// The names are distinct, so two features can't fight over one file.
    #[test]
    fn no_two_features_share_a_path() {
        let mut seen = std::collections::HashSet::new();
        let paths = [
            config_file(),
            recent_file(),
            todos_file(),
            running_dir(),
            sessions_dir(),
            Some(log_file()),
        ];
        for path in paths.into_iter().flatten() {
            assert!(seen.insert(path.clone()), "{} is claimed twice", path.display());
        }
    }

    /// A project's config is inside the project, not the shared directory —
    /// that's what makes it per-project.
    #[test]
    fn a_projects_config_lives_in_the_project() {
        let p = project_config_file(Path::new("/tmp/demo"));
        assert!(p.starts_with("/tmp/demo"), "{} left the project", p.display());
        assert!(p.ends_with("config.toml"));
    }
}
