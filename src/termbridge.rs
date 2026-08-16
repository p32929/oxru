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

/// The fake `osascript`. Routes terminal-opening AppleScript to Oxru, runs the
/// real `osascript` for anything else.
///
/// Recognises the three shapes a script can arrive in — `-e` arguments, a
/// script on stdin, and a `.scpt`/`.applescript` file argument — and both the
/// AppleScript (`do script "…"`, iTerm's `write text "…"`) and JavaScript
/// (`doScript("…")`) spellings. Anything it declines is logged to
/// `$OXRU_REQUEST_FILE.passthrough` with its arguments, so a call that still
/// escapes and opens a window can be identified instead of guessed at.
const OSASCRIPT_SHIM: &str = r##"#!/bin/bash
# Oxru osascript shim — see src/termbridge.rs.
REAL="/usr/bin/osascript"
[ -n "$OXRU_REQUEST_FILE" ] || exec "$REAL" "$@"

# Reconstruct the source: concatenated -e args, a script file argument, or stdin.
src=""
have_e=0
file=""
argv=("$@")
n=${#argv[@]}
i=0
while [ $i -lt $n ]; do
  a="${argv[$i]}"
  case "$a" in
    -e) have_e=1; i=$((i+1)); src="$src${argv[$i]}
" ;;
    -*) : ;;                       # -l, -s, … — flags we don't need to read
    *) [ -z "$file" ] && [ -f "$a" ] && file="$a" ;;
  esac
  i=$((i+1))
done
if [ $have_e -eq 0 ]; then
  if [ -n "$file" ]; then
    src="$(cat -- "$file" 2>/dev/null)"
  else
    src="$(cat)"
  fi
fi

# Pull out every command that would open a terminal: AppleScript `do script`,
# iTerm's `write text`, and the JavaScript-for-Automation `doScript(...)`.
cmds="$(printf '%s' "$src" | perl -0777 -ne '
  while (/(?:do\s+script|write\s+text)\s+"((?:[^"\\]|\\.)*)"/gis) { my $c=$1; $c=~s/\\"/"/g; $c=~s/\s*\n\s*/ /g; print "$c\n"; }
  while (/doScript\s*\(\s*"((?:[^"\\]|\\.)*)"/gs)   { my $c=$1; $c=~s/\\"/"/g; $c=~s/\s*\n\s*/ /g; print "$c\n"; }
  while (/doScript\s*\(\s*'"'"'((?:[^'"'"'\\]|\\.)*)'"'"'/gs) { my $c=$1; $c=~s/\s*\n\s*/ /g; print "$c\n"; }
')"

if [ -n "$cmds" ]; then
  printf '%s\n' "$cmds" >> "$OXRU_REQUEST_FILE"
  exit 0
fi

# Not a terminal-opening script: record it, then run the real osascript.
printf 'osascript %s\n' "$*" >> "$OXRU_REQUEST_FILE.passthrough" 2>/dev/null
if [ $have_e -eq 1 ] || [ -n "$file" ]; then
  exec "$REAL" "$@"
else
  printf '%s' "$src" | "$REAL"
  exit $?
fi
"##;

/// The fake `open`. `open -a Terminal`, `open -b com.apple.Terminal` and
/// `open foo.command` are the other everyday ways a script starts a terminal,
/// and none of them go anywhere near `osascript` — so the `osascript` shim
/// alone left them opening real windows.
const OPEN_SHIM: &str = r##"#!/bin/bash
# Oxru open shim — see src/termbridge.rs.
REAL="/usr/bin/open"
[ -n "$OXRU_REQUEST_FILE" ] || exec "$REAL" "$@"

app=""
bundle=""
files=()
argv=("$@")
n=${#argv[@]}
i=0
while [ $i -lt $n ]; do
  a="${argv[$i]}"
  case "$a" in
    -a) i=$((i+1)); app="${argv[$i]}" ;;
    -b) i=$((i+1)); bundle="${argv[$i]}" ;;
    --args) break ;;               # everything after --args belongs to the app
    -*) : ;;
    *) files+=("$a") ;;
  esac
  i=$((i+1))
