//! Debug logging to a file.
//!
//! Oxru repaints into a GPU window (or the alternate screen in TUI mode), so it
//! can't just print to stdout/stderr — that would corrupt the UI, and in GUI
//! mode nobody sees stderr anyway. Instead every run appends to a single log
//! file on disk that you can `tail -f` while reproducing a problem:
//!
//! ```text
//! ~/Library/Application Support/oxru/oxru.log   (macOS)
//! ~/.config/oxru/oxru.log                       (Linux)
//! ```
//!
//! It sits beside `config.toml` rather than in a separate data directory, so
//! everything Oxru writes is in one place — see [`crate::paths`], which is
//! where that location is decided.
//!
//! The level defaults to `info` (startup, terminal lifecycle, and every error
//! that used to be swallowed). Set `RUST_LOG` for more, e.g. `RUST_LOG=debug`
//! for event-loop / redraw detail or `RUST_LOG=oxru=trace` for per-frame traces.

use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// Roll the log over once it passes this size, so it can't grow without bound
/// across many sessions. We keep just the current file (truncate on rollover) —
/// the most recent run is what matters for debugging.
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Where the log file lives: `<data-local>/oxru/oxru.log`, falling back to the
/// system temp dir if no data directory can be determined.
pub fn log_path() -> PathBuf {
    crate::paths::log_file()
}

/// Initialise file logging. Safe to call once at startup; a second call (e.g.
/// from a test) is a no-op because the global subscriber is already set.
/// Returns the path being written to so the caller can mention it.
pub fn init() -> PathBuf {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Keep the file bounded: if the previous sessions left it large, start fresh.
    let truncate = std::fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false);
    let file = OpenOptions::new()
        .create(true)
        .append(!truncate)
        .truncate(truncate)
        .write(true)
        .open(&path);

    if let Ok(file) = file {
        install(file);
    }
    path
}

/// Wire `file` up as the global tracing subscriber. `try_init` (not `init`)
/// means a duplicate call — or a test that already installed a subscriber —
/// quietly does nothing instead of panicking.
fn install(file: File) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false) // it's a file, not a terminal
        .with_target(false) // module paths add noise; the message says enough
        .compact()
        .try_init();
}
