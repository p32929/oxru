//! Windowed ("GUI") mode.
//!
//! Renders the *exact same* ratatui UI as the terminal build, but into a real
//! OS window via [`ratatui_wgpu`] + [`winit`], using a **bundled monospace font**
//! (JuliaMono). This means colours, box-drawing, and Unicode glyphs render
//! consistently regardless of what the host terminal supports — which is the
//! whole point of `--gui`.
//!
//! The editor logic is untouched: winit key events are translated into the same
//! crossterm `KeyEvent`s that [`crate::input`] already understands, and the same
//! `App` drives everything. Embedded PTY terminals, plugins, search, etc. all
//! work as in the terminal build.
//!
//! Notes / current limitations:
//! - The bundled font has broad Unicode coverage but not Nerd glyphs, so the
//!   window defaults to the `unicode` icon set.
//! - Mouse input is not yet wired in GUI mode (everything is reachable by
//!   keyboard and the Command Palette).
//! - The text cursor is not drawn (a ratatui-wgpu limitation); cursor position
//!   still behaves correctly.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui_wgpu::{Builder, Dimensions, Font, WgpuBackend};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent as WinitKeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, KeyCode as PhysKeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::app::App;
use crate::{input, ui};

/// Bundled fonts, loaded from this repo's `assets/fonts/` (never the system).
/// JuliaMono is the text font — chosen for its huge Unicode coverage, which
/// includes box-drawing and the macOS keyboard glyphs (⌃⇧⌥) used in shortcuts.
/// The Symbols Nerd Font supplies the file/folder icon glyphs as a fallback.
const FONT: &[u8] = include_bytes!("../assets/fonts/JuliaMono.ttf");
const ICON_FONT: &[u8] = include_bytes!("../assets/fonts/SymbolsNerdFontMono.ttf");

/// Build a fresh wgpu-backed terminal for `window` at `font_px`. Used at startup
/// and whenever the font size or display scale changes — rebuilding (rather than
/// patching the font in place) guarantees a clean cell grid with no ghosting.
fn build_terminal(
    window: Arc<Window>,
    font_px: u32,
    fg: ratatui::style::Color,
    bg: ratatui::style::Color,
) -> Option<Terminal<WgpuBackend<'static, 'static>>> {
    let size = window.inner_size();
    let font = Font::new(FONT)?;
    let backend = pollster::block_on(
        Builder::from_font(font)
            // Icon glyphs come from the bundled Nerd font as a fallback.
            .with_fonts(Font::new(ICON_FONT))
            .with_font_size_px(font_px)
            // Make "default"-coloured cells use the theme, not black-on-white.
            .with_fg_color(fg)
            .with_bg_color(bg)
            .with_width_and_height(Dimensions {
                width: NonZeroU32::new(size.width.max(1)).unwrap(),
                height: NonZeroU32::new(size.height.max(1)).unwrap(),
            })
            .build_with_target(window),
    )
    .ok()?;
    Terminal::new(backend).ok()
}

/// The manual game-loop frame interval — we `thread::sleep` this between each
/// `pump_app_events` + render. A plain thread sleep is the one cadence macOS
/// cannot throttle (unlike the run-loop timers/proxy wakes that `run_app` relies
/// on, which the OS coalesces to 1–2.5 s whenever Oxru isn't the active app — the
/// "terminal froze until I clicked it" bug). At 16 ms the on-screen terminal
/// streams live at ~60fps regardless of focus/occlusion. We only composite when
/// something actually changed, so an idle window mostly just sleeps.
const RENDER_TICK: Duration = Duration::from_millis(16);

/// Repaint floor for time-based UI (toasts auto-expiring, the foreground-process
/// label) so it still updates when neither input nor PTY output triggered a frame.
const IDLE_REPAINT: Duration = Duration::from_millis(250);

/// How often to force a *full* repaint (`terminal.clear()` + redraw) that bypasses
/// ratatui's cell diff. This is the load-bearing fix for the "terminal output
/// freezes" bug: ratatui only presents cells that changed since the last `draw()`.
/// When the window is occluded, our tick keeps pumping output and calling `draw()`,
/// so the diff advances against a Metal drawable that isn't being composited; the
/// emulator's last-buffer then equals its current-buffer, so on returning to view a
/// plain `draw()` produces an EMPTY diff and presents nothing — the screen stays
/// stuck on the pre-occlusion frame. Re-presenting every cell on a 1s floor (and
/// immediately on focus/occlusion-return, via `force_redraw`) guarantees the
/// visible surface can never stay stale longer than a second, whatever the OS did
/// to the swapchain while we were backgrounded.
const FULL_REDRAW: Duration = Duration::from_secs(1);

