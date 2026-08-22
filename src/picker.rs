//! Native "Open Folder" picker — the only way to open a project that isn't
//! already in the recents list.
//!
//! On macOS this is the system `NSOpenPanel` (via `rfd`). Beyond the obvious UX
//! win, a folder the user selects through the panel is granted by *user intent*
//! (the "powerbox"), so reading it does **not** raise the Documents/Desktop TCC
//! permission alert for that folder — unlike opening the same path by hand.
//!
//! Must be called on the main thread (the GUI and TUI event loops both invoke it
//! from there). Returns `None` if the user cancels.

use std::path::PathBuf;

#[cfg(target_os = "macos")]
pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().set_title("Open Folder").pick_folder()
}

#[cfg(not(target_os = "macos"))]
pub fn pick_folder() -> Option<PathBuf> {
    // No native picker wired up off macOS yet — callers fall back to the recents
    // dialog / CLI argument.
    None
}
