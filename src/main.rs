//! Oxru — a small terminal/GUI code editor.
//!
//! The UI is intentionally minimal: a blank screen and a single file dialog
//! (open with `Option+F`) for searching, opening, and managing files; `Ctrl+F`
//! finds within the open file. It opens in its own window by default; `--term`
//! runs it inside the host terminal instead.

mod app;
mod buffer;
mod config;
mod editline;
mod filedialog;
mod fonts;
mod fstree;
#[cfg(feature = "gui")]
mod gui;
mod icons;
mod input;
mod instances;
mod logging;
mod picker;
mod prompt;
mod session;
mod recent;
mod search;
mod syntax;
mod termbridge;
mod terminalpane;
mod theme;
mod todo;
mod ui;
mod wrap;

#[cfg(all(unix, feature = "gui"))]
use std::io::IsTerminal;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;

use app::App;

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return Ok(());
    }
    let want_gui = wants_gui(&args);

    // `oxru --gui .` launched from an interactive shell would otherwise sit in
    // the foreground for as long as the window stays open, blocking that shell
    // — and closing the terminal would SIGHUP this process (and the window
    // with it). Relaunch as a session-detached child, like `code .` does, and
    // hand the shell back immediately; the child picks the same branch below
    // but skips this check (via OXRU_GUI_DETACHED) and runs the window itself.
    #[cfg(all(unix, feature = "gui"))]
    if want_gui && io::stdout().is_terminal() && std::env::var_os("OXRU_GUI_DETACHED").is_none() {
        return relaunch_gui_detached(&args);
    }

    // Start file logging before anything else so even early failures are caught,
    // and route panics there too (both GUI and TUI modes).
    let log_path = logging::init();
    install_panic_hook();
    // First non-flag argument is the project path. With no argument the editor
    // opens with **no folder** (a welcome screen) — open one via the recents
    // picker (Option+O). Use `oxru .` to open the current directory.
    let root = args.iter().find(|a| !a.starts_with('-')).map(|p| {
        let r = PathBuf::from(p);
        r.canonicalize().unwrap_or(r)
    });

    // Remember the folder so it shows up in the "Recent folders" dialog later,
    // and mark it as currently open so other windows won't reopen it.
    if let Some(r) = &root {
        recent::record(r);
        instances::register(r);
    }

    let mut app = App::new(root)?;
    app.gui = want_gui;
    // Only the windowed build insets its content: in a host terminal the
    // emulator already supplies the margin against the window frame.
    if want_gui {
        app.gui_padding = app.config_gui_padding;
    }
    // Reopen the tabs that were open last time this folder was used.
    app.restore_session();

    tracing::info!(
        mode = if want_gui { "gui" } else { "tui" },
        root = ?app.root,
        log = %log_path.display(),
        "oxru starting"
    );

    if want_gui {
        #[cfg(feature = "gui")]
        {
            let result = gui::run(app);
            instances::unregister();
            termbridge::cleanup();
            return result;
        }
        #[cfg(not(feature = "gui"))]
        {
            eprintln!(
                "oxru: this build has no GUI support. Rebuild with `--features gui` \
                 (it is enabled by default)."
            );
            std::process::exit(2);
        }
    }

    // The host terminal's font isn't guaranteed to carry Nerd glyphs (the GUI
    // ships its own font; a terminal can't). Install the bundled symbols font so
    // the terminal's glyph fallback can find it, then pick the icon set that will
    // actually render: keep Nerd only when that font is already available.
    let font = fonts::install_symbol_font();
    app.ensure_terminal_icons(font);
    let result = run_tui(app);
    instances::unregister();
    termbridge::cleanup();
    result
}

/// Whether to open a window. **Windowed is the default**; `--term` opts into
/// running inside the host terminal, and `--gui` stays accepted so existing
/// commands and scripts keep working.
///
/// An explicit flag always wins — if both appear, the last one on the command
/// line does, so `oxru --term --gui` reads the way you'd say it aloud.
fn wants_gui(args: &[String]) -> bool {
    resolve_mode(explicit_mode(args), cfg!(feature = "gui"), headless_session())
}

/// The mode named on the command line, if either was. The *last* flag wins, so
/// `oxru --term --gui` reads the way you'd say it aloud.
fn explicit_mode(args: &[String]) -> Option<bool> {
    args.iter().rev().find_map(|a| match a.as_str() {
        "--gui" | "-w" | "--windowed" => Some(true),
        "--term" | "--terminal" | "--tui" | "-t" => Some(false),
        _ => None,
    })
}