struct Gui {
    app: App,
    window: Option<Arc<Window>>,
    terminal: Option<Terminal<WgpuBackend<'static, 'static>>>,
    modifiers: ModifiersState,
    /// Last known cursor position in physical pixels (for click hit-testing).
    cursor_pos: (f64, f64),
    /// Whether the left mouse button is currently held (for drag selection).
    left_pressed: bool,
    /// The font size currently applied to the backend (to detect live changes).
    applied_font_size: u32,
    /// The window title currently set (to detect folder switches).
    applied_title: String,
    /// Set by a window event (input/resize/focus) to request a repaint on the
    /// next manual-loop tick. Terminal output doesn't use this — the tick pumps
    /// every terminal each frame and repaints if any produced bytes.
    dirty: bool,
    /// When we last composited a frame — used to repaint time-based UI (toasts,
    /// the foreground-process label) at a low floor even when nothing else changed.
    last_render: Instant,
    /// Force the next `render()` to do a full, diff-bypassing repaint (see
    /// `FULL_REDRAW`). Set on focus/occlusion-return and OS redraw requests so a
    /// surface left stale while backgrounded is rewritten the instant we're visible.
    force_redraw: bool,
    /// When we last did a full repaint — drives the `FULL_REDRAW` safety floor.
    last_full: Instant,
    /// Diagnostics: frames composited and PTY bytes pumped since the last
    /// heartbeat, plus the window visibility, so a "frozen" report is pinnable.
    frames: u64,
    bytes: u64,
    last_beat: Instant,
    focused: bool,
    occluded: bool,
}

/// The window title for the app's current state: "Oxru - <folder>", or just
/// "Oxru" when no folder is open.
fn window_title(app: &App) -> String {
    match &app.root {
        Some(root) => {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.to_string_lossy().into_owned());
            format!("Oxru - {name}")
        }
        None => "Oxru".to_string(),
    }
}

/// Run the editor in a window. Returns when the window is closed or the app
/// quits. Icons render from the bundled Nerd font, so the configured icon set
/// (Nerd by default) works here just like in a Nerd-Font terminal.
/// Prevent macOS **App Nap** from suspending this process while the window is
/// backgrounded/occluded.
///
/// App Nap throttles — and, as our logs proved, fully *suspends* — a
/// backgrounded app's threads, including the PTY reader threads. The result is
/// embedded-terminal output that "freezes" until the window is focused again
/// and a keypress wakes the process. The `NSAppSleepDisabled` Info.plist key is
/// too weak to stop this reliably; the robust, Apple-sanctioned mechanism (the
/// same one iTerm2, Terminal.app and browsers use) is to hold an `NSProcessInfo`
/// *activity* for the process lifetime.
///
/// We use `UserInitiatedAllowingIdleSystemSleep` (opt out of App Nap, but still
/// allow normal idle sleep) **OR'd with `LatencyCritical`** — and that second
/// flag is the load-bearing one for the "backgrounded terminal stops refreshing"
/// bug. winit drives its redraw loop with a `CFRunLoopTimer`, and macOS
/// *coalesces* that timer (stretches it to 1–2.5 s, as our latency probe caught)
/// once the window is occluded. `LatencyCritical` is the documented API to
/// **disable timer coalescing** for the process, so the redraw timer keeps firing
/// on time even in the background. It's meant to be used sparingly (it costs a
/// little power), which is exactly right for a terminal you're watching stream.
#[cfg(target_os = "macos")]
fn prevent_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};

    let info = NSProcessInfo::processInfo();
    let reason = NSString::from_str("oxru keeps embedded terminals streaming while backgrounded");
    let token = info.beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
            | NSActivityOptions::LatencyCritical,
        &reason,
    );
    // Hold the activity for the entire process lifetime by leaking the token —
    // dropping it would end the activity and re-enable App Nap.
    std::mem::forget(token);
    tracing::info!("macOS App Nap prevention active (NSProcessInfo activity held)");
}

#[cfg(not(target_os = "macos"))]
fn prevent_app_nap() {}