done

is_terminal_app() {
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    *terminal*|*iterm*|*alacritty*|*kitty*|*wezterm*|*ghostty*|*hyper*|*warp*) return 0 ;;
  esac
  return 1
}

# `open -a Terminal <dir>` opens a shell there; with no file it's just a shell.
if { [ -n "$app" ] && is_terminal_app "$app"; } || { [ -n "$bundle" ] && is_terminal_app "$bundle"; }; then
  target="${files[0]}"
  if [ -n "$target" ] && [ -d "$target" ]; then
    printf 'cd %q\n' "$target" >> "$OXRU_REQUEST_FILE"
  elif [ -n "$target" ] && [ -f "$target" ]; then
    printf '%q\n' "$target" >> "$OXRU_REQUEST_FILE"
  else
    printf ':oxru-shell:\n' >> "$OXRU_REQUEST_FILE"
  fi
  exit 0
fi

# A double-clickable shell script is a terminal window by another name.
for f in "${files[@]}"; do
  case "$f" in
    *.command|*.terminal|*.tool)
      printf '%q\n' "$f" >> "$OXRU_REQUEST_FILE"
      exit 0
      ;;
  esac
done

printf 'open %s\n' "$*" >> "$OXRU_REQUEST_FILE.passthrough" 2>/dev/null
exec "$REAL" "$@"
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

    let open_shim = dir.join("open");
    std::fs::write(&open_shim, OPEN_SHIM).ok()?;
    make_executable(&open_shim).ok()?;

    let zdotdir = write_zdotdir(&dir);

    Some(Bridge {
        dir,
        request_file,
        zdotdir,
    })
}

/// Marker dropped into every wrapper we write, so a nested Oxru can recognise
/// another instance's `ZDOTDIR` and refuse to chain to it.
const WRAPPER_MARKER: &str = ".oxru-zdotdir";

/// Resolve the value to export as `OXRU_REAL_ZDOTDIR` — the user's *own* zsh
/// config directory, never another Oxru's wrapper.
///
/// The bug this exists for: launching Oxru from inside an Oxru terminal used to
/// set `OXRU_REAL_ZDOTDIR` to the outer instance's wrapper, whose own startup
/// files then read `${OXRU_REAL_ZDOTDIR:-$HOME}` — still pointing at
/// themselves — and sourced themselves until zsh gave up with "recursion limit
/// exceeded", leaving the shell with none of the user's config loaded.
///
/// Returns an empty string when there's nothing real to point at; the wrapper's
/// `${OXRU_REAL_ZDOTDIR:-$HOME}` treats empty as unset and falls back to `$HOME`.
/// It's always exported, even empty, so a value inherited from an outer Oxru
/// can't leak into the child.
fn resolve_real_zdotdir(
    oxru_real: Option<String>,
    zdotdir: Option<String>,
    is_ours: impl Fn(&Path) -> bool,
) -> String {
    // An outer Oxru already did this resolution; pass its answer through
    // unchanged rather than resolving again on top of it.
    if let Some(v) = oxru_real.filter(|v| !v.is_empty()) {
        if !is_ours(Path::new(&v)) {
            return v;
        }
        // An outer instance that pointed at a wrapper (older build, or a
        // deeper nesting) — drop it rather than propagate the loop.
        return String::new();
    }
    match zdotdir.filter(|v| !v.is_empty()) {
        // Running inside another Oxru terminal: its ZDOTDIR is a wrapper of
        // ours, and chaining to it is exactly the recursion above.
        Some(z) if is_ours(Path::new(&z)) => String::new(),
        Some(z) => z,
        None => String::new(),
    }
}