/// Decide the mode. Split from the environment probing so the policy itself is
/// testable without setting process-wide variables.
fn resolve_mode(explicit: Option<bool>, gui_supported: bool, headless: bool) -> bool {
    match explicit {
        // An explicit `--gui` is honoured even headless: it can only fail
        // loudly, and second-guessing the user is worse than letting them see
        // why it didn't work. A build with no window support is the one case
        // that can't be overridden — there is no window to open.
        Some(choice) => choice && gui_supported,
        // A build compiled without the `gui` feature has no window to open, so
        // the default has to be the terminal or every bare `oxru` would fail.
        None if !gui_supported => false,
        // Nothing to display to: over SSH (or a Linux session with no display
        // server) a window can't appear, and because the windowed path detaches
        // itself the user would get *silence* rather than an error. Fall back
        // rather than vanish.
        None if headless => false,
        None => true,
    }
}

/// Whether this process has no way to put a window on screen.
fn headless_session() -> bool {
    let ssh = std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
        || std::env::var_os("SSH_CLIENT").is_some();
    if ssh {
        return true;
    }
    // macOS always has a window server for a local session; X11/Wayland don't.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return std::env::var_os("DISPLAY").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_none();
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    false
}

/// Respawn this binary as a session-detached child (like `code .` does) and
/// return immediately. `setsid()` puts the child in a brand-new session with
/// no controlling terminal, so a SIGHUP from the launching terminal closing
/// never reaches it; null stdio means it doesn't hold the terminal's pty open
/// either (all real output already goes to the log file, see `logging.rs`).
#[cfg(all(unix, feature = "gui"))]
fn relaunch_gui_detached(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    unsafe extern "C" {
        fn setsid() -> i32;
    }

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env("OXRU_GUI_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

fn print_usage() {
    println!(
        "Oxru — a hackable code editor for the terminal and the desktop\n\n\
         USAGE:\n    oxru [OPTIONS] [PROJECT_DIR]\n\n\
         Opens in its own window by default.\n\n\
         OPTIONS:\n\
         \x20   -t, --term, --terminal  Run inside this terminal instead of a window\n\
         \x20   -w, --gui, --windowed   Open in a window (the default; forces it\n\
         \x20                           even over SSH)\n\
         \x20   -h, --help              Show this help\n\n\
         Over SSH, or on a build without window support, the terminal is used\n\
         automatically. If PROJECT_DIR is omitted, the welcome screen opens."
    );
}

/// Run the editor in the host terminal (the default).
fn run_tui(mut app: App) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

/// Poll cadence for the terminal loop. While a pane is streaming output or the
/// user is typing we poll quickly so echo and live output stay smooth; once
/// things go quiet we fall back to a 1-second idle poll instead of busy-waiting.
/// The GUI loop mirrors this exact principle (see `gui::IDLE_REFRESH`): refresh
/// on activity, with a 1-second floor — never a fast timer spinning on nothing.
const ACTIVE_POLL: Duration = Duration::from_millis(16);
const IDLE_POLL: Duration = Duration::from_secs(1);

fn run(terminal: &mut Tui, app: &mut App) -> Result<()> {
    // Stay in fast-poll ("active") mode until this instant; it's bumped a second
    // into the future on every chunk of terminal output and every input event,
    // so streaming and typing stay responsive while a truly idle editor drops to
    // a 1-second poll.
    let mut active_until = Instant::now();
    loop {
        // Keep terminal output flowing even when the user isn't typing.
        let mut pumped = 0usize;
        for t in app.terminals.iter_mut() {
            pumped += t.pump();
        }
        if pumped > 0 {
            active_until = Instant::now() + IDLE_POLL;
        }
        // Keep a selection drag held against an edge auto-scrolling.
        app.mouse_drag_tick();
        // Open embedded terminals for any scripts that requested a new window.
        app.poll_terminal_requests();
        // Reload files changed on disk (or flag conflicts).
        app.poll_file_changes();
        app.poll_memory();
        app.poll_git();
        // Auto-run a debounced "Search in Files" query once typing settles,
        // and adopt any background search job that finished since the last
        // tick. Stay in fast-poll mode while a job is in flight, same as
        // streaming terminal output — otherwise the results could sit
        // computed-but-unshown for up to a second at the idle floor.
        let search_started = app.poll_pending_search();
        let search_finished = app.poll_search_results();
        if search_started || search_finished || app.project_search.in_flight {
            active_until = Instant::now() + IDLE_POLL;
        }
        terminal.draw(|f| ui::render(f, app))?;

        let timeout = if Instant::now() < active_until {
            ACTIVE_POLL
        } else if app.active_editor.is_some() {
            // A stationary caret still needs to blink: wake at half the blink
            // period rather than the full idle floor, or it would sit solid
            // between keystrokes instead of pulsing.
            App::blink_poll_interval()
        } else {
            IDLE_POLL
        };
        if event::poll(timeout)? {
            // The user is interacting — keep polling fast so it feels instant.
            active_until = Instant::now() + IDLE_POLL;
            match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    input::handle_key(app, key);
                }
                Event::Mouse(me) => {
                    // Same unified routing the GUI uses, so terminal selection,
                    // wheel and mouse-reporting all work identically in the TUI.
                    let shift = me.modifiers.contains(KeyModifiers::SHIFT);
                    let alt = me.modifiers.contains(KeyModifiers::ALT);
                    match me.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            app.mouse_down(me.column, me.row, shift, alt)
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            app.mouse_drag(me.column, me.row)
                        }
                        MouseEventKind::Up(MouseButton::Left) => app.mouse_up(me.column, me.row),
                        MouseEventKind::ScrollUp => app.mouse_wheel(3, me.column, me.row, shift),
                        MouseEventKind::ScrollDown => {
                            app.mouse_wheel(-3, me.column, me.row, shift)
                        }
                        MouseEventKind::Moved => app.mouse_move(me.column, me.row),
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // A folder-open request (⌘/Ctrl+O) pops the native picker on the main
        // thread, then opens the chosen folder.
        if app.take_open_folder_request() {
            if let Some(path) = picker::pick_folder() {
                app.open_picked_folder(path);
            }
        }

        if app.session_dirty {
            app.save_session();
        }
        if app.should_quit {
            app.save_session();
            break;
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // A thin bar for the editor caret (VSCode-style). The terminal draws it with
    // its own hardware cursor, so it sits between cells without ever covering the
    // character it's next to — unlike a glyph we'd paint into a cell. We use the
    // *steady* bar and blink it ourselves (show/hide on our clock): the hardware
    // blink resets every time we reposition the cursor on redraw, so it would
    // otherwise sit solid. See `render_editor_pane` + the blink-paced poll below.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        SetCursorStyle::SteadyBar
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Make sure a panic doesn't leave the user's terminal in raw/alt-screen mode.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Record the panic in the log file before the process unwinds — in GUI
        // mode there's no visible stderr, so this is the only trace left behind.
        tracing::error!("panic: {info}");
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        original(info);
    }));
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn windowed_is_the_default() {
        assert_eq!(explicit_mode(&args(&["."])), None, "a bare path names no mode");
        assert!(resolve_mode(None, true, false), "no flag opens a window");
    }

    #[test]
    fn term_opts_into_the_terminal() {
        for flag in ["--term", "--terminal", "--tui", "-t"] {
            assert_eq!(explicit_mode(&args(&[flag, "."])), Some(false), "{flag}");
            assert!(!resolve_mode(Some(false), true, false), "{flag}");
        }
    }

    #[test]
    fn gui_flags_still_work() {
        // Existing commands, scripts and muscle memory must keep working.
        for flag in ["--gui", "--windowed", "-w"] {
            assert_eq!(explicit_mode(&args(&[flag, "."])), Some(true), "{flag}");
        }
    }

    #[test]
    fn the_last_flag_wins() {
        assert_eq!(explicit_mode(&args(&["--term", "--gui"])), Some(true));
        assert_eq!(explicit_mode(&args(&["--gui", "--term"])), Some(false));
    }

    #[test]
    fn a_headless_session_falls_back_to_the_terminal() {
        // Over SSH the windowed path detaches itself, so without this the user
        // would get silence rather than a window or an error.
        assert!(!resolve_mode(None, true, true), "no display: use the terminal");
        // …but an explicit --gui is still honoured, so it can fail visibly
        // rather than being silently overridden.
        assert!(resolve_mode(Some(true), true, true), "--gui is not second-guessed");
    }

    #[test]
    fn a_build_without_window_support_always_uses_the_terminal() {
        // Otherwise `cargo install --no-default-features` would produce a
        // binary where every bare `oxru` fails.
        assert!(!resolve_mode(None, false, false));
        assert!(!resolve_mode(Some(true), false, false), "even with --gui: there is no window");
    }
}