pub fn run(app: App) -> Result<()> {
    // Stop macOS from App-Napping (suspending) us in the background, so our
    // manual loop's thread keeps running at full speed even when Oxru isn't the
    // active app. Must happen before the event loop starts.
    prevent_app_nap();

    let mut event_loop = EventLoop::<()>::with_user_event()
        .build()
        .map_err(|e| anyhow!("creating event loop: {e}"))?;

    let applied_font_size = app.gui_font_size;
    let mut gui = Gui {
        app,
        window: None,
        terminal: None,
        modifiers: ModifiersState::empty(),
        cursor_pos: (0.0, 0.0),
        left_pressed: false,
        applied_font_size,
        applied_title: String::new(),
        dirty: true, // draw the first frame as soon as the window exists
        last_render: Instant::now(),
        force_redraw: true, // first frame is a full present
        last_full: Instant::now(),
        frames: 0,
        bytes: 0,
        last_beat: Instant::now(),
        focused: true,
        occluded: false,
    };

    // The load-bearing fix for the recurring "embedded terminal stops refreshing
    // until I click it" bug. We DON'T use `run_app`, which parks the main thread
    // in the Cocoa run loop and waits for macOS to wake it — macOS throttles that
    // wake (timers, proxy events, RedrawRequested all route through the run loop)
    // whenever Oxru isn't the *active* application, so a backgrounded window stops
    // repainting. Instead we drive a manual game loop: `pump_app_events` with a
    // zero timeout drains pending input WITHOUT parking, then we render and PTY-
    // pump on our own `thread::sleep` clock. A thread sleep is the one cadence
    // macOS can't coalesce, so the on-screen terminal stays live at ~60fps
    // regardless of focus/occlusion/active-app state.
    loop {
        let status = event_loop.pump_app_events(Some(Duration::ZERO), &mut gui);
        if let PumpStatus::Exit(code) = status {
            if code != 0 {
                return Err(anyhow!("event loop exited with code {code}"));
            }
            break;
        }
        gui.tick();
        std::thread::sleep(RENDER_TICK);
    }
    Ok(())
}