/// Whether `dir` is a wrapper written by some Oxru instance.
///
/// The marker file alone isn't enough: a wrapper left by an *older* build
/// doesn't have one, and that's precisely the case that matters — you upgrade,
/// launch the new Oxru from a terminal of the still-running old one, and the
/// new instance has to recognise the old wrapper to avoid chaining to it. So
/// fall back to the content signature every version of the wrapper has: its
/// `.zshenv` references `OXRU_REAL_ZDOTDIR`, which nobody else's would.
fn is_oxru_zdotdir(dir: &Path) -> bool {
    if dir.join(WRAPPER_MARKER).exists() {
        return true;
    }
    std::fs::read_to_string(dir.join(".zshenv"))
        .is_ok_and(|t| t.contains("OXRU_REAL_ZDOTDIR"))
}

/// Write a `ZDOTDIR` whose startup files source the user's real zsh config and
/// then put `bridge_dir` first on `PATH`. Returns the dir, or `None` on failure.
fn write_zdotdir(bridge_dir: &Path) -> Option<PathBuf> {
    let zdir = bridge_dir.join("zdotdir");
    std::fs::create_dir_all(&zdir).ok()?;
    // Lets a nested Oxru tell this directory apart from the user's own.
    std::fs::write(zdir.join(WRAPPER_MARKER), b"").ok()?;
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
            let real = resolve_real_zdotdir(
                std::env::var("OXRU_REAL_ZDOTDIR").ok(),
                std::env::var("ZDOTDIR").ok(),
                is_oxru_zdotdir,
            );
            // Always exported, even empty: without it a value inherited from an
            // outer Oxru survives into the child and re-creates the loop.
            env.push(("OXRU_REAL_ZDOTDIR".to_string(), real));
            env.push(("ZDOTDIR".to_string(), zdir.display().to_string()));
        }
    }
    env
}

/// Request-file line meaning "open a terminal with no command" — what a bare
/// `open -a Terminal` asks for. A blank line can't carry that: the drain
/// ignores blank lines, and rightly so, or a stray newline would spawn a
/// terminal.
pub const SHELL_REQUEST: &str = ":oxru-shell:";

