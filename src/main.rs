//! Oxru — a small terminal/GUI code editor.
//!
//! The UI is intentionally minimal: a blank screen and a single file dialog
//! (open with `Option+F`) for searching, opening, and managing files; `Ctrl+F`
//! finds within the open file. It can run in the terminal or, with `--gui`, in
//! its own window.

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
mod ui;

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
    let want_gui = args
        .iter()
        .any(|a| matches!(a.as_str(), "--gui" | "-w" | "--windowed"));

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
        "Oxru — a hackable, TUI-only code editor\n\n\
         USAGE:\n    oxru [OPTIONS] [PROJECT_DIR]\n\n\
         OPTIONS:\n\
         \x20   -w, --windowed, --gui   Open in a window (GUI) instead of the terminal\n\
         \x20   -h, --help              Show this help\n\n\
         If PROJECT_DIR is omitted, the current directory is opened."
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
        // Auto-run a debounced "Search in Files" query once typing settles.
        app.poll_pending_search();
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