impl ApplicationHandler for Gui {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Title carries the folder name so multiple windows are tellable apart.
        let title = window_title(&self.app);
        self.applied_title = title.clone();
        let attrs = WindowAttributes::default()
            .with_title(title)
            .with_inner_size(LogicalSize::new(1280.0, 820.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                tracing::error!(error = %e, "failed to create window");
                eprintln!("oxru: failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        // The surface is sized in *physical* pixels, so the font must be scaled
        // by the display's scale factor (e.g. 2× on a Retina screen) or the text
        // ends up tiny. `font_size` is the comfortable logical point size.
        let scale = window.scale_factor().max(1.0);
        let font_px = ((self.app.gui_font_size as f64) * scale).round().max(8.0) as u32;

        match build_terminal(window.clone(), font_px, self.app.theme.fg, self.app.theme.bg) {
            Some(t) => self.terminal = Some(t),
            None => {
                tracing::error!("failed to initialise the GPU surface / font");
                eprintln!("oxru: failed to initialise the GPU surface / font");
                event_loop.exit();
                return;
            }
        }
        self.applied_font_size = self.app.gui_font_size;
        self.window = Some(window);
        tracing::info!(scale, font_px, "gui window ready");
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        // Any state-changing input marks the window dirty so it redraws once,
        // afterwards. (Idle with no terminal open means no events and no
        // redraws — the editor only repaints when something actually changes.)
        let mut dirty = false;
        match event {
            WindowEvent::CloseRequested => {
                // Run the same confirmation as Ctrl+Q: only exit if nothing needs
                // saving / no terminal is busy; otherwise show the prompt.
                self.app.request_quit();
                if self.app.should_quit {
                    event_loop.exit();
                } else {
                    self.dirty = true; // show the save/quit prompt on the next tick
                }
                return;
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::Resized(size) => {
                if let Some(t) = &mut self.terminal {
                    t.backend_mut().resize(size.width, size.height);
                }
                dirty = true;
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // Keep text physically the same size when moving between displays.
                // Rebuild the backend (not just update_fonts) so the cell grid is
                // re-sized cleanly instead of leaving stale cells behind.
                if let Some(w) = &self.window {
                    let px = ((self.app.gui_font_size as f64) * scale_factor.max(1.0))
                        .round()
                        .max(8.0) as u32;
                    if let Some(t) =
                        build_terminal(w.clone(), px, self.app.theme.fg, self.app.theme.bg)
                    {
                        self.terminal = Some(t);
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(key) = translate_key(&event, self.modifiers) {
                        input::handle_key(&mut self.app, key);
                        dirty = true;
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor_pos = (position.x, position.y);
                // Drag extends the text selection — in the terminal or the editor.
                if self.left_pressed {
                    if let Some((col, row)) = self.cursor_cell() {
                        self.app.mouse_drag(col, row);
                    }
                    dirty = true;
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            self.left_pressed = true;
                            if let Some((col, row)) = self.cursor_cell() {
                                self.app.mouse_down(col, row, self.modifiers.shift_key());
                            }
                        }
                        ElementState::Released => {
                            self.left_pressed = false;
                            let (col, row) = self.cursor_cell().unwrap_or((0, 0));
                            self.app.mouse_up(col, row);
                        }
                    }
                    dirty = true;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i32,
                    winit::event::MouseScrollDelta::PixelDelta(p) => (p.y / 16.0).round() as i32,
                };
                if lines != 0 {
                    let (col, row) = self.cursor_cell().unwrap_or((0, 0));
                    self.app
                        .mouse_wheel(lines, col, row, self.modifiers.shift_key());
                    dirty = true;
                }
            }
            // Coming back to the window (regaining focus, or being un-occluded
            // after sitting behind another window) must force a repaint: macOS
            // throttles a backgrounded app, so the terminal can be many frames
            // behind, and nothing else would redraw it until the next keypress.
            // `draw` pumps all pending output, so one redraw fully catches up.
            WindowEvent::Focused(focused) => {
                tracing::info!(focused, "window focus changed");
                self.focused = focused;
                if focused {
                    // Coming back to the window is a good moment to catch files an
                    // external editor changed while we were away.
                    self.app.recheck_files_soon();
                    self.app.poll_file_changes();
                    // Force a full present: while we were unfocused the surface may
                    // hold a stale frame whose diff is already "consumed".
                    self.force_redraw = true;
                    dirty = true;
                } else {
                    // A mouse-up is easily missed when the window loses focus
                    // mid-drag (e.g. Cmd-Tab while selecting), leaving a phantom
                    // held button that makes `about_to_wait` repaint every single
                    // tick — the runaway-render spin seen in the logs. Drop it.
                    self.left_pressed = false;
                }
            }
            WindowEvent::Occluded(occluded) => {
                tracing::info!(occluded, "window occlusion changed");
                self.occluded = occluded;
                if occluded {
                    self.left_pressed = false; // same phantom-drag guard as focus loss
                } else {
                    // Un-occluded: the swapchain may have a stale/old frame from
                    // before we were hidden. Re-present every cell, not just a diff.
                    self.force_redraw = true;
                    dirty = true;
                }
            }
            WindowEvent::RedrawRequested => {
                // The OS asked for a repaint (e.g. after a resize or display change);
                // honour it with a full present so nothing stale survives.
                self.force_redraw = true;
                dirty = true;
            }
            _ => {}
        }
        // Just flag a repaint; the manual loop's `tick()` composites the frame on
        // its own clock (so the screen never depends on the OS delivering a wake).
        if dirty {
            self.dirty = true;
        }
        if self.app.should_quit {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Intentionally empty: with the manual `pump_app_events` loop, all pumping,
        // rendering, and housekeeping happens in `Gui::tick()` on our own clock —
        // not here, which winit only calls when the (throttled) run loop decides to.
    }
}

impl Gui {
    /// One iteration of the manual loop: housekeeping + PTY pump + a repaint when
    /// something changed. Runs on our `thread::sleep` clock, not winit's — so the
    /// on-screen terminal stays live even when Oxru isn't the active app.
    fn tick(&mut self) {
        if self.app.session_dirty {
            self.app.save_session();
        }
        if self.app.should_quit {
            self.app.save_session();
            return; // the run loop notices should_quit and exits
        }
        // A folder-open request (⌘O) pops the native picker here on the main
        // thread, then opens the chosen folder (reuse window or spawn a new one).
        if self.app.take_open_folder_request() {
            if let Some(path) = crate::picker::pick_folder() {
                self.app.open_picked_folder(path);
            }
            self.dirty = true;
        }
        // Open embedded terminals for any scripts that requested a new window.
        self.app.poll_terminal_requests();
        // Reload files changed on disk (or flag conflicts).
        self.app.poll_file_changes();
        // Reflect a folder switch (window reuse) in the title bar.
        let title = window_title(&self.app);
        if title != self.applied_title {
            if let Some(w) = &self.window {
                w.set_title(&title);
            }
            self.applied_title = title;
        }
        // Apply a live font-size change from the Settings dialog by rebuilding
        // the backend at the new size (a clean cell grid, no ghosting).
        if self.app.gui_font_size != self.applied_font_size {
            if let Some(w) = &self.window {
                let scale = w.scale_factor().max(1.0);
                let px = ((self.app.gui_font_size as f64) * scale).round().max(8.0) as u32;
                if let Some(t) =
                    build_terminal(w.clone(), px, self.app.theme.fg, self.app.theme.bg)
                {
                    self.terminal = Some(t);
                }
                self.applied_font_size = self.app.gui_font_size;
            }
        }
        // Drain PTY output (the reader threads already parsed it off-thread; this
        // settles the scroll view + foreground label and reports whether to
        // repaint). A held edge-drag keeps the selection scrolling frame by frame.
        let pumped = self.pump_all();
        if self.left_pressed {
            self.app.mouse_drag_tick();
            self.dirty = true;
        }
        // Composite a frame when input/resize flagged it, when output arrived, or
        // when the idle floor elapsed (so toasts / labels still update). Because
        // the emulator is kept current by the reader threads, this shows live
        // output on every tick — no keypress required, regardless of active-app.
        if self.dirty || pumped > 0 || self.last_render.elapsed() >= IDLE_REPAINT {
            self.dirty = false;
            self.render();
            self.last_render = Instant::now();
        }
        self.beat();
    }
}

impl Gui {
    /// The character cell `(col, row)` under the mouse cursor, if resolvable.
    fn cursor_cell(&self) -> Option<(u16, u16)> {
        let (window, terminal) = (self.window.as_ref()?, self.terminal.as_ref()?);
        let phys = window.inner_size();
        let grid = terminal.backend().size().ok()?;
        if phys.width == 0 || phys.height == 0 || grid.width == 0 || grid.height == 0 {
            return None;
        }
        let col = (self.cursor_pos.0 * grid.width as f64 / phys.width as f64).floor();
        let row = (self.cursor_pos.1 * grid.height as f64 / phys.height as f64).floor();
        if col < 0.0 || row < 0.0 {
            return None;
        }
        Some((col as u16, row as u16))
    }

    /// Drain every terminal's pending PTY output into its emulator (the reader
    /// thread already parsed it; this settles the scroll view + foreground label).
    /// Returns total bytes pumped, so the caller can decide whether to repaint.
    fn pump_all(&mut self) -> u64 {
        let mut total = 0u64;
        for term in self.app.terminals.iter_mut() {
            total += term.pump() as u64;
        }
        self.bytes += total;
        total
    }

    /// Composite one frame of the ratatui UI into the GPU surface.
    ///
    /// ratatui only sends the GPU the cells that changed since the last `draw()`. If
    /// the visible surface was left stale while we were occluded (the diff advanced
    /// against a non-composited drawable), a plain `draw()` would present nothing and
    /// the screen would stay frozen. So when `force_redraw` is set (focus/occlusion
    /// return, OS redraw) or the `FULL_REDRAW` floor elapsed, `terminal.clear()`
    /// resets the diff so the next `draw()` re-presents every cell — guaranteeing the
    /// on-screen frame can never lag the emulator by more than `FULL_REDRAW`.
    fn render(&mut self) {
        let full = self.force_redraw || self.last_full.elapsed() >= FULL_REDRAW;
        if full {
            self.force_redraw = false;
            self.last_full = Instant::now();
        }
        if let Some(terminal) = &mut self.terminal {
            if full {
                // Doesn't present (no flash) — just drops the diff baseline so the
                // following draw re-sends the whole grid and forces a present.
                let _ = terminal.clear();
            }
            let app = &mut self.app;
            if let Err(e) = terminal.draw(|f| ui::render(f, app)) {
                tracing::warn!(error = %e, "frame draw failed");
            }
        }
        self.frames += 1;
    }

    /// Heartbeat (~every 5s while a terminal is open): frames composited and PTY
    /// bytes pumped, plus window visibility. Logged at **info** so it shows at the
    /// default level — the line to read if output ever sticks again.
    fn beat(&mut self) {
        if self.last_beat.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                frames = self.frames,
                bytes = self.bytes,
                terminals = self.app.terminals.len(),
                focused = self.focused,
                occluded = self.occluded,
                "draw heartbeat"
            );
            self.frames = 0;
            self.bytes = 0;
            self.last_beat = Instant::now();
        }
    }
}

/// Translate a winit key event (+ current modifiers) into the crossterm
/// `KeyEvent` that the shared input layer expects.
fn translate_key(ev: &WinitKeyEvent, mods: ModifiersState) -> Option<KeyEvent> {
    let mut m = KeyModifiers::empty();
    if mods.control_key() {
        m |= KeyModifiers::CONTROL;
    }
    if mods.shift_key() {
        m |= KeyModifiers::SHIFT;
    }
    if mods.alt_key() {
        m |= KeyModifiers::ALT;
    }
    if mods.super_key() {
        m |= KeyModifiers::SUPER;
    }
    // Alt/Option composes special characters (e.g. ⌥T -> "†" on macOS), and Cmd
    // combos can report odd logical keys, so for either resolve the key from its
    // physical position instead of the composed character.
    if mods.alt_key() || mods.super_key() {
        if let Some(c) = physical_char(ev.physical_key) {
            return Some(KeyEvent::new(KeyCode::Char(c), m));
        }
    }
    let code = match &ev.logical_key {
        Key::Named(named) => match named {
            NamedKey::Enter => KeyCode::Enter,
            NamedKey::Backspace => KeyCode::Backspace,
            NamedKey::Delete => KeyCode::Delete,
            NamedKey::Escape => KeyCode::Esc,
            NamedKey::Tab => KeyCode::Tab,
            NamedKey::Space => KeyCode::Char(' '),
            NamedKey::ArrowUp => KeyCode::Up,
            NamedKey::ArrowDown => KeyCode::Down,
            NamedKey::ArrowLeft => KeyCode::Left,
            NamedKey::ArrowRight => KeyCode::Right,
            NamedKey::Home => KeyCode::Home,
            NamedKey::End => KeyCode::End,
            NamedKey::PageUp => KeyCode::PageUp,
            NamedKey::PageDown => KeyCode::PageDown,
            NamedKey::F1 => KeyCode::F(1),
            NamedKey::F2 => KeyCode::F(2),
            NamedKey::F3 => KeyCode::F(3),
            NamedKey::F4 => KeyCode::F(4),
            NamedKey::F5 => KeyCode::F(5),
            NamedKey::F6 => KeyCode::F(6),
            NamedKey::F7 => KeyCode::F(7),
            NamedKey::F8 => KeyCode::F(8),
            NamedKey::F9 => KeyCode::F(9),
            NamedKey::F10 => KeyCode::F(10),
            NamedKey::F11 => KeyCode::F(11),
            NamedKey::F12 => KeyCode::F(12),
            _ => return None,
        },
        Key::Character(s) => KeyCode::Char(s.chars().next()?),
        _ => return None,
    };
    Some(KeyEvent::new(code, m))
}

/// The base character for a physical key position (US layout): used for Alt
/// combos, where the layout-resolved character is unreliable.
fn physical_char(pk: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = pk else {
        return None;
    };
    use PhysKeyCode::*;
    Some(match code {
        KeyA => 'a', KeyB => 'b', KeyC => 'c', KeyD => 'd', KeyE => 'e',
        KeyF => 'f', KeyG => 'g', KeyH => 'h', KeyI => 'i', KeyJ => 'j',
        KeyK => 'k', KeyL => 'l', KeyM => 'm', KeyN => 'n', KeyO => 'o',
        KeyP => 'p', KeyQ => 'q', KeyR => 'r', KeyS => 's', KeyT => 't',
        KeyU => 'u', KeyV => 'v', KeyW => 'w', KeyX => 'x', KeyY => 'y',
        KeyZ => 'z',
        Digit1 => '1', Digit2 => '2', Digit3 => '3', Digit4 => '4', Digit5 => '5',
        Digit6 => '6', Digit7 => '7', Digit8 => '8', Digit9 => '9', Digit0 => '0',
        // So Option+comma (Settings) survives macOS turning it into "≤".
        Comma => ',',
        _ => return None,
    })
}