/// Where the shims record calls they decided *not* to intercept. Read into
/// Oxru's log so a script that still manages to open a real window can be
/// identified from the log instead of guessed at.
pub fn passthrough_file() -> Option<PathBuf> {
    bridge().map(|b| {
        let mut p = b.request_file.clone().into_os_string();
        p.push(".passthrough");
        PathBuf::from(p)
    })
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

    /// The reported bug: `./run-local.sh --gui .` from inside an Oxru terminal
    /// produced, in every terminal of the new instance:
    ///
    /// ```text
    /// /T/oxru-26888/zdotdir/.zshenv:1: job table full or recursion limit exceeded
    /// ```
    ///
    /// because the new instance pointed `OXRU_REAL_ZDOTDIR` at the *outer*
    /// instance's wrapper, whose own `${OXRU_REAL_ZDOTDIR:-$HOME}` then
    /// resolved back to itself.
    #[test]
    fn a_nested_oxru_never_chains_to_another_instances_wrapper() {
        let outer = PathBuf::from("/tmp/oxru-26888/zdotdir");
        // `Path::starts_with` matches whole components, so compare as text.
        let ours = |p: &Path| p.to_string_lossy().contains("/oxru-");

        // Inside an Oxru terminal, with no OXRU_REAL_ZDOTDIR yet.
        assert_eq!(
            resolve_real_zdotdir(None, Some(outer.display().to_string()), ours),
            "",
            "must fall back to $HOME, not point at the outer wrapper"
        );

        // Deeper nesting: an outer instance already set it to a wrapper.
        assert_eq!(
            resolve_real_zdotdir(Some(outer.display().to_string()), None, ours),
            "",
            "a wrapper handed down from an outer instance is dropped, not propagated"
        );
    }

    #[test]
    fn a_real_zdotdir_is_still_honoured_and_passed_down() {
        // `Path::starts_with` matches whole components, so compare as text.
        let ours = |p: &Path| p.to_string_lossy().contains("/oxru-");

        // The ordinary case: the user really does set ZDOTDIR.
        assert_eq!(
            resolve_real_zdotdir(None, Some("/Users/me/.config/zsh".into()), ours),
            "/Users/me/.config/zsh"
        );
        // An outer Oxru already resolved it — pass that through untouched, so
        // the user's config still loads at any nesting depth.
        assert_eq!(
            resolve_real_zdotdir(
                Some("/Users/me/.config/zsh".into()),
                Some("/tmp/oxru-1/zdotdir".into()),
                ours
            ),
            "/Users/me/.config/zsh"
        );
        // Nothing set anywhere: empty, which the wrapper reads as $HOME.
        assert_eq!(resolve_real_zdotdir(None, None, ours), "");
        assert_eq!(resolve_real_zdotdir(Some(String::new()), Some(String::new()), ours), "");
    }

    /// A wrapper written by an older build has no marker file, and that's the
    /// case that actually bites: upgrade, then launch the new Oxru from a
    /// terminal of the still-running old one.
    #[test]
    fn a_wrapper_from_an_older_build_is_still_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let zdir = dir.path().join("zdotdir");
        std::fs::create_dir_all(&zdir).unwrap();
        // Exactly what the previous version wrote — no marker file.
        std::fs::write(
            zdir.join(".zshenv"),
            "[ -f \"${OXRU_REAL_ZDOTDIR:-$HOME}/.zshenv\" ] && source \"${OXRU_REAL_ZDOTDIR:-$HOME}/.zshenv\"\n",
        )
        .unwrap();
        assert!(!zdir.join(WRAPPER_MARKER).exists(), "no marker, as an old build left it");
        assert!(is_oxru_zdotdir(&zdir), "recognised by its content signature");

        // Someone's genuine ZDOTDIR must not be mistaken for ours.
        let real = dir.path().join("myzsh");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join(".zshenv"), "export EDITOR=vim\n").unwrap();
        assert!(!is_oxru_zdotdir(&real));
    }

    /// The marker is what makes the detection above work on a real directory.
    #[test]
    fn every_wrapper_we_write_is_recognisable_as_ours() {
        let dir = tempfile::tempdir().unwrap();
        let zdir = write_zdotdir(dir.path()).expect("wrapper written");
        assert!(is_oxru_zdotdir(&zdir), "our own wrapper must be detectable");
        assert!(!is_oxru_zdotdir(dir.path()), "an unrelated directory must not be");
    }

    /// Drive a shim with `args` in an isolated dir, with the "real" binary
    /// replaced by a stub, and return `(captured requests, escaped calls)`.
    fn run_shim(which: &str, body: &str, args: &[&str], stdin: Option<&str>) -> (String, String) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("fake-real");
        std::fs::write(
            &real,
            format!("#!/bin/bash\necho \"$*\" >> {}\n", dir.path().join("escaped").display()),
        )
        .unwrap();
        let shim = dir.path().join(which);
        // Point the shim's REAL at the stub so a test can never open a window.
        let patched = body.replace(
            &format!("REAL=\"/usr/bin/{which}\""),
            &format!("REAL=\"{}\"", real.display()),
        );
        std::fs::write(&shim, patched).unwrap();
        make_executable(&shim).unwrap();
        make_executable(&real).unwrap();

        let requests = dir.path().join("requests");
        std::fs::write(&requests, b"").unwrap();
        let mut child = Command::new(&shim)
            .args(args)
            .env("OXRU_REQUEST_FILE", &requests)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        if let Some(text) = stdin {
            child.stdin.as_mut().unwrap().write_all(text.as_bytes()).unwrap();
        }
        drop(child.stdin.take());
        child.wait().unwrap();
        (
            std::fs::read_to_string(&requests).unwrap_or_default(),
            std::fs::read_to_string(dir.path().join("escaped")).unwrap_or_default(),
        )
    }

    fn osa(args: &[&str], stdin: Option<&str>) -> (String, String) {
        run_shim("osascript", OSASCRIPT_SHIM, args, stdin)
    }
    fn open_(args: &[&str]) -> (String, String) {
        run_shim("open", OPEN_SHIM, args, None)
    }

    /// Every way a script opens a terminal has to be caught, or it opens a real
    /// window — which is the whole thing the bridge exists to prevent.
    #[test]
    fn every_terminal_opening_form_is_captured() {
        // AppleScript via -e, the common case.
        let (req, esc) = osa(&["-e", r#"tell application "Terminal" to do script "echo A""#], None);
        assert!(req.contains("echo A"), "-e form");
        assert!(esc.is_empty(), "must not reach the real binary");

        // Multi-line AppleScript on stdin (the heredoc form).
        let (req, _) = osa(
            &[],
            Some("tell application \"Terminal\"\n activate\n do script \"echo B\"\nend tell\n"),
        );
        assert!(req.contains("echo B"), "stdin heredoc form");

        // JavaScript for Automation — a different verb entirely.
        let (req, _) = osa(
            &["-l", "JavaScript", "-e", r#"Application("Terminal").doScript("echo C")"#],
            None,
        );
        assert!(req.contains("echo C"), "JXA doScript form");

        // iTerm2 writes into a session rather than "do script".
        let (req, _) = osa(
            &["-e", r#"tell application "iTerm" to tell current session to write text "echo D""#],
            None,
        );
        assert!(req.contains("echo D"), "iTerm write text form");
    }

    #[test]
    fn a_script_file_argument_is_read_rather_than_run() {
        let dir = tempfile::tempdir().unwrap();
        let scpt = dir.path().join("s.scpt");
        std::fs::write(&scpt, "tell application \"Terminal\" to do script \"echo E\"\n").unwrap();
        let (req, esc) = osa(&[scpt.to_str().unwrap()], None);
        assert!(req.contains("echo E"), "a .scpt file must be inspected, not executed");
        assert!(esc.is_empty());
    }

    /// `open -a Terminal` never goes near osascript, so the osascript shim
    /// alone left this opening real windows.
    #[test]
    fn the_open_command_cannot_start_a_terminal_either() {
        let (req, esc) = open_(&["-a", "Terminal"]);
        assert!(req.contains(SHELL_REQUEST), "a bare terminal launch asks for a plain shell");
        assert!(esc.is_empty());

        let (req, _) = open_(&["-a", "Terminal", "/tmp"]);
        assert!(req.contains("cd /tmp"), "a directory argument becomes a cd");

        let (req, _) = open_(&["-b", "com.apple.Terminal"]);
        assert!(req.contains(SHELL_REQUEST), "bundle id form");

        let (req, _) = open_(&["-a", "iTerm"]);
        assert!(req.contains(SHELL_REQUEST), "other terminal emulators too");
    }

    #[test]
    fn a_double_clickable_script_counts_as_a_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = dir.path().join("go.command");
        std::fs::write(&cmd, "#!/bin/sh\necho hi\n").unwrap();
        let (req, _) = open_(&[cmd.to_str().unwrap()]);
        assert!(req.contains("go.command"), ".command files open a terminal window");
    }

    /// Just as important: everything else must behave exactly as before, or the
    /// shims would break unrelated automation.
    #[test]
    fn non_terminal_calls_pass_straight_through() {
        let (req, esc) = osa(&["-e", r#"display dialog "hi""#], None);
        assert!(req.is_empty(), "a dialog is not a terminal");
        assert!(esc.contains("display dialog"), "and must still run for real");

        let (req, esc) = open_(&["-a", "Simulator"]);
        assert!(req.is_empty(), "the iOS Simulator is not a terminal");
        assert!(esc.contains("Simulator"));

        let (req, esc) = open_(&["https://example.com"]);
        assert!(req.is_empty(), "a URL is not a terminal");
        assert!(esc.contains("example.com"));
    }

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
