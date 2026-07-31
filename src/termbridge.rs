//! Terminal bridge: makes scripts that open new OS Terminal windows open
//! *inside* Oxru instead.
//!
//! Many macOS scripts spawn extra windows with
//! `osascript … tell application "Terminal" … do script "CMD"`. When Oxru spawns
//! an embedded terminal it prepends a shim directory to the child's `PATH`. The
//! shim's fake `osascript` detects those "do script" calls, appends the command
//! to a request file, and exits without opening a window. Oxru polls that file
//! and opens a new embedded terminal per request. Every other `osascript` use
//! falls through to the real binary, so nothing else changes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

struct Bridge {
    dir: PathBuf,
    request_file: PathBuf,
    /// A `ZDOTDIR` of zsh startup files that source the user's real config and
    /// then re-prepend `dir` to `PATH` (so the shim wins even after a login
    /// shell's `path_helper` reorders things). `None` if it couldn't be written.
    zdotdir: Option<PathBuf>,
}

static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();

/// The fake `osascript`. Routes Terminal "do script" calls to Oxru, runs the
/// real `osascript` for anything else.
const OSASCRIPT_SHIM: &str = r##"#!/bin/bash
# Oxru osascript shim — see src/termbridge.rs.
REAL="/usr/bin/osascript"
[ -n "$OXRU_REQUEST_FILE" ] || exec "$REAL" "$@"

# Reconstruct the AppleScript source: concatenated -e args, or stdin.
src=""
have_e=0
argv=("$@")
n=${#argv[@]}
i=0
while [ $i -lt $n ]; do
  if [ "${argv[$i]}" = "-e" ]; then
    have_e=1
    i=$((i+1))
    src="$src${argv[$i]}
"
  fi
  i=$((i+1))
done
if [ $have_e -eq 0 ]; then
  src="$(cat)"
fi

# A Terminal "do script" call — queue each command for Oxru and don't open a window.
case "$src" in
  *"do script"*)
    printf '%s' "$src" | perl -0777 -ne 'while (/do script\s+"((?:[^"\\]|\\.)*)"/gis) { my $c=$1; $c=~s/\\"/"/g; $c=~s/\s*\n\s*/ /g; print "$c\n"; }' >> "$OXRU_REQUEST_FILE"
    exit 0
    ;;
esac

# Anything else: run the real osascript unchanged.
if [ $have_e -eq 1 ]; then
  exec "$REAL" "$@"
else
  printf '%s' "$src" | "$REAL"
  exit $?
fi
"##;

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn init() -> Option<Bridge> {
    // The shim is a shell script; only meaningful on unix-likes.
    if !cfg!(unix) {
        return None;
    }
    let dir = std::env::temp_dir().join(format!("oxru-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;

    let request_file = dir.join("requests");
    if !request_file.exists() {
        std::fs::write(&request_file, b"").ok()?;
    }

    let shim = dir.join("osascript");
    std::fs::write(&shim, OSASCRIPT_SHIM).ok()?;
    make_executable(&shim).ok()?;

    let zdotdir = write_zdotdir(&dir);

    Some(Bridge {
        dir,
        request_file,
        zdotdir,
    })
}

/// Write a `ZDOTDIR` whose startup files source the user's real zsh config and
/// then put `bridge_dir` first on `PATH`. Returns the dir, or `None` on failure.
fn write_zdotdir(bridge_dir: &Path) -> Option<PathBuf> {
    let zdir = bridge_dir.join("zdotdir");
    std::fs::create_dir_all(&zdir).ok()?;
    let real = "${OXRU_REAL_ZDOTDIR:-$HOME}";
    let me = zdir.display();

    // .zshenv runs first (always). Re-pin ZDOTDIR to ours in case the user's
    // .zshenv changed it, so our .zshrc below is guaranteed to run.
    std::fs::write(
        zdir.join(".zshenv"),
        format!("[ -f \"{real}/.zshenv\" ] && source \"{real}/.zshenv\"\nexport ZDOTDIR=\"{me}\"\n"),
    )
    .ok()?;
    std::fs::write(
        zdir.join(".zprofile"),
        format!("[ -f \"{real}/.zprofile\" ] && source \"{real}/.zprofile\"\n"),
    )
    .ok()?;
    std::fs::write(
        zdir.join(".zlogin"),
        format!("[ -f \"{real}/.zlogin\" ] && source \"{real}/.zlogin\"\n"),
    )
    .ok()?;
    // .zshrc runs last for interactive shells: load the user's, then make our
    // shim dir win.
    std::fs::write(
        zdir.join(".zshrc"),
        format!(
            "[ -f \"{real}/.zshrc\" ] && source \"{real}/.zshrc\"\nexport PATH=\"{}:$PATH\"\n",
            bridge_dir.display()
        ),
    )
    .ok()?;
    Some(zdir)
}

fn bridge() -> Option<&'static Bridge> {
    BRIDGE.get_or_init(init).as_ref()
}

/// Environment to inject into a spawned terminal so child processes route
/// Terminal-opening calls back to Oxru. Empty if the bridge can't be set up.
pub fn child_env() -> Vec<(String, String)> {
    let Some(b) = bridge() else {
        return Vec::new();
    };
    let path = match std::env::var("PATH") {
        Ok(p) => format!("{}:{}", b.dir.display(), p),
        Err(_) => b.dir.display().to_string(),
    };
    let mut env = vec![
        ("PATH".to_string(), path),
        (
            "OXRU_REQUEST_FILE".to_string(),
            b.request_file.display().to_string(),
        ),
        ("OXRU_TERMINAL".to_string(), "1".to_string()),
    ];

    // For zsh, point it at our ZDOTDIR so the shim stays first on PATH even
    // after the login shell's path_helper runs (see write_zdotdir). Preserve a
    // pre-existing ZDOTDIR so our files can still source the user's real config.
    let is_zsh = std::env::var("SHELL")
        .map(|s| s.rsplit('/').next().unwrap_or("").starts_with("zsh"))
        .unwrap_or(false);
    if is_zsh {
        if let Some(zdir) = &b.zdotdir {
            if let Ok(existing) = std::env::var("ZDOTDIR") {
                if !existing.is_empty() {
                    env.push(("OXRU_REAL_ZDOTDIR".to_string(), existing));
                }
            }
            env.push(("ZDOTDIR".to_string(), zdir.display().to_string()));
        }
    }
    env
}

/// The request file Oxru polls for queued terminal-open commands.
pub fn request_file() -> Option<PathBuf> {
    bridge().map(|b| b.request_file.clone())
}

/// Remove the shim directory (best-effort, on exit).
pub fn cleanup() {
    if let Some(b) = bridge() {
        let _ = std::fs::remove_dir_all(&b.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn zdotdir_keeps_shim_ahead_of_usr_bin() {
        // zsh-only mechanism.
        if !std::env::var("SHELL").unwrap_or_default().contains("zsh") {
            return;
        }
        let Some(req) = request_file() else {
            return;
        };
        let bridge_dir = req.parent().unwrap().to_path_buf();
        let zdotdir = bridge_dir.join("zdotdir");
        if !zdotdir.exists() || !Path::new("/bin/zsh").exists() {
            return;
        }

        // A controlled HOME whose .zshrc prepends dirs (like a real user setup),
        // to prove our shim still wins after macOS's path_helper + the user rc.
        let fake = tempfile::tempdir().unwrap();
        std::fs::write(
            fake.path().join(".zshrc"),
            "export PATH=\"/tmp/foo:/tmp/bar:$PATH\"\n",
        )
        .unwrap();

        let out = Command::new("/bin/zsh")
            .args(["-l", "-i", "-c", "command -v osascript"])
            .env_clear()
            .env("HOME", fake.path())
            .env("TERM", "xterm-256color")
            .env("ZDOTDIR", &zdotdir)
            .env("OXRU_REQUEST_FILE", &req)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output()
            .expect("run login zsh");
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            resolved,
            bridge_dir.join("osascript").to_string_lossy(),
            "osascript must resolve to the bridge shim, not /usr/bin"
        );
    }

    #[test]
    fn shim_routes_terminal_do_script_to_request_file() {
        let Some(req) = request_file() else {
            return; // non-unix: no bridge
        };
        let shim = req.parent().unwrap().join("osascript");
        assert!(shim.exists(), "shim osascript should exist");

        let out = Command::new(&shim)
            .arg("-e")
            .arg(r#"tell application "Terminal" to do script "echo HELLO_OXRU_TEST""#)
            .env("OXRU_REQUEST_FILE", &req)
            .output()
            .expect("run shim");
        assert!(out.status.success(), "shim should exit 0");

        let contents = std::fs::read_to_string(&req).unwrap_or_default();
        assert!(
            contents.contains("echo HELLO_OXRU_TEST"),
            "shim should queue the command, got: {contents:?}"
        );
    }
}
