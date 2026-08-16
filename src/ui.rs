//! All ratatui rendering. The UI is intentionally minimal: a blank screen with
//! one hint (or an open file), a thin status footer, and the file dialog /
//! prompt overlays.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as TSpan};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Dialog, TabHit, ToastKind};
use crate::prompt::PromptKind;
use crate::theme::Theme;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(app.theme.bg)),
        area,
    );
    // Repopulated below by `render_editor_pane` (GUI mode only) — the caller
    // reads this after the frame to drive the GPU caret overlay.
    app.gui_carets.clear();

    // Leave the window's very last physical row blank: the GUI window has
    // native (rounded) corners, and text drawn flush against the bottom edge
    // has its leftmost glyph clipped by the corner mask (rows further up
    // aren't close enough to a corner to be affected) — a 1-row margin keeps
    // real content clear of it. Already background-painted above, so this is
    // just "don't put anything else there".
    // Inset the content from the left and right window edges in windowed mode.
    // Glyphs drawn in column 0 sit flush against the frame with no optical
    // margin at all, which is the difference between a window that looks
    // designed and one that looks like a terminal emulator with the padding
    // turned off. Costs two columns, and only where there's a real window edge
    // to be flush against — in a host terminal the emulator supplies its own
    // padding, so insetting again would just waste columns.
    let pad = app.gui_padding.min(area.width.saturating_sub(1) / 2);
    let content_area = Rect::new(
        area.x + pad,
        area.y,
        area.width.saturating_sub(pad * 2),
        area.height.saturating_sub(1),
    );

    // Full-screen main area with a status footer: while editing, three rows of
    // shortcut hints (laid out as an aligned table, like the dialog footers),
    // a divider, then a status row (not a shortcut, so it's set off from the
    // hints above it); one line otherwise.
    let footer_h = if app.active_editor.is_some() { 5 } else { 1 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_h)])
        .split(content_area);

    if app.active_editor.is_some() {
        // Tab strip on top (as many rows as it needs to wrap all the open
        // tabs), an optional breadcrumb bar, editor below.
        let show_breadcrumb = app.breadcrumb_row_shown();
        let tab_rows = tab_grid_rows(app.editors.len(), rows[0].width) as u16;
        let constraints = if show_breadcrumb {
            vec![Constraint::Length(tab_rows), Constraint::Length(1), Constraint::Min(1)]
        } else {
            vec![Constraint::Length(tab_rows), Constraint::Min(1)]
        };
        let ed = Layout::default().direction(Direction::Vertical).constraints(constraints).split(rows[0]);
        render_tabs(f, ed[0], app);
        if show_breadcrumb {
            render_breadcrumb(f, ed[1], app);
            render_editor(f, ed[2], app);
        } else {
            app.breadcrumb_hits.clear();
            render_editor(f, ed[1], app);
        }
    } else {
        render_blank(f, rows[0], app);
    }
    render_footer(f, rows[1], app);

    // Dialogs stack bottom→top. The focused (top) one is centered and full
    // colour; each one below is the same size but nudged down-right so it peeks
    // out like a card, and drawn with a faded theme so colour falls off with
    // depth (see `Theme::dimmed`). Rendering bottom→top puts the bright focused
    // dialog on top.
    let stack = app.dialogs.clone();
    let n = stack.len();
    let base_theme = app.theme.clone();
    // Push the editor behind the stack back so the dialog reads as a layer
    // floating over it rather than a panel spliced into it. Now that dialogs
    // no longer cover ~the whole screen, what's underneath is visible around
    // them, and undimmed it competes with the thing that actually has focus.
    if n > 0 {
        dim_backdrop(f, area, &base_theme);
    }
    for (depth, d) in stack.into_iter().enumerate() {
        let below = n - 1 - depth; // 0 = focused/top
        app.theme = if below == 0 {
            base_theme.clone()
        } else {
            base_theme.dimmed(below as u32)
        };
        match d {
            Dialog::Terminal => render_terminal_modal(f, area, app, below),
            Dialog::TerminalPicker => render_terminal_picker(f, area, app, below),
            Dialog::Files => render_file_dialog(f, area, app, below),
            Dialog::Settings => render_settings(f, area, app, below),
            Dialog::Recent => render_recent(f, area, app, below),
            Dialog::Help => render_help(f, area, app, below),
            Dialog::SearchFiles => render_search_files(f, area, app, below),
            Dialog::Todos => render_todos(f, area, app, below),
            Dialog::Clipboard => render_clipboard(f, area, app, below),
        }
    }
    app.theme = base_theme;
    // The prompt (rename / delete / save-before-close) always floats on top.
    if app.prompt.active {
        render_prompt(f, area, app);
    }

    // Toast notifications float above everything, in the top-right corner.
    app.expire_toast();
    render_toast(f, area, app);
}

/// Fade everything already drawn toward the editor background, so a dialog
/// drawn on top of it stands out as the focused layer.
///
/// A cell grid has no alpha to composite with, so this re-styles the cells that
/// are already there: every foreground and background is mixed toward `bg`,
/// which is exactly what a translucent black scrim would do to them. Cells are
/// only ever pushed *toward* the background, never brightened, so no amount of
/// stacking can make the backdrop louder than the dialog over it.
fn dim_backdrop(f: &mut Frame, area: Rect, t: &Theme) {
    /// How far the backdrop moves toward the background. Enough to visibly
    /// recede while leaving the layout underneath readable — a scrim, not a
    /// blackout.
    const AMOUNT: f32 = 0.55;

    let buf = f.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            let cell = &mut buf[(x, y)];
            let style = cell.style();
            let fg = crate::theme::mix(style.fg.unwrap_or(t.fg), t.bg, AMOUNT);
            let bg = crate::theme::mix(style.bg.unwrap_or(t.bg), t.bg, AMOUNT);
            cell.set_style(style.fg(fg).bg(bg));
        }
    }
}

/// A transient corner notification ("Copied N chars", "Saved", …).
fn render_toast(f: &mut Frame, area: Rect, app: &App) {
    let Some(toast) = &app.toast else {
        return;
    };
    if !toast.is_visible() {
        return;
    }
    let t = &app.theme;
    let (accent, icon) = match toast.kind {
        ToastKind::Success => (t.green, "\u{2713}"), // ✓
        ToastKind::Error => (t.red, "\u{2717}"),     // ✗
        ToastKind::Info => (t.accent, "\u{2139}"),   // ℹ
    };
    let label = format!(" {icon}  {} ", toast.message);
    let inner_w = label.chars().count() as u16;
    // Box is content + 2 borders; clamp to the screen and keep a small margin.
    let w = (inner_w + 2).min(area.width.saturating_sub(2)).max(6);
    let h = 3u16;
    let x = area.x + area.width.saturating_sub(w + 2);
    let y = area.y + 1;
    let rect = Rect::new(x, y, w, h);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .style(Style::default().bg(t.bg_dark));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(
            label,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(t.bg_dark)),
        inner,
    );
}

fn conv_color(c: vt100::Color, default: Color, t: &Theme) -> Color {
    match c {
        vt100::Color::Default => default,
        // The first 16 are the ANSI colours, and Oxru resolves them itself
        // rather than letting the backend fall back to its hardcoded xterm
        // table — see `theme::ANSI_PALETTE` for why that table is unusable.
        // 16-255 (the 6x6x6 cube and the greyscale ramp) are already sane and
        // pass straight through.
        vt100::Color::Idx(i) if (i as usize) < t.ansi.len() => t.ansi[i as usize],
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Tab-strip dot color for a terminal: green while a foreground command is
/// actively running, yellow if one just finished and this tab hasn't been
/// looked at since, `None` when idle and already seen.
fn terminal_dot_color(term: &crate::terminalpane::TerminalPane, t: &Theme) -> Option<Color> {
    if term.is_running() {
        Some(t.green)
    } else if term.finished_unseen() {
        Some(t.yellow)
    } else {
        None
    }
}

/// The terminal modal: a centered ~80%×80% dialog holding a tab strip, the
/// terminal body (one terminal or an auto-arranged grid of all of them), and a
/// footer of Alt shortcuts.
fn render_terminal_modal(f: &mut Frame, area: Rect, app: &mut App, _below: usize) {
    let t = app.theme.clone();
    // Re-recorded by render_one_terminal for the active pane each frame. Only the
    // focused (top) terminal accepts mouse selection, so leave it cleared when
    // the terminal is buried under another dialog.
    app.terminal_view = None;
    let focused = app.top_dialog() == Some(Dialog::Terminal);
    // Whatever's actually on screen counts as "seen" — every pane in grid
    // view, or just the active tab in single-pane view — clearing its
    // finished-unseen flag before this frame's tab-strip dot is computed
    // below, so a pane being looked at right now never shows the indicator.
    if focused {
        if app.terminal_grid && app.terminals.len() > 1 {
            for term in app.terminals.iter_mut() {
                term.mark_viewed();
            }
        } else if let Some(term) = app.terminals.get_mut(app.active_terminal) {
            term.mark_viewed();
        }
    }

    // Always at the same spot, regardless of `below` — unlike the other
    // dialogs, the terminal's content is dense and constantly live (shell
    // text, a blinking cursor), so even the small "peek out" position nudge
    // other stacked dialogs get reads as a jarring shake/reflow the moment
    // something (e.g. the ⌘K quick-switcher) opens on top of it. It still
    // dims via the caller's `below`-based theme, just doesn't move.
    let rect = dialog_rect_stacked(area, 0, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    // Show a scroll indicator on the modal title when the active terminal is
    // scrolled up into history (the single-pane view has no per-pane border).
    let scrolled = app
        .terminals
        .get(app.active_terminal)
        .map(|tm| tm.scroll_offset())
        .unwrap_or(0);
    let title = if scrolled > 0 && !(app.terminal_grid && app.terminals.len() > 1) {
        format!(" Terminal  \u{2191}{scrolled} (Shift+End to bottom) ")
    } else {
        " Terminal ".to_string()
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            title,
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        // The whole modal sits on the terminal surface, so the area around and
        // between panes matches the panes themselves rather than showing a
        // darker frame through the gaps.
        .style(Style::default().bg(t.terminal_bg));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    // Tab strip (duplicate names get a disambiguating index), wrapping onto
    // more rows instead of scrolling — every terminal is always visible.
    let labels = app.terminal_labels();
    let tab_rows = tab_grid_rows(labels.len(), inner.width) as u16;

    // Footer height is fixed regardless of how many hints are showing this
    // frame (see the fixed TERMINAL_HINT_COLS below) — otherwise opening a
    // second terminal, which adds 6 more hints (Next/Prev/Move/Grid/Switch/
    // Jump), visibly resizes the whole dialog and reflows every column.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_rows),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);

    let dot_labels: Vec<(String, Option<Color>)> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| (label.clone(), app.terminals.get(i).and_then(|term| terminal_dot_color(term, &t))))
        .collect();
    let (lines, hits) =
        wrapping_tab_grid(&dot_labels, app.active_terminal, rows[0].width, &t, None);
    app.terminal_tab_hits = hits;
    app.terminal_tabstrip_rect = if focused {
        Some((rows[0].x, rows[0].y, rows[0].width, rows[0].height))
    } else {
        None
    };
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        rows[0],
    );

    // Body: a grid of all terminals, or just the active one.
    let body = rows[1];
    app.terminal_grid_rects.clear();
    if app.terminal_grid && app.terminals.len() > 1 {
        let n = app.terminals.len();
        let gcols = (n as f64).sqrt().ceil() as usize;
        let grows = n.div_ceil(gcols);
        let row_rects = Layout::default()
            .direction(Direction::Vertical)
            .constraints(vec![Constraint::Ratio(1, grows as u32); grows])
            .split(body);
        let mut idx = 0;
        for rr in row_rects.iter() {
            let col_rects = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(vec![Constraint::Ratio(1, gcols as u32); gcols])
                .split(*rr);
            for cr in col_rects.iter() {
                if idx < n {
                    // Recorded before rendering (not after) so a click lands
                    // on this cell even if `render_one_terminal` shrinks
                    // `inner` for its border — the outer cell is what's
                    // visually "this terminal" to click on.
                    if focused {
                        app.terminal_grid_rects.push((cr.x, cr.y, cr.width, cr.height));
                    }
                    render_one_terminal(f, *cr, app, idx, &labels[idx], &t, idx == app.active_terminal, true);
                    idx += 1;
                }
            }
        }
    } else if app.active_terminal < app.terminals.len() {
        let i = app.active_terminal;
        render_one_terminal(f, body, app, i, &labels[i], &t, true, false);
    }

    // Footer of shortcuts, laid out across two justified lines so it never
    // overflows (mouse works too: wheel scrolls, drag selects).
    let (copy_key, paste_key) = if cfg!(target_os = "macos") {
        ("\u{2318}C", "\u{2318}V")
    } else {
        ("\u{2303}\u{21e7}C", "\u{2303}\u{21e7}V")
    };
    let actions: Vec<(String, String)> = if app.terminal_copy_mode() {
        // Copy mode owns the keyboard: show its controls instead.
        vec![
            ("\u{2190}\u{2191}\u{2193}\u{2192}".into(), "Move".into()),
            ("\u{21e7}\u{2190}\u{2191}\u{2193}\u{2192}".into(), "Mark".into()),
            ("\u{2325}\u{2190}\u{2192}".into(), "Word".into()),
            (format!("\u{21b5} / y / {copy_key}"), "Copy".into()),
            ("\u{21e7}PgUp/Dn".into(), "Scroll".into()),
            ("Esc / q".into(), "Exit".into()),
        ]
    } else {
        // These are all handled locally by the embedded terminal's own key
        // handling (see `input::handle_terminal_modal`), which deliberately
        // keeps its own literal chords to keep behaving like a real
        // terminal app — so they're always shown in their 2-key GUI style,
        // regardless of `app.gui`. Quit is the one exception: it's the
        // global ⌘Q, not a terminal-local binding, so it does follow
        // `app.gui`.
        let mut actions: Vec<(String, String)> = vec![
            (alt_str("N"), "New".into()),
            (alt_str("W"), "Close".into()),
        ];
        if app.terminals.len() > 1 {
            // Switching / moving mirror the editor tabs (Ctrl+Tab, Ctrl+Shift+,/.).
            actions.push((ctrl_str("Tab"), "Next".into()));
            actions.push((ctrl_str("\u{21e7}Tab"), "Prev".into()));
            actions.push((ctrl_str("\u{21e7}\u{2194}"), "Move".into()));
            actions.push((alt_str("G"), if app.terminal_grid { "Tabs".into() } else { "Grid".into() }));
            actions.push((cmd_str("K"), "Switch\u{2026}".into()));
            actions.push((cmd_str("1-9"), "Jump to tab".into()));
        }
        actions.push((alt_str("T"), "Hide".into()));
        actions.push(("\u{2325}\u{2190}\u{2192} \u{2325}\u{232b}".into(), "Word edit".into()));
        actions.push(("\u{21e7}\u{2190}\u{2191}\u{2193}\u{2192}".into(), "Mark".into()));
        actions.push((alt_str("\u{2191}"), "Select mode".into()));
        actions.push(("\u{21e7}PgUp/Dn".into(), "Scroll".into()));
        actions.push((copy_key.into(), "Copy".into()));
        actions.push((paste_key.into(), "Paste".into()));
        actions.push((cmd_str("Q"), "Quit".into()));
        actions
    };

    let refs: Vec<(&str, &str)> = actions.iter().map(|(k, l)| (k.as_str(), l.as_str())).collect();
    // Fixed, not derived from the current hint count (see the comment on
    // `rows` above) — 16 hints (the max, with 2+ terminals open) fits in
    // exactly the 3 rows reserved; fewer hints (a single terminal, or copy
    // mode) just leaves trailing rows blank instead of widening columns.
    const TERMINAL_HINT_COLS: usize = 6;
    f.render_widget(
        Paragraph::new(hint_table(&refs, TERMINAL_HINT_COLS, rows[2].width, &t))
            .style(Style::default().bg(t.bg_dark)),
        rows[2],
    );

    // Only the focused terminal owns the mouse; clear the body rect otherwise so
    // clicks on a dialog above don't land in the (buried) terminal.
    if !focused {
        app.terminal_view = None;
    }
}

/// Render one terminal's screen into `area` (optionally inside a titled border).
fn render_one_terminal(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    idx: usize,
    label: &str,
    t: &Theme,
    active: bool,
    bordered: bool,
) {
    let scrolled = app.terminals[idx].scroll_offset();
    let inner = if bordered {
        let color = if active { t.accent } else { t.bg_light };
        let mut title = format!(" {label} ");
        if scrolled > 0 {
            title.push_str(&format!("\u{2191}{scrolled} (Shift+End to bottom) "));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color))
            .title(TSpan::styled(
                title,
                Style::default()
                    .fg(if active { t.accent } else { t.fg_dim })
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(t.terminal_bg));
        let inner = block.inner(area);
        f.render_widget(block, area);
        inner
    } else {
        area
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Record the active terminal's body so mouse events can map to its cells.
    if active {
        app.terminal_view = Some((inner.x, inner.y, inner.width, inner.height));
    }

    let term = &mut app.terminals[idx];
    term.resize(inner.height, inner.width);
    term.pump();
    let screen = term.screen();
    let (srows, scols) = screen.size();

    // One `Span` per run of *consecutive same-styled* cells, not one per cell —
    // a terminal row is almost always long runs of identically-styled text
    // (a whole line of plain output, say), so cell-per-span was allocating and
    // laying out up to `cols` tiny one-character Strings/Spans per row, every
    // repaint. Style is `Copy + PartialEq`, so merging runs is just a compare.
    let mut lines = Vec::with_capacity(inner.height as usize);
    for row in 0..srows.min(inner.height) {
        let mut spans: Vec<TSpan> = Vec::new();
        let mut run = String::new();
        let mut run_style = Style::default();
        for col in 0..scols.min(inner.width) {
            let (contents, style) = match screen.cell(row, col) {
                Some(cell) => {
                    let raw = cell.contents();
                    let contents = if raw.is_empty() { " " } else { raw };
                    let mut style = Style::default()
                        .fg(conv_color(cell.fgcolor(), t.fg, t))
                        .bg(conv_color(cell.bgcolor(), t.terminal_bg, t));
                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell.inverse() {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                    // Paint the mouse selection over the cell.
                    if term.is_selected(row, col) {
                        style = style.bg(t.bg_light).fg(t.fg);
                    }
                    (contents, style)
                }
                None => (" ", Style::default()),
            };
            if style != run_style && !run.is_empty() {
                spans.push(TSpan::styled(std::mem::take(&mut run), run_style));
            }
            run_style = style;
            run.push_str(contents);
        }
        if !run.is_empty() {
            spans.push(TSpan::styled(run, run_style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg)),
        inner,
    );

    // Block cursor for the focused terminal (the GUI doesn't draw the native
    // one): the copy-mode cursor — a distinct amber block — when in copy mode,
    // otherwise the shell's cursor in the accent colour. The shell cursor is the
    // *live* position, so hide it once the view is scrolled back into history —
    // otherwise it floats as a stray pointer over old output as you scroll.
    let copy_cur = term.copy_cursor();
    let cursor_cell = if active {
        copy_cur.or_else(|| {
            (!screen.hide_cursor() && term.scroll_offset() == 0).then(|| screen.cursor_position())
        })
    } else {
        None
    };
    if let Some((crow, ccol)) = cursor_cell {
        let cx = inner.x + ccol.min(inner.width.saturating_sub(1));
        let cy = inner.y + crow.min(inner.height.saturating_sub(1));
        let ch = screen
            .cell(crow, ccol)
            .map(|c| {
                let s = c.contents();
                if s.is_empty() {
                    " ".to_string()
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| " ".to_string());
        let bg = if copy_cur.is_some() { t.yellow } else { t.accent };
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(ch, Style::default().bg(bg).fg(t.bg)))),
            Rect::new(cx, cy, 1, 1),
        );
    }
}

/// The terminal quick-switcher (⌘K): a small floating palette — type to
/// fuzzy-filter open terminals, ↑↓ to move, Enter to jump. Stacks on top of
/// the terminal dialog rather than replacing it, so Esc just falls back to
/// whichever terminal was showing before.
fn render_terminal_picker(f: &mut Frame, area: Rect, app: &App, below: usize) {
    let t = &app.theme;
    let labels = app.terminal_labels();

    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Switch terminal ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 0 query
            Constraint::Length(1), // 1 divider
            Constraint::Min(0),    // 2 list
        ])
        .split(inner);

    // Query line: a search icon, then the live query with a blinking cursor
    // always at the end (this picker only ever appends/backspaces).
    let icon = format!("{}{}", app.icons.search, " ".repeat(space::XS as usize));
    let icon_w = app.icons.search.chars().count() as u16 + space::XS;
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(icon, Style::default().fg(t.accent))))
            .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    let field = Rect::new(parts[0].x + icon_w, parts[0].y, parts[0].width.saturating_sub(icon_w), 1);
    let cur = app.terminal_picker.query.chars().count();
    render_line_input(f, field, &app.terminal_picker.query, cur, None, t.fg, t);
    render_divider(f, parts[1], t);

    // The list: matched terminals, best first, with matched chars bolded —
    // same highlight convention as the Files dialog's search results.
    let mut lines: Vec<Line> = Vec::new();
    if app.terminal_picker.matches.is_empty() {
        lines.push(Line::from(TSpan::styled("No matches", Style::default().fg(t.fg_dim))));
    }
    let list_h = parts[2].height as usize;
    let start = app.terminal_picker.selected.saturating_sub(list_h.saturating_sub(1));
    for (row, &ti) in app.terminal_picker.matches.iter().enumerate().skip(start).take(list_h) {
        let on_cursor = row == app.terminal_picker.selected;
        let base = if on_cursor { t.selection(true) } else { Style::default().fg(t.fg) };
        let match_style = if on_cursor { base } else { Style::default().fg(t.accent).add_modifier(Modifier::BOLD) };
        let label = labels.get(ti).map(String::as_str).unwrap_or("");
        let positions = crate::filedialog::fuzzy_positions(&app.terminal_picker.query, label);
        let mut spans = vec![TSpan::styled(" ", base)];
        spans.extend(spans_for(label, &positions, base, match_style));
        if let Some(color) = app.terminals.get(ti).and_then(|term| terminal_dot_color(term, t)) {
            spans.push(TSpan::styled(" \u{25cf}", if on_cursor { base } else { Style::default().fg(color) }));
        }
        // Pad the rest of the row so the selection highlight fills its width,
        // not just the text.
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let pad = (parts[2].width as usize).saturating_sub(used);
        if pad > 0 {
            spans.push(TSpan::styled(" ".repeat(pad), base));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines).style(Style::default().bg(t.bg_dark)), parts[2]);
}

/// Project-wide "Search in Files" (⌘⇧F): a query box, then results grouped by
/// file (bold path header, indented match lines with the hit bolded), same
/// visual language as the Files dialog's search list.
fn render_search_files(f: &mut Frame, area: Rect, app: &mut App, below: usize) {
    let t = app.theme.clone();
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Search in Files ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 0 query line
            Constraint::Length(1), // 1 divider
            Constraint::Min(1),    // 2 results
            Constraint::Length(1), // 3 divider
            Constraint::Length(1), // 4 one-line footer
        ])
        .split(inner);

    // Query line: search icon + editable text, same as the Files dialog.
    let icon = format!("{}{}", app.icons.search, " ".repeat(space::XS as usize));
    let icon_w = app.icons.search.chars().count() as u16 + space::XS;
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(icon, Style::default().fg(t.accent))))
            .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    let field = Rect::new(parts[0].x + icon_w, parts[0].y, parts[0].width.saturating_sub(icon_w), 1);
    render_line_input(f, field, &app.project_search.query, app.project_search.cursor, app.project_search.anchor, t.fg, &t);

    // Right-aligned status: a pending-search notice, the result count, or a
    // truncation notice — filling the empty right side of the query row. The
    // search auto-runs a moment after typing settles, so "pending" is the
    // normal in-between state, not an idle prompt.
    let ps = &app.project_search;
    let total = ps.total_matches();
    let status = if !ps.searched {
        if ps.pending_search_at.is_some() || ps.in_flight {
            "Searching\u{2026}".to_string()
        } else {
            String::new()
        }
    } else if ps.results.is_empty() {
        "No results".to_string()
    } else {
        let files = ps.results.len();
        let suffix = if ps.truncated { "+" } else { "" };
        format!(
            "{total}{suffix} result{} in {files} file{}",
            if total == 1 { "" } else { "s" },
            if files == 1 { "" } else { "s" }
        )
    };
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(status, Style::default().fg(t.fg_dim))))
            .alignment(Alignment::Right)
            .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );

    render_divider(f, parts[1], &t);

    // Flatten (file header + its matches) into rows so a single scroll offset
    // can page through the whole grouped list, same idea as the Files
    // dialog's `start`/`list_h` pagination.
    enum Row {
        Header(usize),
        Match(usize, usize),
    }
    let mut rows: Vec<Row> = Vec::new();
    for (fi, r) in app.project_search.results.iter().enumerate() {
        rows.push(Row::Header(fi));
        for mi in 0..r.matches.len() {
            rows.push(Row::Match(fi, mi));
        }
    }
    let list_h = parts[2].height as usize;
    let sel = app.project_search.selected_match();
    let sel_row = rows
        .iter()
        .position(|r| matches!(r, Row::Match(fi, mi) if Some((*fi, *mi)) == sel))
        .unwrap_or(0);
    let start = sel_row.saturating_sub(list_h.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::new();
    if !app.project_search.searched {
        lines.push(Line::from(TSpan::styled(
            "Type a query and press Enter to search every file in the project.",
            Style::default().fg(t.fg_dim),
        )));
    } else if app.project_search.results.is_empty() {
        lines.push(Line::from(TSpan::styled("No results", Style::default().fg(t.fg_dim))));
    } else {
        let root = app.root.clone();
        for row in rows.iter().skip(start).take(list_h) {
            match row {
                Row::Header(fi) => {
                    let r = &app.project_search.results[*fi];
                    let rel = root
                        .as_deref()
                        .and_then(|root| r.path.strip_prefix(root).ok())
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| r.path.to_string_lossy().into_owned());
                    let name = rel.rsplit('/').next().unwrap_or(&rel);
                    let (icon, icon_color) = app.icons.file(name);
                    lines.push(Line::from(vec![
                        TSpan::styled(format!("{icon} "), Style::default().fg(icon_color)),
                        TSpan::styled(rel, Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                        TSpan::styled(format!("  {}", r.matches.len()), Style::default().fg(t.fg_dim)),
                    ]));
                }
                Row::Match(fi, mi) => {
                    let m = &app.project_search.results[*fi].matches[*mi];
                    let selected = Some((*fi, *mi)) == sel;
                    let (base, match_style) = if selected {
                        (t.selection(true), t.selection(true).add_modifier(Modifier::BOLD))
                    } else {
                        (
                            Style::default().fg(t.fg_dim),
                            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                        )
                    };
                    let chars: Vec<char> = m.preview.chars().collect();
                    let pcol = m.preview_col.min(chars.len());
                    let pend = m.preview_end_col.min(chars.len()).max(pcol);
                    let prefix: String = chars[..pcol].iter().collect();
                    let matched: String = chars[pcol..pend].iter().collect();
                    let suffix: String = chars[pend..].iter().collect();
                    let mut spans = vec![TSpan::styled(format!("  {:>5}  ", m.line + 1), Style::default().fg(t.fg_dim))];
                    spans.push(TSpan::styled(prefix, base));
                    spans.push(TSpan::styled(matched, match_style));
                    spans.push(TSpan::styled(suffix, base));
                    if selected {
                        let used: usize = spans.iter().map(|s| s.width()).sum();
                        let pad = (parts[2].width as usize).saturating_sub(used);
                        if pad > 0 {
                            spans.push(TSpan::styled(" ".repeat(pad), base));
                        }
                    }
                    lines.push(Line::from(spans));
                }
            }
        }
    }
    f.render_widget(Paragraph::new(lines).style(Style::default().bg(t.bg_dark)), parts[2]);

    render_divider(f, parts[3], &t);

    let actions: Vec<(&str, &str)> = vec![
        ("\u{2191}\u{2193}", "Navigate"),
        ("\u{21b5}", "Open"),
        ("Esc", "Close"),
    ];
    let refs: Vec<(&str, &str)> = actions;
    f.render_widget(
        Paragraph::new(hint_table(&refs, refs.len(), parts[4].width, &t)).style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

/// OS-styled Control chord for an arbitrary key label, e.g. `⌃Tab` /
/// `Ctrl+Tab`. Identical in both modes — see `input::shortcut_mods` for why
/// the terminal build no longer shows (or requires) a different combo.
fn ctrl_str(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("\u{2303}{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

/// OS-styled Alt/Option chord, e.g. `⌥T` on macOS, `Alt+T` elsewhere.
fn alt_str(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("\u{2325}{key}")
    } else {
        format!("Alt+{key}")
    }
}

/// Command-key notation (⌘ on macOS, `Ctrl+` elsewhere) for the shortcuts that
/// fire on either ⌘ or Ctrl. Used for the letter/punctuation actions; tab
/// switching keeps [`ctrl_str`] since ⌘Tab is reserved by macOS.
fn cmd_str(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("\u{2318}{key}")
    } else {
        format!("Ctrl+{key}")
    }
}

fn cmd_key(letter: char) -> String {
    cmd_str(&letter.to_ascii_uppercase().to_string())
}

/// Draw a 1×1 block cursor showing `ch`. Used everywhere a text cursor is
/// needed because the GUI (wgpu) backend doesn't render the native terminal
/// cursor that `set_cursor_position` relies on.
fn draw_block_cursor(f: &mut Frame, x: u16, y: u16, ch: char, t: &Theme) {
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(
            ch.to_string(),
            Style::default().bg(t.accent).fg(t.bg),
        ))),
        Rect::new(x, y, 1, 1),
    );
}

/// Render a single-line text input into `rect` (one row): the text on a
/// `bg_dark` field, the selection highlighted, and a block cursor at `cursor`.
/// Scrolls horizontally to keep the cursor in view. `cursor`/`anchor` are char
/// indices (see [`crate::editline`]). Used by the find bar, file-dialog query,
/// and name prompt so all three edit the same way as the main buffer.
fn render_line_input(
    f: &mut Frame,
    rect: Rect,
    text: &str,
    cursor: usize,
    anchor: Option<usize>,
    fg: ratatui::style::Color,
    t: &Theme,
) {
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let cur = cursor.min(n);
    let width = rect.width as usize;
    // Left-anchored horizontal scroll: only shift once the cursor would fall off
    // the right edge, keeping it on the last visible cell.
    let start = if cur >= width { cur + 1 - width } else { 0 };
    let end = (start + width).min(n);

    let base = Style::default().fg(fg).bg(t.bg_dark);
    let sel = Style::default().fg(fg).bg(t.sel_bg);
    let slice = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };
    let spans = match crate::editline::selection(cur, anchor) {
        Some((ss, se)) if se.min(end) > ss.max(start) => {
            let ss = ss.clamp(start, end);
            let se = se.clamp(start, end);
            vec![
                TSpan::styled(slice(start, ss), base),
                TSpan::styled(slice(ss, se), sel),
                TSpan::styled(slice(se, end), base),
            ]
        }
        _ => vec![TSpan::styled(slice(start, end), base)],
    };
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg_dark)),
        rect,
    );

    // Block cursor over the cell at the (scrolled) cursor column.
    let cx = rect.x + (cur - start) as u16;
    if cx < rect.x + rect.width {
        let under = chars.get(cur).copied().unwrap_or(' ');
        draw_block_cursor(f, cx, rect.y, under, t);
    }
}

/// Spacing scale, in character cells (the terminal's atomic unit — there are no
/// sub-cell pixels in a TUI). A consistent step keeps dialogs visually regular,
/// in the spirit of a 4/8/12/16px design grid: one cell is the base unit, and
/// these are 1×/2×/3× of it.
mod space {
    /// 1 cell — gap between an icon and its label.
    pub const XS: u16 = 1;
    /// 2 cells — the standard content gutter inside a dialog.
    pub const SM: u16 = 2;
}

/// Lay `(key, label)` action hints out across `width`, "space-between" justified
/// so the first sits at the left edge, the last at the right, and the slack is
/// distributed evenly between them — filling the full width instead of bunching
/// to the left.
/// One "⟨key⟩ label" hint segment — the single spacing/style rule every
/// footer in the app renders hints with (dialog footers, the terminal pane,
/// the editor status bar), so shortcut hints line up the same way everywhere
/// instead of each footer inventing its own gap.
fn hint_span(chord: &str, label: &str, t: &Theme) -> Vec<TSpan<'static>> {
    vec![
        TSpan::styled(
            format!(" {chord} "),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        TSpan::styled(format!("{label}  "), Style::default().fg(t.fg_dim)),
    ]
}

/// A single-row table of hints spread evenly across `width`, distributing any
/// slack as inter-column padding so a handful of hints still fills the row
/// instead of clumping at the left. Unlike `hint_table` below, there's only
/// one row here — nothing else to look inconsistent against — so stretching
/// to fill the width is unambiguously fine (a *multi*-row footer wants
/// per-column sizing instead, or a row of long labels ends up looking
/// cramped next to a row of short ones; see `hint_table`'s docs).
fn hint_row(items: &[(&str, &str)], width: u16, t: &Theme) -> Line<'static> {
    if items.is_empty() {
        return Line::default();
    }
    let content_w = |k: &str, l: &str| k.chars().count() + l.chars().count() + 4;
    let col_w = (width as usize / items.len()).max(1);
    let dim = Style::default().fg(t.fg_dim);
    // If the hints don't even fit at their own natural width, stretching
    // would only push some off the edge — pack them tightly instead.
    let natural: usize = items.iter().map(|(k, l)| content_w(k, l)).sum();
    if natural > width as usize {
        let mut spans = Vec::with_capacity(items.len() * 2);
        for (k, l) in items {
            spans.extend(hint_span(k, l, t));
        }
        return Line::from(spans);
    }
    let mut spans = Vec::with_capacity(items.len() * 3);
    let mut cursor = 0usize;
    for (i, (k, l)) in items.iter().enumerate() {
        spans.extend(hint_span(k, l, t));
        cursor += content_w(k, l);
        let target = (i + 1) * col_w;
        if target > cursor {
            spans.push(TSpan::styled(" ".repeat(target - cursor), dim));
            cursor = target;
        }
    }
    Line::from(spans)
}

/// Same column-table layout as `hint_row`, but for footers that flow across
/// multiple lines: `cols` fixes the column count for every row, and each
/// column is sized to the widest hint *that lands in it* (row-major, so
/// column `i` is items `i`, `i+cols`, `i+2*cols`, …) — a real table, not one
/// shared width across the whole grid. Sharing one width made a row of long
/// labels (e.g. "Multi-cursor", "Copy path") look cramped next to a row of
/// short ones (e.g. "Undo", "Redo") even though both used the same nominal
/// column width; per-column sizing gives each column exactly the room its
/// own content needs, so every row reads consistently.
fn hint_table(items: &[(&str, &str)], cols: usize, width: u16, t: &Theme) -> Vec<Line<'static>> {
    if items.is_empty() || cols == 0 {
        return vec![Line::default()];
    }
    let content_w = |k: &str, l: &str| k.chars().count() + l.chars().count() + 4;
    let mut col_w = vec![0usize; cols];
    for (i, (k, l)) in items.iter().enumerate() {
        let c = i % cols;
        col_w[c] = col_w[c].max(content_w(k, l));
    }
    let dim = Style::default().fg(t.fg_dim);
    let natural: usize = col_w.iter().sum();
    // If the columns' own natural widths already don't fit, padding them out
    // would only push later columns off the edge — pack every row tightly
    // instead (just `hint_span`'s own built-in gap) so a narrow window
    // degrades to "cramped but all visible" rather than clipping.
    if natural > width as usize {
        return items
            .chunks(cols)
            .map(|row| {
                let mut spans = Vec::with_capacity(row.len() * 2);
                for (k, l) in row {
                    spans.extend(hint_span(k, l, t));
                }
                Line::from(spans)
            })
            .collect();
    }
    // Otherwise there's slack — spread it evenly across every column so the
    // table genuinely fills the row (rather than leaving the natural widths'
    // total as dead space on the right), while every column still keeps the
    // *same* width across every row.
    let slack = width as usize - natural;
    let (base, extra) = (slack / cols, slack % cols);
    for (i, w) in col_w.iter_mut().enumerate() {
        *w += base + usize::from(i < extra);
    }
    items
        .chunks(cols)
        .map(|row| {
            let mut spans = Vec::with_capacity(row.len() * 3);
            for (i, (k, l)) in row.iter().enumerate() {
                spans.extend(hint_span(k, l, t));
                let pad = col_w[i].saturating_sub(content_w(k, l));
                if pad > 0 {
                    spans.push(TSpan::styled(" ".repeat(pad), dim));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Draw a subtle full-width horizontal rule across the (1-row) `area` — a quiet
/// separator between sections of a dialog.
fn render_divider(f: &mut Frame, area: Rect, t: &Theme) {
    if area.width == 0 {
        return;
    }
    let rule = "\u{2500}".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(rule, Style::default().fg(t.bg_light))))
            .style(Style::default().bg(t.bg_dark)),
        area,
    );
}

/// Inset `area` by a horizontal padding so content never hugs the border.
fn pad_x(area: Rect, pad: u16) -> Rect {
    Rect {
        x: area.x + pad,
        y: area.y,
        width: area.width.saturating_sub(pad * 2),
        height: area.height,
    }
}

/// Build styled spans for `text`, applying `match_style` to the char indices in
/// `positions` (assumed sorted, ascending) and `base` to the rest. Returns a
/// single base-styled span when nothing matches.
fn spans_for(text: &str, positions: &[usize], base: Style, match_style: Style) -> Vec<TSpan<'static>> {
    if positions.is_empty() {
        return vec![TSpan::styled(text.to_string(), base)];
    }
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut buf_matched = false;
    let mut pi = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let is_match = pi < positions.len() && positions[pi] == i;
        if is_match {
            pi += 1;
        }
        if !buf.is_empty() && is_match != buf_matched {
            let style = if buf_matched { match_style } else { base };
            spans.push(TSpan::styled(std::mem::take(&mut buf), style));
        }
        buf.push(ch);
        buf_matched = is_match;
    }
    if !buf.is_empty() {
        spans.push(TSpan::styled(buf, if buf_matched { match_style } else { base }));
    }
    spans
}

/// A VSCode quick-open style result row: the **filename first** (bright), then
/// the parent **folder** (dim), with the fuzzy-matched characters highlighted in
/// both. `rel` is the path relative to the project root (no trailing slash).
fn path_result_spans(
    rel: &str,
    query: &str,
    name_base: Style,
    folder_base: Style,
    match_style: Style,
) -> Vec<TSpan<'static>> {
    // Match positions are char indices into the full relative path.
    let positions = crate::filedialog::fuzzy_positions(query, rel);
    let chars: Vec<char> = rel.chars().collect();
    // Split at the last '/': everything after is the filename, before is folder.
    let split = chars.iter().rposition(|&c| c == '/');
    let (folder, name): (String, String) = match split {
        Some(i) => (
            chars[..i].iter().collect(),
            chars[i + 1..].iter().collect(),
        ),
        None => (String::new(), rel.to_string()),
    };
    let name_start = split.map(|i| i + 1).unwrap_or(0);
    let name_pos: Vec<usize> = positions
        .iter()
        .filter(|&&p| p >= name_start)
        .map(|&p| p - name_start)
        .collect();
    let folder_pos: Vec<usize> = positions
        .iter()
        .copied()
        .filter(|&p| split.map(|i| p < i).unwrap_or(false))
        .collect();

    let mut spans = spans_for(&name, &name_pos, name_base, match_style);
    if !folder.is_empty() {
        spans.push(TSpan::styled("  ".to_string(), folder_base));
        spans.extend(spans_for(&folder, &folder_pos, folder_base, match_style));
    }
    spans
}

/// Rect for a dialog `below` levels under the focused one (0 = focused),
/// sized to `pct` percent of the screen (clamped to the 80–99 range the
/// Settings dialog offers — see `App::dialog_size_pct`) instead of the old
/// fixed 90% for every dialog. The focused dialog is centered and each one
/// below it is nudged down-right by a couple of cells so it peeks out behind
/// like a stack of cards; the offset is clamped so it never runs off screen.
fn dialog_rect_stacked(area: Rect, below: usize, pct: u32) -> Rect {
    let pct = pct.clamp(80, 99);
    let w = (area.width as u32 * pct / 100) as u16;
    let h = (area.height as u32 * pct / 100) as u16;
    let cx = area.x + (area.width.saturating_sub(w)) / 2;
    let cy = area.y + (area.height.saturating_sub(h)) / 2;
    let max_x = area.x + area.width.saturating_sub(w);
    let max_y = area.y + area.height.saturating_sub(h);
    let x = (cx + below as u16 * 2).min(max_x);
    let y = (cy + below as u16).min(max_y);
    Rect::new(x, y, w, h)
}

/// Rect for an *alert* — the rename / delete / save-before-close prompt, which
/// asks one question and shouldn't bury the list it's asking about.
///
/// Width and horizontal placement come from [`dialog_rect_stacked`], the same
/// source every dialog uses, so it lines up with them; only the height hugs
/// the content. Every browsable dialog uses `dialog_rect_stacked` directly and
/// takes the configured size in full — a short list leaves empty space, the
/// same way the Files dialog does.
fn dialog_rect_content(area: Rect, below: usize, pct: u32, content_rows: u16) -> Rect {
    let full = dialog_rect_stacked(area, below, pct);
    // At least 3 rows: a border pair plus one line of something.
    let h = content_rows.clamp(3, full.height.max(3));
    Rect::new(full.x, full.y, full.width, h.min(area.height))
}

/// The empty starting screen: a centred title and the two shortcuts.
fn render_blank(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;

    // Every chord is right-aligned into one column and every label starts in
    // the next, so the keys stack into a readable block. Prose ("Press ⌥F to
    // open files") re-indented the chord on every row, which is exactly the
    // shape the eye can't scan.
    //
    // The paragraph is centre-aligned, which centres each line independently —
    // so every hint row is padded out to the same total width. Without that,
    // rows with shorter labels would sit further right and the column would
    // shear apart exactly where it's supposed to line up.
    const KEY_W: usize = 6;
    const LABEL_W: usize = 16;
    let hint = |label: &str, chord: String| {
        let lead = KEY_W.saturating_sub(chord.chars().count());
        let trail = LABEL_W.saturating_sub(label.chars().count());
        Line::from(vec![
            TSpan::raw(" ".repeat(lead)),
            TSpan::styled(chord, Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            TSpan::styled(format!("   {label}{}", " ".repeat(trail)), Style::default().fg(t.fg_dim)),
        ])
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(TSpan::styled(
        "OXRU",
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(TSpan::styled(
        format!("v{}", env!("CARGO_PKG_VERSION")),
        Style::default().fg(crate::theme::mix(t.fg_dim, t.bg, 0.35)),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    if app.has_folder() {
        lines.push(hint("Open files", alt_str("F")));
        lines.push(hint("Search in files", cmd_str("\u{21e7}F")));
        lines.push(hint("Open a folder", cmd_str("O")));
        lines.push(hint("Recent folders", alt_str("O")));
        lines.push(hint("New terminal", alt_str("T")));
    } else {
        // No folder open — only show what actually works here. Files and the
        // terminal both need a folder, so they're hidden until one is open.
        lines.push(Line::from(TSpan::styled(
            "No folder open",
            Style::default().fg(t.fg_dim),
        )));
        lines.push(Line::from(""));
        lines.push(hint("Open a folder", cmd_str("O")));
        lines.push(hint("Recent folders", alt_str("O")));
    }
    lines.push(hint("Settings", cmd_str(",")));
    lines.push(hint("Shortcuts", "F1".to_string()));
    lines.push(hint("Quit", cmd_key('q')));

    // Vertically centred as a block rather than pinned to a fixed row, so the
    // whole screen stays balanced whatever the window height is.
    let top = (area.height as usize).saturating_sub(lines.len()) / 2;
    let mut out = vec![Line::from(""); top];
    out.extend(lines);

    f.render_widget(
        Paragraph::new(out)
            .alignment(Alignment::Center)
            .style(Style::default().bg(t.bg)),
        area,
    );
}

/// Fixed footprint of one tab cell in the wrapping grid — a leading number
/// (1-9, blank past that), a name slot, and a modified/running-dot slot, all
/// at fixed widths so a single tab's own state changing (e.g. gaining the
/// modified dot) never shifts every tab after it. No close button — tabs
/// only close via a keyboard shortcut, never the mouse. Any width left over
/// once as many `TAB_CELL_W`-wide columns fit as possible is spread evenly
/// across them, so the strip still fills the row — the same "aligned
/// columns, full width" convention `hint_table` uses for the shortcut
/// footers.
const TAB_CELL_W: usize = 24;
const TAB_NUMBER_W: usize = 2;
const TAB_DOT_W: usize = 2;
/// Leading marker slot: an accent bar on the active tab, blank on the rest.
/// Fixed-width like every other slot so switching tabs shifts nothing.
const TAB_MARK_W: usize = 1;
/// File-type icon plus the space after it.
const TAB_ICON_W: usize = 2;
/// Trailing blank margin so a tab's text doesn't hug the next one.
const TAB_MARGIN_W: usize = 3;
const TAB_NAME_BUDGET: usize =
    TAB_CELL_W - TAB_MARK_W - TAB_NUMBER_W - TAB_ICON_W - TAB_DOT_W - TAB_MARGIN_W;

/// How many columns the wrapping tab grid uses at `width` — shared by the
/// layout pass (sizing the strip's height before anything is drawn) and the
/// renderer itself, so they always agree on the row count.
fn tab_grid_cols(width: u16) -> usize {
    ((width as usize) / TAB_CELL_W).max(1)
}

/// How many rows `n` tabs need at `width` — see [`tab_grid_cols`]. Always at
/// least 1, even for an empty strip, so the caller has somewhere to draw the
/// (empty) row rather than a zero-height area.
fn tab_grid_rows(n: usize, width: u16) -> usize {
    if n == 0 { 1 } else { n.div_ceil(tab_grid_cols(width)) }
}

/// Truncate `s` to at most `max` chars, appending "…" if it didn't fit —
/// keeps every tab cell's name slot a fixed width regardless of the actual
/// filename length, so a long filename never widens its column (or any
/// other tab's).
fn truncate_tab_label(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let mut out: String = chars[..max - 1].iter().collect();
        out.push('\u{2026}');
        out
    }
}

/// Lay `labels` out as a wrapping grid instead of a horizontally-scrolling
/// line — every tab is always visible, wrapping onto more rows instead of
/// scrolling off-screen. Shared by the editor and terminal tab strips.
/// `labels` is (display name, dot color) — yellow "modified" for editors,
/// green "running" for terminals, `None` for neither. `TabHit.row` in the
/// result is the *grid* row (0-indexed), not a screen row — the caller
/// already knows the strip's own screen offset. Clicking a tab only ever
/// selects it — tabs close via a keyboard shortcut only, so there's no close
/// button to render or hover over.
fn wrapping_tab_grid(
    labels: &[(String, Option<Color>)],
    active: usize,
    width: u16,
    t: &Theme,
    icons: Option<&crate::icons::Icons>,
) -> (Vec<Line<'static>>, Vec<TabHit>) {
    if labels.is_empty() {
        return (vec![Line::default()], Vec::new());
    }
    let cols = tab_grid_cols(width);
    let col_w = ((width as usize) / cols).max(TAB_CELL_W);

    let mut lines = Vec::with_capacity(labels.len().div_ceil(cols));
    let mut hits = Vec::with_capacity(labels.len());
    for (row_idx, row) in labels.chunks(cols).enumerate() {
        let mut spans: Vec<TSpan<'static>> = Vec::with_capacity(row.len() * 4);
        let mut col = 0u16;
        for (i_in_row, (label, dot)) in row.iter().enumerate() {
            let i = row_idx * cols + i_in_row;
            let active_here = i == active;
            // `t.bg` and `t.bg_dark` are nearly identical dark grays (a ~7/255
            // difference), so using `t.bg` here made the active tab's
            // background essentially indistinguishable from an inactive
            // one. `t.sel_bg` (the same accent-tinted highlight used for text
            // selection) is a real, unmistakable color difference instead.
            let bg = if active_here { t.sel_bg } else { t.bg_dark };
            let number = if i < 9 { format!("{} ", i + 1) } else { "  ".to_string() };
            let number_style = Style::default().fg(t.fg_dim).bg(bg);
            let name_style = if active_here {
                Style::default().fg(t.accent).bg(bg).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.fg_dim).bg(bg)
            };
            let truncated = truncate_tab_label(label, TAB_NAME_BUDGET);
            let name_pad = TAB_NAME_BUDGET.saturating_sub(truncated.chars().count());

            // A solid accent bar down the active tab's leading edge. Bold
            // accent text already marked it, but weight alone is a weak signal
            // across a grid of similar cells — an unbroken vertical rule is
            // read as "this one" before any text is.
            let mark = if active_here {
                TSpan::styled("\u{258e}", Style::default().fg(t.accent).bg(bg))
            } else {
                TSpan::styled(" ", Style::default().bg(bg))
            };
            // The file-type icon carries its own colour (the same one the
            // Explorer uses) so a tab is identifiable by shape and hue, not
            // just by reading the filename. Terminal tabs pass no icon set.
            let icon = match icons {
                Some(set) => {
                    let (glyph, color) = set.file(label);
                    if glyph.is_empty() {
                        TSpan::styled("  ", Style::default().bg(bg))
                    } else {
                        TSpan::styled(format!("{glyph} "), Style::default().fg(color).bg(bg))
                    }
                }
                None => TSpan::styled("  ", Style::default().bg(bg)),
            };

            let mut cell: Vec<TSpan<'static>> = vec![
                mark,
                TSpan::styled(number, number_style),
                icon,
                TSpan::styled(format!("{truncated}{}", " ".repeat(name_pad)), name_style),
            ];
            if let Some(color) = dot {
                cell.push(TSpan::styled(" \u{25cf}", Style::default().fg(*color).bg(bg)));
            } else {
                cell.push(TSpan::styled("  ", Style::default().bg(bg)));
            }

            let used = cell.iter().map(|s| s.width()).sum::<usize>();
            let pad = col_w.saturating_sub(used);
            if pad > 0 {
                cell.push(TSpan::styled(" ".repeat(pad), Style::default().bg(bg)));
            }
            hits.push(TabHit { row: row_idx as u16, col_start: col, col_end: col + col_w as u16, tab: i });
            spans.extend(cell);
            col += col_w as u16;
        }
        lines.push(Line::from(spans));
    }
    (lines, hits)
}

/// The breadcrumb bar: the active file's root-relative folder path as
/// clickable segments, then the filename. Shown only in single-file view
/// with a real path — see [`App::breadcrumb_row_shown`].
fn render_breadcrumb(f: &mut Frame, area: Rect, app: &mut App) {
    app.breadcrumb_hits.clear();
    let t = app.theme.clone();
    f.render_widget(Block::default().style(Style::default().bg(t.bg_dark)), area);
    // Borrowed, not cloned: neither `root` nor `path` needs to outlive this
    // function, so there's no reason to allocate a fresh copy of either every
    // single frame the breadcrumb is on screen.
    let (Some(idx), Some(root)) = (app.active_editor, app.root.as_deref()) else {
        return;
    };
    let Some(path) = app.editors[idx].path.as_deref() else {
        return;
    };
    let rel = path.strip_prefix(root).unwrap_or(path);

    // One segment per folder from the root down to (but not including) the
    // filename; each is a click target that scopes the Files dialog there.
    // `acc` is the one owned `PathBuf` we actually need (as a mutable
    // accumulator); segments each keep their own snapshot of it.
    let mut acc = root.to_path_buf();
    let mut segments: Vec<(String, std::path::PathBuf)> = vec![(
        root.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string()),
        acc.clone(),
    )];
    if let Some(parent) = rel.parent() {
        for comp in parent.components() {
            acc = acc.join(comp.as_os_str());
            segments.push((comp.as_os_str().to_string_lossy().into_owned(), acc.clone()));
        }
    }
    let filename = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();

    let sep = Style::default().fg(t.fg_dim);
    let seg_style = Style::default().fg(t.fg_dim);
    let mut spans = Vec::new();
    let mut col = area.x;
    for (i, (label, folder)) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(TSpan::styled(" \u{203a} ", sep));
            col += 3;
        }
        let w = label.chars().count() as u16;
        app.breadcrumb_hits.push((col, col + w, folder.clone()));
        spans.push(TSpan::styled(label.as_str(), seg_style));
        col += w;
    }
    spans.push(TSpan::styled(" \u{203a} ", sep));
    spans.push(TSpan::styled(filename, Style::default().fg(t.fg)));
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.bg_dark)),
        area,
    );
}

/// The editor tab strip: open files, numbered 1-9 (then unlabeled) with a dot
/// on any with unsaved changes. Wraps onto more rows instead of scrolling —
/// every tab is always visible. `area`'s height must already match
/// [`tab_grid_rows`] for this tab count, since this only draws into it, not
/// resize it.
fn render_tabs(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme.clone();
    f.render_widget(Block::default().style(Style::default().bg(t.bg_dark)), area);

    let labels: Vec<(String, Option<Color>)> = app
        .editors
        .iter()
        .map(|buf| (buf.name(), buf.modified.then_some(t.yellow)))
        .collect();
    let (lines, hits) =
        wrapping_tab_grid(&labels, app.active_editor.unwrap_or(0), area.width, &t, Some(&app.icons));
    app.tab_hits = hits;
    app.tab_strip_rect = Some((area.x, area.y, area.width, area.height));
    f.render_widget(Paragraph::new(lines).style(Style::default().bg(t.bg_dark)), area);
}

/// A thin status line. While editing it shows the key shortcuts on the left and
/// the active file's save state + cursor position on the right; otherwise it
/// shows any status message.
/// This process's current RAM use, formatted for the footer (e.g. "142 MB"),
/// or `None` before the first `App::poll_memory` sample lands.
fn mem_label(app: &App) -> Option<String> {
    let kb = app.mem_rss_kb?;
    Some(format!("{:.0} MB", kb as f64 / 1024.0))
}

/// Temporary debug readout: measured composited-frames-per-second next to the
/// configured target, e.g. "fps: 1.2/1", so a mismatch is visible at a glance.
fn fps_label(app: &App) -> Option<String> {
    let measured = app.measured_fps?;
    Some(format!("fps: {measured:.1}/{}", app.gui_fps))
}

fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let dim = Style::default().fg(t.fg_dim);

    // No file open: show any status message on the left, RAM use on the right.
    // Same surface as the editing status bar below, so the window's bottom edge
    // doesn't change colour just because no file happens to be open.
    let Some(i) = app.active_editor else {
        let (_, sfg_muted) = t.status_fg();
        let bar = Style::default().bg(t.status_bg);
        let seg_muted = Style::default().fg(sfg_muted).bg(t.status_bg);
        let left = if app.status.is_empty() {
            String::new()
        } else {
            format!(" {}", app.status)
        };
        let right = [fps_label(app), mem_label(app)].into_iter().flatten().collect::<Vec<_>>().join("  ");
        let mut spans = Vec::new();
        if !left.is_empty() {
            spans.push(TSpan::styled(left.clone(), seg_muted));
        }
        if !right.is_empty() {
            let w = area.width as usize;
            let pad = w.saturating_sub(left.chars().count() + right.chars().count() + 1);
            spans.push(TSpan::styled(" ".repeat(pad), bar));
            spans.push(TSpan::styled(format!("{right} "), seg_muted));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(bar),
            area,
        );
        return;
    };

    let b = &app.editors[i];
    // Shortcut hints, most-used first. They flow across the two footer rows; any
    // that don't fit are still in the F1 cheat-sheet. Tab switching stays on Ctrl
    // (⌘Tab is reserved by macOS).
    let mut hints: Vec<(String, &str)> = vec![
        (cmd_key('s'), "Save"),
        (cmd_key('f'), "Find"),
        (cmd_key('z'), "Undo"),
        (cmd_str("\u{21e7}Z"), "Redo"),
    ];
    hints.push(("\u{21e7}\u{2190}\u{2191}\u{2193}\u{2192}".to_string(), "Select"));
    if b.selection().is_some() {
        hints.push((cmd_key('c'), "Copy"));
        hints.push((cmd_key('x'), "Cut"));
    } else {
        hints.push((cmd_key('a'), "Select all"));
    }
    hints.push((cmd_key('v'), "Paste"));
    hints.push((cmd_str("\u{21e7}S"), "Save all"));
    hints.push((cmd_key('w'), "Close"));
    hints.push((cmd_str("\u{21e7}W"), "Close all"));
    hints.push((cmd_str("\u{21e7}T"), "Reopen"));
    hints.push((cmd_str("\\"), if app.editor_grid { "Unsplit" } else { "Split" }));
    if app.editors.len() > 1 {
        hints.push((ctrl_str("Tab"), "Switch"));
        hints.push((ctrl_str("\u{21e7}\u{2194}"), "Move tab"));
    }
    // Multi-cursor: ⌘D adds the next match; while several carets are live, show
    // how to add a column caret and how to collapse back to one. Add-caret is
    // ⌘⌥↕ — already multi-modifier, so it's written literally.
    if b.has_extra_carets() {
        hints.push((cmd_str("\u{2325}\u{2195}"), "Add caret"));
        hints.push(("Esc".to_string(), "One caret"));
    } else {
        hints.push((cmd_key('d'), "Multi-cursor"));
    }
    hints.push((cmd_str("\u{21e7}C"), "Copy path"));
    hints.push((alt_str("F"), "Files"));
    hints.push((cmd_str("\u{21e7}F"), "Search files"));
    hints.push((alt_str("T"), "Term"));
    hints.push((alt_str("Z"), if app.word_wrap { "Unwrap" } else { "Wrap" }));
    hints.push((cmd_str(","), "Settings"));
    hints.push(("F1".to_string(), "Shortcuts"));
    hints.push((cmd_key('q'), "Quit"));

    // Status (left-aligned): position · language · line ending · indent ·
    // git branch (green = clean, yellow = uncommitted changes) · saved state
    // · RAM — 7 fields. Starts with a bare space so the leftmost glyph never
    // sits flush against the window's left edge.
    // The status row is drawn on `status_bg` — its own surface, not another
    // stripe of `bg_dark` — so the window has a defined bottom edge instead of
    // fading out. Foregrounds come from the theme's derived pair so they stay
    // readable whatever colour the bar ends up.
    let (sfg, sfg_muted) = t.status_fg();
    let bar = Style::default().bg(t.status_bg);
    let seg = Style::default().fg(sfg).bg(t.status_bg);
    let seg_muted = Style::default().fg(sfg_muted).bg(t.status_bg);

    let lang = b.lang.map(|l| l.name).unwrap_or("Plain Text");
    // On the bar, colour means "this needs you". The calm states use the
    // derived foregrounds instead of a palette hue: now that the bar is a
    // darkened shade of the accent, a green "saved" against a green bar (the
    // default) is green-on-green, and every other accent would need its own
    // hand-tuned exception. One rule, and it holds for any accent.
    let (state, scolor) = if b.modified {
        ("\u{25cf} unsaved", t.yellow)
    } else {
        ("\u{2713} saved", sfg_muted)
    };
    // With multiple carets, surface the count instead of a single Ln/Col.
    let pos = if b.has_extra_carets() {
        format!("{} carets", b.caret_count())
    } else {
        format!("Ln {}, Col {}", b.cursor_row() + 1, b.cursor_col() + 1)
    };
    // Separator between fields: one glyph carrying the field boundary, so the
    // gaps can be tight without the fields running together. Three spaces of
    // padding did the same job before but needed far more of the row's width.
    let divider = TSpan::styled(
        " \u{2502} ",
        Style::default().fg(crate::theme::mix(sfg_muted, t.status_bg, 0.45)).bg(t.status_bg),
    );
    let mut status: Vec<TSpan> = vec![TSpan::styled(" ", bar)];
    if let Some((branch, dirty)) = &app.git_branch {
        // Branch first: it's the field that answers "where am I?", and putting
        // it at the left edge keeps it in one place instead of sliding around
        // as the fields before it change width.
        let color = if *dirty { t.yellow } else { sfg };
        let label = if app.icons.branch.is_empty() {
            branch.clone()
        } else {
            format!("{} {branch}", app.icons.branch)
        };
        status.push(TSpan::styled(label, Style::default().fg(color).bg(t.status_bg)));
        status.push(divider.clone());
    }
    status.push(TSpan::styled(pos, seg));
    status.push(divider.clone());
    status.push(TSpan::styled(lang.to_string(), seg_muted));
    status.push(divider.clone());
    status.push(TSpan::styled(b.line_ending.label().to_string(), seg_muted));
    status.push(divider.clone());
    status.push(TSpan::styled(format!("Spaces: {}", b.indent_unit()), seg_muted));
    status.push(divider.clone());
    status.push(TSpan::styled(
        format!("{state} "),
        Style::default().fg(scolor).bg(t.status_bg),
    ));
    let status_w: usize = status.iter().map(|s| s.content.chars().count()).sum();
    let w = area.width as usize;
    // fps/RAM are pinned to the right edge rather than appended to the
    // left-aligned status text — otherwise their column shifts every time
    // something earlier in the line (branch name, save state) changes length.
    let right = [fps_label(app), mem_label(app)].into_iter().flatten().collect::<Vec<_>>().join("  ");
    let right_w = if right.is_empty() { 0 } else { right.chars().count() + 1 };

    if area.height >= 2 {
        // A clean hint table — same aligned-column layout the dialog footers
        // use — then (room permitting) a divider and the save/cursor status
        // on its own row, set off from the shortcuts since it isn't one.
        let has_divider = area.height >= 3;
        let hint_rows = (area.height as usize - if has_divider { 2 } else { 1 }).max(1);
        let refs: Vec<(&str, &str)> = hints.iter().map(|(k, l)| (k.as_str(), *l)).collect();
        // A fixed column count (rather than one derived from the hint count)
        // keeps the grid's column width stable — otherwise it visibly
        // reflows every time the hint list's length changes (e.g. selecting
        // text adds "Copy"/"Cut" in place of "Select all"), which reads as
        // the footer jittering/cutting off mid-edit. Any hints past what 7
        // columns × the reserved rows can hold are still in the F1 sheet.
        const HINT_COLS: usize = 7;
        for (row, line) in hint_table(&refs, HINT_COLS, area.width, t).into_iter().take(hint_rows).enumerate() {
            let r = Rect::new(area.x, area.y + row as u16, area.width, 1);
            f.render_widget(Paragraph::new(line).style(Style::default().bg(t.bg_dark)), r);
        }
        if has_divider {
            let div_row = Rect::new(area.x, area.y + hint_rows as u16, area.width, 1);
            render_divider(f, div_row, t);
        }
        // Left-aligned, like the hint rows above it — except fps/RAM, which
        // are right-aligned to a fixed column.
        let mut line = status;
        if !right.is_empty() {
            let pad = w.saturating_sub(status_w + right_w);
            line.push(TSpan::styled(" ".repeat(pad), bar));
            line.push(TSpan::styled(format!("{right} "), seg_muted));
        }
        let status_row = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
        f.render_widget(Paragraph::new(Line::from(line)).style(bar), status_row);
    } else {
        // Not enough room for a dedicated status row (tiny terminal): pack
        // hints left, status middle, fps/RAM pinned right, on the one line
        // we have.
        let mut line: Vec<TSpan> = Vec::new();
        let mut lw = 0;
        for (chord, label) in &hints {
            let seg = hint_span(chord, label, t);
            let sw = chord.chars().count() + label.chars().count() + 4;
            if lw + sw > w {
                break;
            }
            line.extend(seg);
            lw += sw;
        }
        line.push(TSpan::raw(" ".repeat(w.saturating_sub(lw + status_w + right_w))));
        line.extend(status);
        if !right.is_empty() {
            line.push(TSpan::styled(format!("{right} "), dim));
        }
        f.render_widget(Paragraph::new(Line::from(line)).style(Style::default().bg(t.bg_dark)), area);
    }
}

/// Rects for tiling `n` editor panes: 1 = full, 2 = left|right, 3 = left|right
/// on top + a full-width pane below, 4+ = a √n grid (like the terminal). Panes
/// fill in order, so editors[0] is left, editors[1] right, editors[2] bottom.
fn editor_grid_rects(area: Rect, n: usize) -> Vec<Rect> {
    let halves = || {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
    };
    match n {
        0 => vec![],
        1 => vec![area],
        2 => halves().split(area).to_vec(),
        3 => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
                .split(area);
            let top = halves().split(rows[0]);
            vec![top[0], top[1], rows[1]]
        }
        _ => {
            let gcols = (n as f64).sqrt().ceil() as usize;
            let grows = n.div_ceil(gcols);
            let row_rects = Layout::default()
                .direction(Direction::Vertical)
                .constraints(vec![Constraint::Ratio(1, grows as u32); grows])
                .split(area);
            let mut rects = Vec::with_capacity(n);
            for rr in row_rects.iter() {
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(vec![Constraint::Ratio(1, gcols as u32); gcols])
                    .split(*rr);
                for cr in cols.iter() {
                    if rects.len() < n {
                        rects.push(*cr);
                    }
                }
            }
            rects
        }
    }
}

/// Split off the rightmost column as a scrollbar track, if there's room to
/// spare (below that, the scrollbar would eat too much of the text).
fn split_scrollbar(rect: Rect) -> (Rect, Option<Rect>) {
    if rect.width <= 4 {
        return (rect, None);
    }
    let content = Rect::new(rect.x, rect.y, rect.width - 1, rect.height);
    let bar = Rect::new(rect.x + rect.width - 1, rect.y, 1, rect.height);
    (content, Some(bar))
}

/// Draw pane `idx`'s scrollbar into the reserved column `rect`, and record its
/// thumb (if the content overflows the viewport) into `app.scrollbar_thumbs`
/// for the mouse handlers. Left blank when everything already fits.
fn render_scrollbar_col(f: &mut Frame, rect: Rect, idx: usize, app: &mut App) {
    if rect.height == 0 {
        return;
    }
    let t = app.theme.clone();
    let (scroll_row, line_count) = {
        let b = &app.editors[idx];
        (b.scroll_row, b.line_count())
    };
    let gutter_w = (line_count.max(1).to_string().len() as u16).max(3) + 1;
    let pane_width = app.editor_panes.iter().find(|(i, _)| *i == idx).map(|(_, (_, _, w, _))| *w);
    let content_width = crate::app::wrap_content_width(app.word_wrap, pane_width, gutter_w);
    let (total_rows, rows_before_scroll) = app.editors[idx].wrap_scroll_metrics(scroll_row, content_width);
    let total = total_rows.max(1);
    let viewport = rect.height as usize;
    if total <= viewport {
        return;
    }
    let thumb_h = ((viewport * viewport) / total).max(1).min(viewport);
    let max_scroll = total - viewport;
    let track_h = viewport.saturating_sub(thumb_h).max(1);
    let thumb_off = (track_h * rows_before_scroll.min(max_scroll)) / max_scroll.max(1);
    let track = Style::default().bg(t.bg_dark);
    let thumb = Style::default().bg(t.bg_light);
    let lines: Vec<Line> = (0..viewport)
        .map(|i| {
            let style = if i >= thumb_off && i < thumb_off + thumb_h { thumb } else { track };
            Line::from(TSpan::styled(" ", style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), rect);
    app.scrollbar_thumbs.push((idx, rect.y + thumb_off as u16, thumb_h as u16));
}

/// The editor area: a single file, or — in grid view — every open file tiled.
fn render_editor(f: &mut Frame, area: Rect, app: &mut App) {
    app.editor_panes.clear();
    app.scrollbar_thumbs.clear();
    let n = app.editors.len();
    if app.editor_grid && n > 1 {
        let t = app.theme.clone();
        let rects = editor_grid_rects(area, n);
        for (idx, rect) in rects.into_iter().enumerate() {
            let active = Some(idx) == app.active_editor;
            let name = app.editors[idx].name();
            let modified = app.editors[idx].modified;
            let title = if modified { format!(" {name} \u{25cf} ") } else { format!(" {name} ") };
            let color = if active { t.accent } else { t.bg_light };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(color))
                .title(TSpan::styled(
                    title,
                    Style::default()
                        .fg(if active { t.accent } else { t.fg_dim })
                        .add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(t.bg));
            let inner = block.inner(rect);
            f.render_widget(block, rect);
            if inner.width == 0 || inner.height == 0 {
                continue;
            }
            let (content, bar) = split_scrollbar(inner);
            if active {
                app.ensure_cursor_visible(content.height, content.width);
            }
            app.editor_panes
                .push((idx, (content.x, content.y, content.width, content.height)));
            render_editor_pane(f, content, app, idx, active);
            if let Some(bar_rect) = bar {
                render_scrollbar_col(f, bar_rect, idx, app);
            }
        }
    } else if let Some(idx) = app.active_editor {
        let (content, bar) = split_scrollbar(area);
        app.ensure_cursor_visible(content.height, content.width);
        app.editor_panes
            .push((idx, (content.x, content.y, content.width, content.height)));
        render_editor_pane(f, content, app, idx, true);
        if let Some(bar_rect) = bar {
            render_scrollbar_col(f, bar_rect, idx, app);
        }
    }
    // The find bar floats over the top-right of the editor area when open.
    if app.find.active {
        render_find_bar(f, area, app);
    }
}

/// The in-file find bar: a compact box at the editor's top-right showing the
/// query, a live "N of M" count, the optional replace field (Ctrl+H), and the
/// key hints for whichever field has focus.
fn render_find_bar(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let w = 48u16.min(area.width);
    if w < 12 {
        return;
    }
    let x = area.x + area.width.saturating_sub(w);
    // Inner rows: query (+ count), optional replace field, then key hints.
    let inner_rows = if app.find.replace_active { 3 } else { 2 };
    let rect = Rect::new(x, area.y, w, inner_rows + 2);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Find ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    // Key hints on the last inner row — differ by which field has focus.
    let hints_row = inner_rows.saturating_sub(1);
    if inner.height > hints_row {
        let acc = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(t.fg_dim);
        let hints = if app.find.replace_focus {
            Line::from(vec![
                TSpan::styled("\u{21b5}", acc),
                TSpan::styled(" replace  ", dim),
                TSpan::styled("\u{21e7}\u{21b5}", acc),
                TSpan::styled(" all  ", dim),
                TSpan::styled("\u{21e5}", acc),
                TSpan::styled(" find  ", dim),
                TSpan::styled("Esc", acc),
                TSpan::styled(" close", dim),
            ])
        } else if app.find.replace_active {
            Line::from(vec![
                TSpan::styled("\u{21b5}/\u{2193}", acc),
                TSpan::styled(" next  ", dim),
                TSpan::styled("\u{21e5}", acc),
                TSpan::styled(" replace  ", dim),
                TSpan::styled("Esc", acc),
                TSpan::styled(" close", dim),
            ])
        } else {
            Line::from(vec![
                TSpan::styled("\u{21b5}/\u{2193}", acc),
                TSpan::styled(" next  ", dim),
                TSpan::styled("\u{21e7}\u{21b5}/\u{2191}", acc),
                TSpan::styled(" prev  ", dim),
                TSpan::styled("^R", acc),
                TSpan::styled(" replace  ", dim),
                TSpan::styled("Esc", acc),
                TSpan::styled(" close", dim),
            ])
        };
        f.render_widget(
            Paragraph::new(hints).style(Style::default().bg(t.bg_dark)),
            Rect::new(inner.x, inner.y + hints_row as u16, inner.width, 1),
        );
    }

    // Right side: match count (or "No results"), on the query row.
    let count = if app.find.query.is_empty() {
        String::new()
    } else if app.find.matches.is_empty() {
        "No results".to_string()
    } else {
        format!("{} of {}", app.find.current + 1, app.find.matches.len())
    };
    let count_w = count.chars().count() as u16;

    // Query row: a full single-line input (cursor + selection), leaving room
    // on the right for the count.
    let input_w = if count_w > 0 {
        inner.width.saturating_sub(count_w + 1)
    } else {
        inner.width
    };
    let q_color = if app.find.matches.is_empty() && !app.find.query.is_empty() {
        t.red
    } else {
        t.fg
    };
    render_line_input(
        f,
        Rect::new(inner.x, inner.y, input_w, 1),
        &app.find.query,
        app.find.cursor,
        app.find.anchor,
        q_color,
        t,
    );
    // Count, right-aligned on the same row.
    if count_w > 0 && count_w <= inner.width {
        let count_color = if app.find.matches.is_empty() { t.red } else { t.fg_dim };
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(count, Style::default().fg(count_color))))
                .alignment(ratatui::layout::Alignment::Right),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
    }

    // Replace row, when showing: a labeled input, its focus state indicated by
    // the label color matching whichever field is currently being edited.
    if app.find.replace_active && inner.height > 1 {
        let label = "Replace: ";
        let label_w = label.chars().count() as u16;
        let label_color = if app.find.replace_focus { t.accent } else { t.fg_dim };
        let row = Rect::new(inner.x, inner.y + 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(
                label,
                Style::default().fg(label_color),
            )))
            .style(Style::default().bg(t.bg_dark)),
            row,
        );
        let field = Rect::new(
            row.x + label_w,
            row.y,
            row.width.saturating_sub(label_w),
            1,
        );
        render_line_input(
            f,
            field,
            &app.find.replace,
            app.find.replace_cursor,
            app.find.replace_anchor,
            t.fg,
            t,
        );
    }
}

/// Render one editor (`idx`) into `area`: line numbers, highlighted text, the
/// selection, and — when `active` — the block cursor.
fn render_editor_pane(f: &mut Frame, area: Rect, app: &mut App, idx: usize, active: bool) {
    let t = app.theme.clone();
    let (scroll_row, cursor_row, cursor_col, line_count) = {
        let b = &app.editors[idx];
        (b.scroll_row, b.cursor_row(), b.cursor_col(), b.line_count())
    };
    let gutter_w = (line_count.max(1).to_string().len() as u16).max(3) + 1;
    // Caret blink phase for this frame (shared by the primary and all secondary
    // carets so they pulse in sync).
    let blink_on = active && app.cursor_blink_on();
    let height = area.height as usize;
    // With word wrap off this is effectively infinite, so every rope line
    // produces exactly one row below — identical to the pre-wrap behavior,
    // through the same code path rather than a separate branch to keep in
    // sync.
    let content_width = if app.word_wrap {
        (area.width.saturating_sub(gutter_w) as usize).max(1)
    } else {
        usize::MAX
    };

    let row_map: Vec<crate::buffer::WrapRow> = app.editors[idx].wrap_rows(scroll_row, content_width, height);
    let hl = app.highlighted_for(idx);

    let mut lines = Vec::with_capacity(height);
    for row in &row_map {
        let num_style = if row.line == cursor_row {
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.fg_dim)
        };
        // The gutter shows the line number only on a wrapped line's first
        // (leftmost) row; a wrap-continuation row's gutter stays blank, like
        // VSCode (with wrap off every row is a first row, so this never
        // differs from before).
        let num_text = if row.start == 0 {
            format!("{:>width$} ", row.line + 1, width = (gutter_w - 1) as usize)
        } else {
            " ".repeat(gutter_w as usize)
        };
        let mut spans = vec![TSpan::styled(num_text, num_style)];
        if let Some(line_spans) = hl.get(row.line) {
            spans.extend(slice_spans(line_spans, row.start, row.end));
        } else if row.start == 0 {
            spans.push(TSpan::styled("~", Style::default().fg(t.bg_light)));
        }
        lines.push(Line::from(spans));
    }
    while lines.len() < height {
        lines.push(Line::from(vec![
            TSpan::raw(" ".repeat(gutter_w as usize)),
            TSpan::styled("~", Style::default().fg(t.bg_light)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg)),
        area,
    );

    // Indent guides, then the current-line tint, both painted straight into the
    // buffer *after* the text: reaching into the spans instead would mean
    // splitting syntax-highlighted runs apart to insert a glyph, and the two
    // effects have to layer under the selection/find highlights that follow.
    if app.indent_guides {
        paint_indent_guides(f.buffer_mut(), area, gutter_w, &row_map, &app.editors[idx], &t);
    }
    if app.current_line_hl && active {
        paint_current_line(f.buffer_mut(), area, &row_map, cursor_row, t.line_hl);
    }

    // Paint the selection background over the affected cells — the primary
    // selection plus every secondary caret's selection (multi-cursor).
    {
        let b = &app.editors[idx];
        let buf = f.buffer_mut();
        if let Some((s, e)) = b.selection() {
            paint_range(buf, area, gutter_w, &row_map, b, s, e, t.sel_bg, true);
        }
        for caret in b.extra_carets() {
            let (s, e) = caret.bounds();
            if e > s {
                paint_range(buf, area, gutter_w, &row_map, b, s, e, t.sel_bg, true);
            }
        }
    }

    // Highlight every in-file find match (the current one brighter), painted
    // over the selection so all hits are visible while searching.
    if active && app.find.active && !app.find.matches.is_empty() {
        let b = &app.editors[idx];
        let cur = app.find.current;
        let buf = f.buffer_mut();
        let len = b.rope.len_chars();
        for (mi, &(ms, me)) in app.find.matches.iter().enumerate() {
            // `find.matches` is a cache of char offsets from whenever it was
            // last (re)computed — normally kept in sync with edits, but this
            // guards against ever rendering one that's drifted out of range
            // for the buffer's *current* content (ropey panics on an
            // out-of-bounds char index, which would otherwise crash the
            // whole app on the very next frame).
            if me <= ms || ms > len || me > len {
                continue;
            }
            let color = if mi == cur { t.find_current } else { t.find_match };
            paint_range(buf, area, gutter_w, &row_map, b, ms, me, color, false);
        }
    }

    // The editor caret. In the terminal we position the host's own *hardware*
    // cursor (set to a thin `SteadyBar` in `setup_terminal`) and blink it
    // ourselves by only calling `set_cursor_position` on the "on" phase — leaving
    // it uncalled hides the cursor (see `Frame::set_cursor_position`). We can't
    // rely on the terminal's own hardware blink: it resets to solid on every
    // redraw's cursor move, so a caret that's genuinely stationary (not being
    // typed at) would never appear to blink.
    //
    // The GUI's wgpu backend renders no native cursor at all, and hand-drawing
    // one as a `▏` character necessarily replaces whatever glyph was in that
    // cell (ratatui cells hold exactly one symbol) — hiding the character next
    // to the caret, which is exactly the problem this avoids. Instead we just
    // record the caret's *cell position* in `app.gui_carets`; the GUI layer
    // reads it after this frame and draws it as a GPU overlay painted on top of
    // the already-rendered text, so the character is never touched.
    let cursor_screen_pos = if cursor_row >= scroll_row {
        app.editors[idx].wrap_position(scroll_row, cursor_row, cursor_col, content_width)
    } else {
        None
    };
    if active {
        if let Some((row_offset, col_in_row)) = cursor_screen_pos {
            let cx = area.x + gutter_w + col_in_row as u16;
            let cy = area.y + row_offset as u16;
            // A dialog, terminal modal, or prompt draws on top of the editor later
            // in this same frame, but the GPU overlay is a separate pass painted
            // over the *entire* finished frame — it doesn't know about that
            // z-order. Without this guard the editor's stale caret floats over
            // whatever's on top of it (e.g. a terminal dialog's text).
            let editor_is_topmost = app.dialogs.is_empty() && !app.prompt.active;
            if cx < area.x + area.width && cy < area.y + area.height {
                if app.gui {
                    if blink_on && editor_is_topmost {
                        app.gui_carets.push(crate::app::GuiCaret {
                            col: cx,
                            row: cy,
                            color: t.accent,
                        });
                    }
                } else if blink_on && editor_is_topmost {
                    // Claim the host's hardware cursor only when the editor is the
                    // focused surface — a dialog, terminal, or prompt on top draws its
                    // own caret, and a stray bar must not bleed through it.
                    f.set_cursor_position((cx, cy));
                }
            }
        }
    }

    // Secondary carets (multi-cursor): in GUI mode these get the same overlay
    // treatment as the primary. In the terminal there's only one hardware
    // cursor to claim, so they fall back to a hand-drawn bar that (like the old
    // GUI approach) does overwrite the cell's glyph — an accepted limitation
    // since multiple simultaneous carets are already a less common case.
    if blink_on && app.dialogs.is_empty() && !app.prompt.active {
        let b = &app.editors[idx];
        let carets: Vec<(usize, usize)> = b
            .extra_carets()
            .iter()
            .map(|c| {
                let row = b.rope.char_to_line(c.cursor);
                (row, c.cursor - b.rope.line_to_char(row))
            })
            .collect();
        for (row, col) in carets {
            if row < scroll_row {
                continue;
            }
            let Some((row_offset, col_in_row)) = app.editors[idx].wrap_position(scroll_row, row, col, content_width)
            else {
                continue;
            };
            if row_offset >= height {
                continue;
            }
            let cx = area.x + gutter_w + col_in_row as u16;
            let cy = area.y + row_offset as u16;
            if cx < area.x + area.width && cy < area.y + area.height {
                if app.gui {
                    app.gui_carets.push(crate::app::GuiCaret {
                        col: cx,
                        row: cy,
                        color: t.fg_dim,
                    });
                } else {
                    f.render_widget(
                        Paragraph::new(Line::from(TSpan::styled(
                            "\u{258f}",
                            Style::default().fg(t.fg_dim),
                        ))),
                        Rect::new(cx, cy, 1, 1),
                    );
                }
            }
        }
    }
}

/// Extract the portion of a rope line's highlighted spans covering the char
/// range `[start, end)` (relative to that line's own start), preserving each
/// run's style — lets a wrapped sub-row render its slice of an already
/// syntax-highlighted line without re-highlighting anything.
fn slice_spans(spans: &[crate::syntax::Span], start: usize, end: usize) -> Vec<TSpan<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for (text, style) in spans {
        let span_start = pos;
        let span_end = pos + text.chars().count();
        pos = span_end;
        let from = start.max(span_start);
        let to = end.min(span_end);
        if from >= to {
            continue;
        }
        let piece: String = text.chars().skip(from - span_start).take(to - from).collect();
        if !piece.is_empty() {
            out.push(TSpan::styled(piece, *style));
        }
    }
    out
}

/// Paint a char range's background over the cells it covers in the visible
/// viewport, walking `row_map` so it's correct whether or not word-wrap has
/// split a line into multiple screen rows. Shared by selection painting
/// (`extend_into_newline` = true: a line's trailing newline shows one extra
/// selected cell, like most editors) and find-match painting (false: a
/// match never includes the newline).
#[allow(clippy::too_many_arguments)]
/// Tint every cell of the caret's row, gutter included, across the pane's full
/// width — the line has to read as one continuous band, so it can't stop at the
/// end of the text. Under word wrap a long line occupies several screen rows and
/// all of them are part of "the current line", so each is tinted.
///
/// Runs before the selection and find-match passes, so those paint over the
/// tint rather than being flattened by it — the caret's row inside a selection
/// still reads as selected.
fn paint_current_line(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    row_map: &[crate::buffer::WrapRow],
    cursor_row: usize,
    tint: Color,
) {
    for (i, row) in row_map.iter().enumerate() {
        if row.line != cursor_row {
            continue;
        }
        let y = area.y + i as u16;
        if y >= area.y + area.height {
            break;
        }
        for x in area.x..area.x + area.width {
            buf[(x, y)].set_bg(tint);
        }
    }
}

/// Draw a thin vertical rule at every indent level, VSCode-style: one at column
/// 0 of each indent step, up to (but not including) the level the line's own
/// text starts at.
///
/// A guide only ever replaces a *blank* cell, so it can never hide a character —
/// which also means the rule naturally stops where code begins. Blank lines
/// inherit the deeper of their neighbours' indentation so guides run unbroken
/// through the gaps between blocks instead of dashing on and off.
fn paint_indent_guides(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    gutter_w: u16,
    row_map: &[crate::buffer::WrapRow],
    b: &crate::buffer::Buffer,
    t: &Theme,
) {
    let unit = b.indent_unit().max(1);
    let style = Style::default().fg(t.indent_guide);

    for (i, row) in row_map.iter().enumerate() {
        // A wrap-continuation row is a slice out of the middle of a line; its
        // "indentation" is just where the wrap happened to fall, so it gets no
        // guides of its own.
        if row.start != 0 {
            continue;
        }
        let y = area.y + i as u16;
        if y >= area.y + area.height {
            break;
        }

        let (width, has_text) = b.indent_width(row.line);
        let depth = if has_text {
            width
        } else {
            // Blank line: bridge the gap using the deeper of the nearest
            // non-blank line above and below, so a guide doesn't break across
            // the blank line separating two statements in the same block.
            let above = (0..row.line)
                .rev()
                .map(|l| b.indent_width(l))
                .find(|(_, text)| *text)
                .map(|(w, _)| w)
                .unwrap_or(0);
            let below = (row.line + 1..b.rope.len_lines())
                .map(|l| b.indent_width(l))
                .find(|(_, text)| *text)
                .map(|(w, _)| w)
                .unwrap_or(0);
            above.min(below)
        };

        // Starts at column 0 — the first indent level is the one nearly every
        // line has, so skipping it (on the theory that a rule next to the
        // gutter reads as a border) means the overwhelming majority of code
        // shows no guides at all. Every editor draws this one.
        let mut col = 0;
        while col < depth {
            let x = area.x + gutter_w + col as u16;
            if x >= area.x + area.width {
                break;
            }
            let cell = &mut buf[(x, y)];
            if cell.symbol() == " " {
                cell.set_symbol("\u{2502}");
                cell.set_style(style);
            }
            col += unit;
        }
    }
}

fn paint_range(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    gutter_w: u16,
    row_map: &[crate::buffer::WrapRow],
    b: &crate::buffer::Buffer,
    range_s: usize,
    range_e: usize,
    color: Color,
    extend_into_newline: bool,
) {
    for (i, row) in row_map.iter().enumerate() {
        let line_start = b.rope.line_to_char(row.line);
        let row_char_start = line_start + row.start;
        let mut row_char_end = line_start + row.end;
        if extend_into_newline && row.end == b.line_len_chars(row.line) {
            row_char_end = (row_char_end + 1).min(b.rope.len_chars());
        }
        let from = range_s.max(row_char_start);
        let to = range_e.min(row_char_end);
        if from >= to {
            continue;
        }
        let y = area.y + i as u16;
        for col in (from - row_char_start)..(to - row_char_start) {
            let x = area.x + gutter_w + col as u16;
            if x < area.x + area.width && y < area.y + area.height {
                buf[(x, y)].set_bg(color);
            }
        }
    }
}

/// The global to-do list (⌥D). A view over the markdown file, not a separate
/// store: what's drawn here is what's in `todos.md`, and ⌃E opens that same
/// file in a real editor tab.
fn render_todos(f: &mut Frame, area: Rect, app: &App, below: usize) {
    let t = &app.theme;
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    let (done, total) = crate::todo::counts(&app.todos);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            format!(" To-do  {done}/{total} "),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 0 add-a-task input
            Constraint::Length(1), // 1 divider
            Constraint::Min(1),    // 2 list
            Constraint::Length(1), // 3 divider
            Constraint::Length(1), // 4 footer
        ])
        .split(inner);

    // The input line, with a placeholder while it's empty so the dialog says
    // what to do rather than showing a bare cursor.
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(
            "+ ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    let field = Rect::new(parts[0].x + 2, parts[0].y, parts[0].width.saturating_sub(2), 1);
    render_line_input(
        f,
        field,
        &app.todo_input,
        app.todo_input_cursor,
        app.todo_input_anchor,
        t.fg,
        t,
    );
    if app.todo_input.is_empty() {
        let hint = Rect::new(field.x + 1, field.y, field.width.saturating_sub(1), 1);
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(
                "type to add a task \u{00b7} \u{2318}V pastes a list",
                Style::default().fg(crate::theme::mix(t.fg_dim, t.bg_dark, 0.35)),
            )))
            .style(Style::default().bg(t.bg_dark)),
            hint,
        );
    }
    render_divider(f, parts[1], t);

    // The list. Headings render as headings; a done task is dimmed so the eye
    // skips it the way a crossed-out line would.
    let rows = crate::todo::visible_rows(&app.todos);
    let list_h = parts[2].height as usize;
    let cursor_row = rows.iter().position(|&i| i == app.todo_cursor).unwrap_or(0);
    let start = cursor_row.saturating_sub(list_h.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(TSpan::styled(
            "Nothing yet — type above to add your first task.",
            Style::default().fg(t.fg_dim),
        )));
    }
    for &idx in rows.iter().skip(start).take(list_h) {
        match &app.todos[idx] {
            crate::todo::Entry::Task { done, text } => {
                let on_cursor = idx == app.todo_cursor;
                let base = if on_cursor {
                    t.selection(true)
                } else if *done {
                    Style::default().fg(t.fg_dim)
                } else {
                    Style::default().fg(t.fg)
                };
                let box_style = if on_cursor {
                    base
                } else if *done {
                    Style::default().fg(t.accent)
                } else {
                    Style::default().fg(t.fg_dim)
                };
                let mut spans = vec![
                    TSpan::styled(" ", base),
                    TSpan::styled(if *done { "[✓] " } else { "[ ] " }, box_style),
                    TSpan::styled(text.clone(), base),
                ];
                // Pad the row so the selection highlight fills its width.
                let used: usize = spans.iter().map(|s| s.width()).sum();
                let pad = (parts[2].width as usize).saturating_sub(used);
                spans.push(TSpan::styled(" ".repeat(pad), base));
                lines.push(Line::from(spans));
            }
            crate::todo::Entry::Raw(text) => lines.push(Line::from(TSpan::styled(
                text.clone(),
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            ))),
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        parts[2],
    );
    render_divider(f, parts[3], t);

    let foot: Vec<(&str, &str)> = vec![
        ("↵", "Add"),
        ("Space", "Toggle"),
        ("↑↓", "Move"),
        ("⌘E", "Edit"),
        ("⌘D", "Delete"),
        ("⌘⇧D", "Clear"),
        ("Esc", "Close"),
    ];
    f.render_widget(
        Paragraph::new(hint_row(&foot, parts[4].width, t)).style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

/// Clipboard history (⌥V) — this session's copies, newest first.
fn render_clipboard(f: &mut Frame, area: Rect, app: &App, below: usize) {
    let t = &app.theme;
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Clipboard ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    let mut lines: Vec<Line> = Vec::new();
    if app.clip_ring.is_empty() {
        lines.push(Line::from(TSpan::styled(
            "Nothing copied yet in this session.",
            Style::default().fg(t.fg_dim),
        )));
    }
    let list_h = parts[0].height as usize;
    let start = app.clip_cursor.saturating_sub(list_h.saturating_sub(1));
    for (row, entry) in app.clip_ring.iter().enumerate().skip(start).take(list_h) {
        let on_cursor = row == app.clip_cursor;
        let base = if on_cursor { t.selection(true) } else { Style::default().fg(t.fg) };
        let mut spans =
            vec![TSpan::styled(" ", base), TSpan::styled(entry.label.clone(), base)];
        // Multi-line copies say so — the preview only shows the first line.
        if entry.lines > 1 {
            spans.push(TSpan::styled(
                format!("  +{} lines", entry.lines - 1),
                if on_cursor { base } else { Style::default().fg(t.fg_dim) },
            ));
        }
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let pad = (parts[0].width as usize).saturating_sub(used);
        spans.push(TSpan::styled(" ".repeat(pad), base));
        lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    render_divider(f, parts[1], t);

    let foot: Vec<(&str, &str)> =
        vec![("↵", "Paste"), ("↑↓", "Move"), ("⌘D", "Clear all"), ("Esc", "Close")];
    f.render_widget(
        Paragraph::new(hint_row(&foot, parts[2].width, t)).style(Style::default().bg(t.bg_dark)),
        parts[2],
    );
}

/// The Explorer file-operation prompt (new file / folder, rename, delete). A
/// compact box near the top so the list it acts on stays visible underneath,
/// rather than a full-screen overlay.
fn render_prompt(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    // Height: borders + an input row (when typing) + a footer row. The prompt
    // is an alert rather than a browsable dialog — a full-height box for
    // "Delete file?" would bury the list it's asking about — but its width
    // comes from the same helper so it lines up with everything else.
    let h = if app.prompt.needs_input() { 4 } else { 3 };
    let rect = dialog_rect_content(area, 0, app.dialog_size_pct, h);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            format!(" {} ", app.prompt.title()),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    if app.prompt.needs_input() {
        let top = Rect::new(inner.x, inner.y, inner.width, 1);
        // " › " prompt marker, then the editable name field.
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(" \u{203a} ", Style::default().fg(t.accent))))
                .style(Style::default().bg(t.bg_dark)),
            top,
        );
        let field = Rect::new(inner.x + 3, inner.y, inner.width.saturating_sub(3), 1);
        render_line_input(f, field, &app.prompt.input, app.prompt.cursor, app.prompt.anchor, t.fg, t);
    }

    // A footer hint, so the dialog doesn't read as an empty box.
    if inner.height >= 1 {
        let foot: Vec<(&str, &str)> = match app.prompt.kind() {
            Some(PromptKind::CloseUnsaved) => {
                vec![("Y", "save"), ("N", "discard"), ("Esc", "cancel")]
            }
            Some(PromptKind::CloseTab) => {
                vec![("Y", "save"), ("N", "don't save"), ("Esc", "cancel")]
            }
            Some(PromptKind::QuitUnsaved) => {
                vec![
                    ("Y", "save"),
                    ("N", "don't save"),
                    ("A", "save all"),
                    ("D", "discard all"),
                    ("Esc", "cancel"),
                ]
            }
            Some(PromptKind::QuitTerminal) => {
                vec![("Y", "close"), ("A", "close all"), ("Esc", "cancel")]
            }
            Some(PromptKind::OpenUrl) => vec![("\u{21b5}", "open"), ("Esc", "cancel")],
            _ => vec![("Enter", "confirm"), ("Esc", "cancel")],
        };
        let foot_rect = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(hint_row(&foot, foot_rect.width, t)).style(Style::default().bg(t.bg_dark)),
            foot_rect,
        );
    }
}

/// The "Recent folders" dialog: a multi-select list; Enter opens each checked
/// folder (or the one under the cursor) in its own window.
fn render_recent(f: &mut Frame, area: Rect, app: &App, below: usize) {
    let t = &app.theme;
    const HEADER: &str = "Pick folders to open — Space to check, Enter opens each in its own window";
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Recent folders ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 0 header
            Constraint::Length(1), // 1 divider
            Constraint::Min(1),    // 2 list
            Constraint::Length(1), // 3 divider
            Constraint::Length(1), // 4 footer
        ])
        .split(inner);

    // Header line.
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(
            HEADER,
            Style::default().fg(t.fg_dim),
        )))
        .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    render_divider(f, parts[1], t);

    // The list.
    let list_h = parts[2].height as usize;
    let mut lines: Vec<Line> = Vec::new();
    if app.recent_folders.is_empty() {
        lines.push(Line::from(TSpan::styled(
            "No recent folders yet.",
            Style::default().fg(t.fg_dim),
        )));
    }
    // Scroll so the cursor stays visible.
    let start = app.recent_cursor.saturating_sub(list_h.saturating_sub(1));
    for (i, folder) in app.recent_folders.iter().enumerate().skip(start).take(list_h) {
        let on_cursor = i == app.recent_cursor;
        let checked = app.recent_checked.get(i).copied().unwrap_or(false);
        let disabled = app.recent_disabled.get(i).copied().unwrap_or(false);
        let row_style = if on_cursor {
            t.selection(true)
        } else {
            Style::default().fg(t.fg)
        };
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| folder.to_string_lossy().into_owned());
        let parent = folder
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        if disabled {
            // Already open in a window — dimmed, no checkbox, labelled.
            let dim = if on_cursor {
                row_style
            } else {
                Style::default().fg(t.fg_dim)
            };
            lines.push(Line::from(vec![
                TSpan::styled(" \u{2713} ", dim), // ✓ marker
                TSpan::styled(name, dim),
                TSpan::styled("  (already open)", dim),
                TSpan::styled(format!("   {parent}"), Style::default().fg(t.bg_light)),
            ]));
            continue;
        }

        let check = if checked { "\u{2611}" } else { "\u{2610}" }; // ☑ / ☐
        lines.push(Line::from(vec![
            TSpan::styled(
                format!(" {check} "),
                if checked && !on_cursor {
                    Style::default().fg(t.accent)
                } else {
                    row_style
                },
            ),
            TSpan::styled(
                name,
                if on_cursor {
                    row_style
                } else {
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
                },
            ),
            TSpan::styled(format!("   {parent}"), Style::default().fg(t.fg_dim)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        parts[2],
    );
    render_divider(f, parts[3], t);

    // Footer hints, justified across the full width.
    let checked_count = app.recent_checked.iter().filter(|c| **c).count();
    let open_label = if checked_count > 0 {
        format!("Open {checked_count}")
    } else {
        "Open".to_string()
    };
    let foot: Vec<(&str, &str)> = vec![
        ("\u{2191}\u{2193}", "Move"),
        ("Space", "Check"),
        ("\u{21b5}", open_label.as_str()),
        ("\u{232b}", "Remove"),
        ("\u{2318}O", "Open other\u{2026}"),
        ("Esc", "Close"),
    ];
    f.render_widget(
        Paragraph::new(hint_row(&foot, parts[4].width, t)).style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

/// The Settings dialog: live font size and a theme-colour picker.
fn render_settings(f: &mut Frame, area: Rect, app: &App, below: usize) {
    use crate::theme::ACCENT_PALETTE;
    let t = &app.theme;
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Settings ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let font_focused = app.settings_focus == 0;
    let fps_focused = app.settings_focus == 1;
    let dialog_size_focused = app.settings_focus == 2;
    let word_wrap_focused = app.settings_focus == 3;
    let color_focused = app.settings_focus == 4;

    // A section label with a focus marker.
    let label = |text: &str, focused: bool| {
        let marker = if focused { "\u{25b8} " } else { "  " };
        Line::from(vec![
            TSpan::styled(
                marker,
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            ),
            TSpan::styled(
                text.to_string(),
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
            ),
        ])
    };

    let mut lines: Vec<Line> = vec![Line::from("")];

    // Font size control.
    lines.push(label("Font size", font_focused));
    lines.push(Line::from(vec![
        TSpan::raw("    "),
        TSpan::styled("-", Style::default().fg(t.fg_dim)),
        TSpan::styled(
            format!("  {}  ", app.gui_font_size),
            Style::default()
                .fg(if font_focused { t.accent } else { t.fg })
                .add_modifier(Modifier::BOLD),
        ),
        TSpan::styled("+", Style::default().fg(t.fg_dim)),
        TSpan::styled("     \u{2190}/\u{2192} to resize", Style::default().fg(t.fg_dim)),
    ]));
    lines.push(Line::from(""));

    // Terminal repaint rate (windowed mode) — only throttles unattended
    // terminal output; typing and other real interaction always stay instant.
    lines.push(label("Terminal FPS", fps_focused));
    let fps_opts = crate::config::FPS_OPTIONS;
    let mut fps_spans: Vec<TSpan> = vec![TSpan::raw("    ")];
    for (i, &f) in fps_opts.iter().enumerate() {
        if i > 0 {
            fps_spans.push(TSpan::raw("  "));
        }
        let picked = f == app.gui_fps;
        let style = if picked {
            Style::default()
                .fg(if fps_focused { t.accent } else { t.fg })
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(t.fg_dim)
        };
        fps_spans.push(TSpan::styled(f.to_string(), style));
    }
    lines.push(Line::from(fps_spans));
    lines.push(Line::from(vec![
        TSpan::raw("    "),
        TSpan::styled(
            "\u{2190}/\u{2192} to change (unattended terminal output only — typing is always instant)",
            Style::default().fg(t.fg_dim),
        ),
    ]));
    lines.push(Line::from(""));

    // Dialog/terminal-modal size, as a percent of the screen.
    lines.push(label("Dialog size", dialog_size_focused));
    lines.push(Line::from(vec![
        TSpan::raw("    "),
        TSpan::styled("-", Style::default().fg(t.fg_dim)),
        TSpan::styled(
            format!("  {}%  ", app.dialog_size_pct),
            Style::default()
                .fg(if dialog_size_focused { t.accent } else { t.fg })
                .add_modifier(Modifier::BOLD),
        ),
        TSpan::styled("+", Style::default().fg(t.fg_dim)),
        TSpan::styled(
            format!(
                "     \u{2190}/\u{2192} to resize ({}-{}%)",
                crate::config::DIALOG_SIZE_RANGE.start(),
                crate::config::DIALOG_SIZE_RANGE.end()
            ),
            Style::default().fg(t.fg_dim),
        ),
    ]));
    lines.push(Line::from(""));

    // Word wrap: on/off, VSCode-style (long lines soft-wrap to the pane
    // width instead of scrolling off-screen horizontally).
    lines.push(label("Word wrap", word_wrap_focused));
    lines.push(Line::from(vec![
        TSpan::raw("    "),
        TSpan::styled(
            if app.word_wrap { "On" } else { "Off" },
            Style::default()
                .fg(if word_wrap_focused { t.accent } else { t.fg })
                .add_modifier(Modifier::BOLD),
        ),
        TSpan::styled("     \u{2190}/\u{2192} to toggle", Style::default().fg(t.fg_dim)),
    ]));
    lines.push(Line::from(""));

    // Theme-colour picker.
    lines.push(label("Theme Color", color_focused));
    let mut swatches: Vec<TSpan> = vec![TSpan::raw("    ")];
    for (i, (_, (r, g, b))) in ACCENT_PALETTE.iter().enumerate() {
        let color = Color::Rgb(*r, *g, *b);
        if i == app.settings_color {
            let check = if luminance(*r, *g, *b) > 140 {
                Color::Black
            } else {
                Color::White
            };
            swatches.push(TSpan::styled(
                " \u{2713}  ",
                Style::default().bg(color).fg(check).add_modifier(Modifier::BOLD),
            ));
        } else {
            swatches.push(TSpan::styled("    ", Style::default().bg(color)));
        }
        swatches.push(TSpan::raw(" "));
    }
    lines.push(Line::from(swatches));
    lines.push(Line::from(vec![
        TSpan::raw("    "),
        TSpan::styled(
            ACCENT_PALETTE[app.settings_color].0.to_string(),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
    ]));

    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        inner,
    );

    // Footer hints, justified across the full width.
    if inner.height >= 1 {
        let foot: Vec<(&str, &str)> = vec![
            ("\u{2191}\u{2193}", "Section"),
            ("\u{2190}\u{2192}", "Change"),
            ("Esc", "Close"),
            ("\u{2318}Q", "Quit"),
        ];
        if inner.height >= 2 {
            let div_rect = Rect::new(inner.x, inner.y + inner.height - 2, inner.width, 1);
            render_divider(f, div_rect, t);
        }
        let foot_rect = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(hint_row(&foot, foot_rect.width, t)).style(Style::default().bg(t.bg_dark)),
            foot_rect,
        );
    }
}

/// Perceptual brightness (0–255) for picking a contrasting check mark.
fn luminance(r: u8, g: u8, b: u8) -> u16 {
    // u32 intermediate: 255*299 already overflows u16.
    ((r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000) as u16
}

/// The file dialog: a flat, fuzzy-searchable file list (VSCode quick-open
/// style) — every file shown up front, filtered as you type — plus the
/// Control-only actions in a footer.
/// Build the browsable file-tree rows shown in the Files dialog when the query
/// is empty: indented by depth, a chevron on folders, file/folder icons, and the
/// highlighted row styled like the search list.
fn dialog_tree_lines(app: &App, t: &Theme, list_h: usize, start: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let entries = &app.tree.entries;
    let sel = app.tree.selected;
    if entries.is_empty() {
        lines.push(Line::from(TSpan::styled(
            "Empty folder",
            Style::default().fg(t.fg_dim),
        )));
        return lines;
    }
    for (i, e) in entries.iter().enumerate().skip(start).take(list_h) {
        let selected = i == sel;
        let hovered = !selected && app.dialog_hover == Some(i);
        // Faded rows: binary files (images, PDFs, … — can't open as text) and
        // gitignored entries (.env, dist/, … — shown but greyed, like VSCode).
        let binary = !e.is_dir && crate::icons::is_binary(&e.name);
        let faded = binary || e.ignored;
        let dim = if selected { t.selection(true) } else { Style::default().fg(t.fg_dim) };
        let indent = "  ".repeat(e.depth);
        let chevron = if e.is_dir {
            if e.expanded { "\u{25be} " } else { "\u{25b8} " }
        } else {
            "  "
        };
        let (icon, icon_color) = if e.is_dir {
            (app.icons.folder_closed, t.accent)
        } else {
            app.icons.file(&e.name)
        };
        // Disabled rows: grey when idle, the selection tint dimmed when current.
        let disabled = |sel: bool| {
            if sel {
                t.selection(true).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(t.fg_dim)
            }
        };
        let name_style = if faded {
            disabled(selected)
        } else if selected {
            t.selection(true)
        } else if hovered {
            Style::default().fg(t.fg).add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(t.fg)
        };
        let icon_style = if faded {
            disabled(selected)
        } else if selected {
            t.selection(true)
        } else {
            Style::default().fg(icon_color)
        };
        let icon_disp = if icon.is_empty() { String::new() } else { format!("{icon} ") };
        let mut spans = vec![
            TSpan::styled(indent, Style::default().fg(t.fg_dim)),
            TSpan::styled(chevron.to_string(), dim),
            TSpan::styled(icon_disp, icon_style),
            TSpan::styled(e.name.clone(), name_style),
        ];
        if e.is_dir {
            spans.push(TSpan::styled("/".to_string(), dim));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn render_file_dialog(f: &mut Frame, area: Rect, app: &mut App, below: usize) {
    let t = app.theme.clone();
    let query_empty = app.file_dialog.query.is_empty();
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);

    // When the search is scoped to a folder, surface it in the title as a
    // breadcrumb ("Files › src/widgets/") so it's clear results are limited.
    let scope_rel: Option<String> = app.dialog_scope.as_ref().and_then(|s| {
        app.root.as_ref().map(|r| {
            let rel = s.strip_prefix(r).unwrap_or(s).to_string_lossy().into_owned();
            if rel.is_empty() { "/".to_string() } else { format!("{rel}/") }
        })
    });
    let title = match &scope_rel {
        Some(rel) => format!(" Files \u{203a} {rel} "),
        None => " Files ".to_string(),
    };

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            title,
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    // Inset the content by a consistent gutter so nothing hugs the border.
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    // search · rule · results · rule · footer, each section separated.
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // 0 search line
            Constraint::Length(1), // 1 divider
            Constraint::Min(1),    // 2 results
            Constraint::Length(1), // 3 divider
            Constraint::Length(3), // 4 three-line action footer
        ])
        .split(inner);

    let list_h = parts[2].height as usize;
    // The row currently highlighted, in whichever list is showing — used below
    // to scroll the window, and stashed for the mouse handlers so a click/hover
    // can resolve a screen row back to a tree/match index without recomputing
    // this scroll math (mirrors `editor_panes`/`gui_carets`).
    let sel_idx = if query_empty { app.tree.selected } else { app.file_dialog.selected };
    let start = sel_idx.saturating_sub(list_h.saturating_sub(1));
    app.dialog_list_rect = Some((parts[2].x, parts[2].y, parts[2].width, parts[2].height));
    app.dialog_list_start = start;

    // Search line: a leading icon, then either the editable query (cursor +
    // selection) or a dim placeholder when empty. One cell (XS) after the icon.
    let icon = format!("{}{}", app.icons.search, " ".repeat(space::XS as usize));
    let icon_w = app.icons.search.chars().count() as u16 + space::XS;
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(icon, Style::default().fg(t.accent))))
            .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    let field = Rect::new(
        parts[0].x + icon_w,
        parts[0].y,
        parts[0].width.saturating_sub(icon_w),
        1,
    );
    if query_empty {
        // No block cursor here: the dialog is always focused when open (there's
        // nothing else to focus), so the placeholder text alone already says
        // "type here" — a solid accent-colored cursor block sitting right next
        // to the search icon (also accent-colored) reads as one blob rather
        // than a distinct character, which made the placeholder's first
        // letter effectively unreadable even though it was still being drawn.
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(
                "Type to search, or browse below\u{2026}",
                Style::default().fg(t.fg_dim),
            )))
            .style(Style::default().bg(t.bg_dark)),
            field,
        );
    } else {
        render_line_input(
            f,
            field,
            &app.file_dialog.query,
            app.file_dialog.cursor,
            app.file_dialog.anchor,
            t.fg,
            &t,
        );
    }

    // Right-aligned match count on the search row, filling the empty right side.
    let count = app.file_dialog.matches.len();
    let ticked = app.file_dialog.checked.len();
    let count_label = if ticked > 0 {
        format!("{ticked} selected")
    } else if query_empty {
        format!("{count} file{}", if count == 1 { "" } else { "s" })
    } else {
        format!("{count} result{}", if count == 1 { "" } else { "s" })
    };
    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(count_label, Style::default().fg(t.fg_dim))))
            .alignment(Alignment::Right)
            .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );

    // Divider under the search box.
    render_divider(f, parts[1], &t);

    // Results: the browse tree (empty query) or the flat, fuzzy-ranked file list.
    let mut lines = if query_empty {
        dialog_tree_lines(app, &t, list_h, start)
    } else {
        Vec::new()
    };
    if !query_empty {
        if app.file_dialog.matches.is_empty() {
            lines.push(Line::from(TSpan::styled(
                if query_empty { "No files" } else { "No matches" },
                Style::default().fg(t.fg_dim),
            )));
        }
        for (vis, &src) in app
            .file_dialog
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(list_h)
        {
            let selected = vis == app.file_dialog.selected;
            let hovered = !selected && app.dialog_hover == Some(vis);
            let (display, is_dir) = match app.dialog_entries.get(src) {
                Some((_, d)) => (app.dialog_display[src].as_str(), *d),
                None => ("", false),
            };
            // The display string carries a trailing '/' for folders; strip it so
            // the filename/folder split is clean.
            let rel = display.strip_suffix('/').unwrap_or(display);
            let name = rel.rsplit('/').next().unwrap_or(rel);
            let (icon, icon_color) = if is_dir {
                (app.icons.folder_closed, t.accent)
            } else {
                app.icons.file(name)
            };
            // Faded results: binary files (can't open as text) and gitignored
            // files (.env, dist/, … — findable but greyed, like the explorer).
            let binary = (!is_dir && crate::icons::is_binary(name))
                || app.dialog_ignored.get(src).copied().unwrap_or(false);
            let disabled = |sel: bool| {
                if sel {
                    t.selection(true).add_modifier(Modifier::DIM)
                } else {
                    Style::default().fg(t.fg_dim)
                }
            };
            let istyle = if binary {
                disabled(selected)
            } else if selected {
                t.selection(true)
            } else {
                Style::default().fg(icon_color)
            };
            let icon_disp = if icon.is_empty() {
                " ".to_string()
            } else {
                format!("{icon} ")
            };
            // VSCode-style: bright filename + dim parent folder, with the matched
            // characters bold+accent in both. On the selected row (accent bg) the
            // whole row inverts; matches just go bold so they stay legible. Binary
            // files render greyed throughout (no match highlight).
            let (name_base, folder_base, match_style) = if binary {
                let d = disabled(selected);
                (d, d, d)
            } else if selected {
                let rs = t.selection(true);
                (rs, rs, rs.add_modifier(Modifier::BOLD))
            } else {
                let name_fg = if hovered {
                    Style::default().fg(t.fg).add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default().fg(t.fg)
                };
                (
                    name_fg,
                    Style::default().fg(t.fg_dim),
                    Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                )
            };
            let mut spans = Vec::new();
            // A tick column once multi-select is in play.
            if !app.file_dialog.checked.is_empty() {
                let ticked = app.file_dialog.checked.contains(&src);
                let mark = if ticked { "\u{2713} " } else { "  " };
                let mstyle = if selected {
                    t.selection(true)
                } else {
                    Style::default().fg(t.accent)
                };
                spans.push(TSpan::styled(mark.to_string(), mstyle));
            }
            spans.push(TSpan::styled(icon_disp, istyle));
            spans.extend(path_result_spans(
                rel,
                &app.file_dialog.query,
                name_base,
                folder_base,
                match_style,
            ));
            lines.push(Line::from(spans));
        }
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg_dark)),
        parts[2],
    );

    // Divider above the action footer.
    render_divider(f, parts[3], &t);

    // Three-line action footer, laid out as an aligned table across the full
    // width (see `hint_table`). The shown actions differ between browsing the
    // tree and searching.
    let open_label = if ticked > 0 { format!("Open {ticked}") } else { "Open".to_string() };
    // ⌥H flips whether search includes node_modules / build dirs; label the
    // action by what pressing it will do.
    let junk_label = if app.dialog_show_junk { "Hide deps" } else { "Show deps" };
    let actions: Vec<(&str, &str)> = if query_empty {
        let mut a = vec![
            ("\u{2191}\u{2193}", "Move"),
            ("\u{21b5}", "Open"),
            ("\u{2192}", "Expand"),
            ("\u{2190}", "Collapse"),
            ("Tab", "In Folder"),
            ("\u{2318}N", "New File"),
            ("\u{21e7}\u{2325}N", "New Folder"),
            ("\u{2318}R", "Rename"),
            ("\u{2318}D", "Delete"),
            ("\u{2318}\u{21e7}C", "Copy Path"),
            ("\u{2318}\u{21e7}R", "Rel Path"),
            ("\u{2325}R", "Reveal in Finder"),
            ("Esc", "Close"),
            ("\u{2318}Q", "Quit"),
        ];
        // Offer a way back out once the search has been scoped into a folder
        // (Shift+Tab mirrors the Tab that drilled in; Backspace works too).
        if scope_rel.is_some() {
            a.insert(4, ("\u{21e7}Tab", "Out"));
        }
        a
    } else {
        vec![
            ("\u{2191}\u{2193}", "Move"),
            ("\u{21b5}", open_label.as_str()),
            ("Tab", "Select"),
            ("\u{2318}N", "New File"),
            ("\u{21e7}\u{2325}N", "New Folder"),
            ("\u{2318}R", "Rename"),
            ("\u{2318}D", "Delete"),
            ("\u{2318}\u{21e7}C", "Copy Path"),
            ("\u{2318}\u{21e7}R", "Rel Path"),
            ("\u{2325}R", "Reveal in Finder"),
            ("\u{2325}H", junk_label),
            ("Esc", "Close"),
            ("\u{2318}Q", "Quit"),
        ]
    };
    let cols = actions.len().div_ceil(3);
    f.render_widget(
        Paragraph::new(hint_table(&actions, cols, parts[4].width, &t)).style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

/// Every keyboard shortcut, grouped by where it applies — the data behind the
/// F1 cheat-sheet. `⌘` entries also fire on Ctrl (noted in the overlay). When no
/// folder is open the folder-requiring sections are dropped, so only shortcuts
/// that actually work are shown.
///
/// Entries are built through [`cmd_str`]/[`ctrl_str`]/[`alt_str`] rather than
/// hardcoded, so the displayed combo is always the one that actually fires on
/// this OS. The embedded terminal's own chords (word-jump, copy mode, shell
/// pass-through) are written literally: they mirror a real terminal app rather
/// than being Oxru shortcuts.
fn shortcut_groups(has_folder: bool) -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let c = |k: &str| cmd_str(k); // ⌘/Ctrl-fold shortcuts
    let a = |k: &str| alt_str(k); // Alt/Option-only shortcuts

    // Global section: hide Files / Terminal entries when there's no folder.
    let mut global: Vec<(String, &'static str)> =
        vec![(c("O"), "Open folder\u{2026}"), (a("O"), "Recent folders")];
    if has_folder {
        global.insert(0, (a("F"), "Open files"));
        global.push((c("\u{21e7}F"), "Search in files"));
        global.push((a("T"), "Toggle terminal"));
    }
    global.push((a("Z"), "Toggle word wrap"));
    global.push((a("D"), "To-do list"));
    global.push((a("V"), "Clipboard history"));
    global.push((c(","), "Settings"));
    global.push((c("Q"), "Quit"));
    global.push(("F1".to_string(), "This shortcuts list"));

    let mut groups: Vec<(&'static str, Vec<(String, &'static str)>)> = vec![("Global", global)];

    // The editor, file dialog, terminal and find bar all need an open folder.
    if has_folder {
        groups.extend(folder_shortcut_groups());
    }
    groups.push((
        "Settings",
        vec![
            ("\u{2191} \u{2193}  Tab".to_string(), "Switch section"),
            ("\u{2190} \u{2192}  + \u{2212}".to_string(), "Adjust value"),
            ("Esc / \u{21b5}".to_string(), "Close"),
        ],
    ));
    groups.push((
        "Recent folders",
        vec![
            ("\u{2191} \u{2193}".to_string(), "Navigate"),
            ("Space".to_string(), "Toggle selection"),
            ("\u{21b5}".to_string(), "Open"),
        ],
    ));
    groups
}

/// The shortcut sections that only make sense once a folder is open.
fn folder_shortcut_groups() -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let c = |k: &str| cmd_str(k);
    let ct = |k: &str| ctrl_str(k);
    let a = |k: &str| alt_str(k);
    let pair = |a: String, b: String| format!("{a}  {b}");

    vec![
        (
            "Editor \u{2014} files & tabs",
            vec![
                (pair(c("S"), c("\u{21e7}S")), "Save \u{b7} Save all"),
                (pair(c("W"), c("\u{21e7}W")), "Close tab \u{b7} Close all"),
                (c("\u{21e7}T"), "Reopen closed tab"),
                (c("F"), "Find in file"),
                (c("\\"), "Split / unsplit view"),
                (pair(ct("Tab"), ct("\u{21e7}Tab")), "Next \u{b7} previous tab"),
                (pair(ct("<"), ct(">")), "Move tab left \u{b7} right"),
                (format!("{}  {}  {}", c("C"), c("X"), c("V")), "Copy \u{b7} cut \u{b7} paste"),
                (pair(c("Z"), c("\u{21e7}Z")), "Undo \u{b7} redo"),
                (c("A"), "Select all"),
                (pair(c("\u{21e7}C"), c("\u{21e7}R")), "Copy path \u{b7} relative path"),
            ],
        ),
        (
            "Editor \u{2014} cursor & selection",
            vec![
                ("\u{2190} \u{2192} \u{2191} \u{2193}".to_string(), "Move the cursor"),
                ("\u{21e7} + any move below".to_string(), "Extend the selection (mark text)"),
                ("Home  End".to_string(), "Line start \u{b7} end"),
                (pair(c("\u{2190}"), c("\u{2192}")), "Line start \u{b7} end"),
                (pair(a("\u{2190}"), a("\u{2192}")), "Word left \u{b7} right"),
                (pair(c("\u{2191}"), c("\u{2193}")), "Document start \u{b7} end"),
                (c("A"), "Select all"),
                (pair(a("\u{232b}"), c("\u{232b}")), "Delete word \u{b7} to line start"),
                ("Tab  \u{21e7}Tab".to_string(), "Indent \u{b7} outdent selection"),
                (c("D"), "Add caret at next match (multi-cursor)"),
                // Already ⌘⌥ (Ctrl+Alt) — written literally.
                (
                    pair(cmd_str("\u{2325}\u{2191}"), cmd_str("\u{2325}\u{2193}")),
                    "Add caret above \u{b7} below",
                ),
                ("Esc".to_string(), "Collapse to one caret"),
            ],
        ),
        (
            "Files dialog",
            vec![
                ("\u{2191} \u{2193}".to_string(), "Navigate"),
                ("\u{2192}  \u{2190}".to_string(), "Expand \u{b7} collapse folder"),
                ("\u{21b5}".to_string(), "Open file / fold folder"),
                ("Tab  \u{21e7}Tab".to_string(), "Search into \u{b7} out of folder"),
                (c("N"), "New file"),
                // Already ⇧⌥ (Alt+Shift) — written literally.
                (alt_str("\u{21e7}N"), "New folder"),
                (c("R"), "Rename"),
                (c("D"), "Delete"),
                (pair(c("\u{21e7}C"), c("\u{21e7}R")), "Copy path \u{b7} relative path"),
                (a("R"), "Reveal in Finder"),
                (a("H"), "Search: show / hide node_modules, build\u{2026}"),
                ("\u{232b}".to_string(), "Delete query / out of folder"),
                ("type\u{2026}".to_string(), "Search files"),
            ],
        ),
        (
            "Search in Files",
            vec![
                (c("\u{21e7}F"), "Open (from anywhere)"),
                ("type\u{2026}".to_string(), "Edit query \u{2014} searches automatically"),
                ("\u{2191} \u{2193}".to_string(), "Navigate results"),
                ("\u{21b5}".to_string(), "Open selected result"),
                ("Esc".to_string(), "Close"),
            ],
        ),
        (
            // The embedded terminal's own key handling is exempt from the TUI
            // own chords (see the doc comment above) — every entry below is
            // written literally.
            "Terminal",
            vec![
                (
                    pair(ctrl_str("Tab"), ctrl_str("\u{21e7}Tab")),
                    "Next \u{b7} previous terminal",
                ),
                (alt_str("N"), "New terminal"),
                (alt_str("W"), "Close terminal"),
                (alt_str("G"), "Grid layout (click a tile to switch)"),
                (alt_str("K"), "Quick-switch terminal (type to filter)"),
                (alt_str("1-9"), "Jump to terminal N"),
                (ctrl_str("\u{21e7}\u{2190} / \u{2192}"), "Move terminal left \u{b7} right"),
                (
                    pair(alt_str("\u{2190}\u{2192}"), ctrl_str("\u{2190}\u{2192}")),
                    "Shell cursor by word",
                ),
                (pair(cmd_str("\u{2190}"), cmd_str("\u{2192}")), "Cursor to line start \u{b7} end"),
                (
                    pair(alt_str("\u{232b}"), cmd_str("\u{232b}")),
                    "Delete word \u{b7} to line start",
                ),
                (
                    pair(alt_str("\u{2191}"), alt_str("\u{2193}")),
                    "Copy mode (free cursor + select)",
                ),
                (pair(cmd_str("C"), cmd_str("V")), "Copy selection \u{b7} paste"),
                ("\u{21e7}PgUp/Dn  fn\u{2191}/\u{2193}".to_string(), "Scroll history"),
                ("\u{21e7}\u{2190} \u{2191} \u{2192} \u{2193}".to_string(), "Mark text (\u{21e7}\u{2325} by word)"),
                ("Drag \u{b7} \u{21e7}Click".to_string(), "Select w/ mouse (\u{21e7}Click extends)"),
                (
                    pair(alt_str("\u{21b5}"), "\u{21e7}\u{21b5}".to_string()),
                    "Soft newline (don't submit)",
                ),
            ],
        ),
        (
            // Also terminal-pane-exempt — entering copy mode has no global
            // equivalent, only the terminal's own Option+Up.
            "Terminal copy mode",
            vec![
                ("\u{2190}\u{2191}\u{2193}\u{2192} / hjkl".to_string(), "Move cursor"),
                ("\u{21e7} + arrows".to_string(), "Mark / extend selection"),
                (pair(alt_str("\u{2190}"), alt_str("\u{2192}")), "Move by word"),
                (format!("\u{21b5} / y / {}", cmd_str("C")), "Copy & exit"),
                ("Esc / q".to_string(), "Exit copy mode"),
            ],
        ),
        (
            "To-do list",
            vec![
                ("\u{21b5}".to_string(), "Add the typed task"),
                ("Space".to_string(), "Toggle the highlighted task"),
                (c("E"), "Edit the raw markdown file in a tab"),
                (c("D"), "Delete the highlighted task"),
                (c("\u{21e7}D"), "Clear every completed task"),
            ],
        ),
        (
            "Clipboard history",
            vec![
                ("\u{21b5}".to_string(), "Paste where you were"),
                (c("D"), "Clear the history"),
            ],
        ),
        (
            "Find bar",
            vec![
                ("\u{21b5}  \u{2193}".to_string(), "Next match"),
                ("\u{21e7}\u{21b5}  \u{2191}".to_string(), "Previous match"),
                ("Esc".to_string(), "Close find"),
            ],
        ),
    ]
}

/// The F1 cheat-sheet: every shortcut, grouped by where it works, in a scrollable
/// panel with the key column aligned so rows are spaced cleanly across the width.
fn render_help(f: &mut Frame, area: Rect, app: &mut App, below: usize) {
    let t = app.theme.clone();
    // Align the description column to the widest key combo (clamped) so every row
    // is spaced out evenly across the panel.
    let groups = shortcut_groups(app.has_folder());
    let key_col = groups
        .iter()
        .flat_map(|(_, items)| items.iter())
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(10)
        .min(22);
    let target = key_col + 4; // two leading spaces + key + a gap
    let rect = dialog_rect_stacked(area, below, app.dialog_size_pct);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.accent))
        .title(TSpan::styled(
            " Keyboard Shortcuts ",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(t.bg_dark));
    let inner = pad_x(block.inner(rect), space::SM);
    f.render_widget(block, rect);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // ⌘/Ctrl note
            Constraint::Length(1), // divider
            Constraint::Min(1),    // scrollable shortcut list
            Constraint::Length(1), // divider
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(TSpan::styled(
            "\u{2318} (Command) works anywhere Ctrl does \u{b7} same keys in the terminal and the window.",
            Style::default().fg(t.fg_dim),
        )))
        .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    render_divider(f, parts[1], &t);

    let mut all: Vec<Line> = Vec::new();
    for (gi, (title, items)) in groups.iter().enumerate() {
        if gi > 0 {
            all.push(Line::from(""));
        }
        all.push(Line::from(TSpan::styled(
            title.to_string(),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        )));
        for (keys, desc) in items {
            let mut k = format!("  {keys}");
            let pad = target.saturating_sub(k.chars().count());
            k.push_str(&" ".repeat(pad));
            all.push(Line::from(vec![
                TSpan::styled(k, Style::default().fg(t.fg).add_modifier(Modifier::BOLD)),
                TSpan::styled(desc.to_string(), Style::default().fg(t.fg_dim)),
            ]));
        }
    }

    let view_h = parts[2].height as usize;
    let max_scroll = all.len().saturating_sub(view_h);
    if app.help_scroll > max_scroll {
        app.help_scroll = max_scroll;
    }
    let visible: Vec<Line> = all.into_iter().skip(app.help_scroll).take(view_h).collect();
    f.render_widget(
        Paragraph::new(visible).style(Style::default().bg(t.bg_dark)),
        parts[2],
    );

    render_divider(f, parts[3], &t);
    let foot: Vec<(&str, &str)> = vec![
        ("\u{2191}\u{2193}", "Scroll"),
        ("PgUp/Dn", "Page"),
        ("Esc", "Close"),
        ("\u{2318}Q", "Quit"),
    ];
    f.render_widget(
        Paragraph::new(hint_row(&foot, parts[4].width, &t))
            .style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tab_hit_at;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    #[test]
    fn wrapping_tab_grid_wraps_numbers_and_hit_tests_by_row() {
        let t = Theme::default();
        // Wide enough for exactly 3 columns of TAB_CELL_W (24) each.
        let width = (TAB_CELL_W * 3) as u16;
        let labels: Vec<(String, Option<Color>)> =
            (1..=7).map(|n| (format!("file{n}.rs"), None)).collect();
        let (lines, hits) = wrapping_tab_grid(&labels, 0, width, &t, None);

        assert_eq!(lines.len(), 3, "7 tabs over 3 columns wraps to 3 rows (3,3,1)");

        // Numbering: 1-7 shown (all under 9), each tab's own number.
        for (i, line) in lines.iter().enumerate().take(2) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            for col in 0..3 {
                let n = i * 3 + col + 1;
                let icon_gap = " ".repeat(TAB_ICON_W);
                assert!(
                    text.contains(&format!("{n} {icon_gap}file{n}.rs")),
                    "tab {n} is numbered"
                );
            }
        }

        // Row-aware hit-testing: the same column on row 0 and row 1 resolves
        // to different tabs (row 0's tab 1, row 1's tab 4) — proves `TabHit`
        // actually distinguishes rows, not just columns.
        assert_eq!(tab_hit_at(&hits, 0, 2), Some(0), "row 0 col 2 hits tab 0");
        assert_eq!(tab_hit_at(&hits, 1, 2), Some(3), "row 1 col 2 hits tab 3");
    }

    #[test]
    fn wrapping_tab_grid_has_no_close_hit_region() {
        // Tabs close only via a keyboard shortcut now — every column in a
        // tab's cell (not just the "name" part) resolves to a plain Select
        // of that same tab; there's no separate close sub-region to hit.
        let t = Theme::default();
        let width = (TAB_CELL_W * 3) as u16;
        let labels: Vec<(String, Option<Color>)> = vec![
            ("a.rs".to_string(), None),
            ("b.rs".to_string(), Some(t.yellow)),
            ("c.rs".to_string(), None),
        ];
        let (_, hits) = wrapping_tab_grid(&labels, 0, width, &t, None);

        let tab1_hits: Vec<&TabHit> = hits.iter().filter(|h| h.tab == 1).collect();
        assert_eq!(tab1_hits.len(), 1, "tab 1 has exactly one (Select-only) hit region");
        let hit = tab1_hits[0];
        for col in hit.col_start..hit.col_end {
            assert_eq!(tab_hit_at(&hits, 0, col), Some(1), "every column in tab 1's cell selects tab 1");
        }
    }

    #[test]
    fn wrapping_tab_grid_stops_numbering_after_nine() {
        let t = Theme::default();
        let width = (TAB_CELL_W * 5) as u16;
        let labels: Vec<(String, Option<Color>)> =
            (1..=11).map(|n| (format!("f{n}"), None)).collect();
        let (lines, _) = wrapping_tab_grid(&labels, 0, width, &t, None);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        // Derived from the slot widths rather than hardcoded spaces: with no
        // icon set the icon slot is blank, so the number and name are
        // separated by it.
        let icon_gap = " ".repeat(TAB_ICON_W);
        assert!(text.contains(&format!("9 {icon_gap}f9")), "tab 9 is still numbered");
        assert!(!text.contains("10 f10"), "tab 10 is not numbered");
        assert!(!text.contains("11 f11"), "tab 11 is not numbered");
        assert!(text.contains("f10") && text.contains("f11"), "the tabs themselves still show");
    }

    #[test]
    fn wrapping_tab_grid_marks_the_active_tab_with_bold_name_and_a_distinct_background() {
        let t = Theme::default();
        let width = (TAB_CELL_W * 3) as u16;
        let labels: Vec<(String, Option<Color>)> = vec![
            ("a.rs".to_string(), Some(t.yellow)),
            ("b.rs".to_string(), None),
        ];
        let (lines, _) = wrapping_tab_grid(&labels, 0, width, &t, None);
        let line = &lines[0];

        // Located by content rather than by span index: the cell is built from
        // several fixed-width slots (marker, number, icon, name, dot, padding)
        // and hardcoding a position means every future slot silently retargets
        // these assertions at the wrong span.
        let name_span = |label: &str| {
            line.spans
                .iter()
                .find(|s| s.content.starts_with(label))
                .unwrap_or_else(|| panic!("no span for {label}"))
        };

        // Tab 0 (active): bold name, and a background that's genuinely
        // different from an inactive tab's — not `t.bg`, which (in the
        // default theme) is only 7/255 off from `t.bg_dark` and reads as
        // "no visible difference at all".
        let name0 = name_span("a.rs");
        assert!(name0.style.add_modifier.contains(Modifier::BOLD), "active tab's name is bold");
        assert_eq!(name0.style.bg, Some(t.sel_bg), "active tab uses the same distinct highlight as text selection");

        // Tab 1 (inactive): plain background, not bold.
        let name1 = name_span("b.rs");
        assert!(!name1.style.add_modifier.contains(Modifier::BOLD), "inactive tab's name isn't bold");
        assert_eq!(name1.style.bg, Some(t.bg_dark));
        assert_ne!(t.sel_bg, t.bg_dark, "the two backgrounds are actually distinguishable colors");
    }

    #[test]
    fn truncate_tab_label_keeps_short_names_and_ellipsizes_long_ones() {
        assert_eq!(truncate_tab_label("short.rs", 20), "short.rs");
        let long = truncate_tab_label("a_very_long_filename_indeed.rs", 10);
        assert_eq!(long.chars().count(), 10);
        assert!(long.ends_with('\u{2026}'));
    }

    #[test]
    fn hint_row_fills_the_full_width_when_it_divides_evenly() {
        let t = Theme::default();
        let items = [("\u{21b5}", "Open"), ("^N", "New"), ("Esc", "Close")];
        let line = hint_row(&items, 60, &t);
        let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(w, 60, "the row fills the full width");
        assert!(line.spans.first().unwrap().content.contains('\u{21b5}'));
    }

    #[test]
    fn hint_table_sizes_each_column_to_its_own_widest_hint() {
        let t = Theme::default();
        // content widths (chord + label + 4): Move=10, Open=9, Select=13,
        // Close=12, Quit=10. Width == the natural total, so there's no slack
        // to stretch with — isolates the per-column-sizing behavior itself.
        let items = [
            ("\u{2191}\u{2193}", "Move"), // col 0, row 0
            ("\u{21b5}", "Open"),         // col 1, row 0
            ("Tab", "Select"),            // col 2, row 0 (only row with col 2)
            ("Esc", "Close"),             // col 0, row 1 — wider than "Move"
            ("\u{2318}Q", "Quit"),        // col 1, row 1
        ];
        let lines = hint_table(&items, 3, 35, &t);
        assert_eq!(lines.len(), 2, "5 items over 3 columns wraps to 2 rows");
        let row_w = |l: &Line| l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
        // Column 0 is sized to "Close" (12), the wider of "Move"/"Close", so
        // it's the same width in both rows even though "Move" itself is
        // narrower — that's what makes column 0 actually line up.
        let (col0, col1, col2) = (12, 10, 13);
        assert_eq!(row_w(&lines[0]), col0 + col1 + col2);
        assert_eq!(row_w(&lines[1]), col0 + col1);
    }

    #[test]
    fn hint_table_stretches_columns_to_fill_leftover_width() {
        let t = Theme::default();
        let items = [
            ("\u{2191}\u{2193}", "Move"),
            ("\u{21b5}", "Open"),
            ("Tab", "Select"),
            ("Esc", "Close"),
            ("\u{2318}Q", "Quit"),
        ];
        // Natural total is 35 (see the test above); asking for 90 leaves 55
        // of slack to distribute across the 3 columns.
        let lines = hint_table(&items, 3, 90, &t);
        let row_w = |l: &Line| l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
        assert_eq!(row_w(&lines[0]), 90, "row 0 (all 3 columns) fills the width");
        // Row 1 only has 2 of the 3 columns, so it's short by column 2's
        // (stretched) width — but columns 0 and 1 are the exact same width
        // as they are in row 0, so the two rows still align.
        let col2_w = 90 - row_w(&lines[1]);
        assert!(col2_w > 13, "column 2 grew past its natural width of 13");
    }

    /// Row-major screen text, one `String` per row (unlike `screen_text`,
    /// which concatenates the whole grid) — for tests that care which row a
    /// piece of text landed on, like word-wrap.
    fn screen_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buf = terminal.backend().buffer();
        let width = buf.area.width as usize;
        buf.content()
            .chunks(width.max(1))
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect()
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        dir
    }

    /// Find the screen row showing `needle`, and return that row's cells.
    fn row_with<'a>(terminal: &'a Terminal<TestBackend>, needle: &str) -> Vec<ratatui::buffer::Cell> {
        let buf = terminal.backend().buffer();
        let w = buf.area.width;
        for y in 0..buf.area.height {
            let text: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            if text.contains(needle) {
                return (0..w).map(|x| buf[(x, y)].clone()).collect();
            }
        }
        panic!("no row containing {needle:?}");
    }

    /// Measure the dialog frame actually drawn on screen: `(width, height)` of
    /// the rounded border box.
    fn dialog_frame(app: &mut App) -> (usize, usize) {
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut left = usize::MAX;
        let mut right = 0usize;
        let mut top = usize::MAX;
        let mut bottom = 0usize;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if matches!(buf[(x, y)].symbol(), "╭" | "╮" | "╰" | "╯") {
                    left = left.min(x as usize);
                    right = right.max(x as usize);
                    top = top.min(y as usize);
                    bottom = bottom.max(y as usize);
                }
            }
        }
        if left == usize::MAX {
            return (0, 0);
        }
        (right - left + 1, bottom - top + 1)
    }

    /// Every browsable dialog is the size Settings says, in *both* dimensions,
    /// and they're all the same size as each other.
    ///
    /// The bug this pins: the clipboard palette and the terminal quick-switch
    /// sized their height to their content, so at 99% they drew a short box at
    /// the top of the window while every dialog beside them filled the screen.
    /// An earlier version of this test only compared widths, which is exactly
    /// why the height drift survived it.
    #[test]
    fn every_dialog_is_the_configured_size_in_both_dimensions() {
        use crate::app::Dialog;
        let dir = workspace();
        let all = [
            Dialog::Files,
            Dialog::Recent,
            Dialog::Settings,
            Dialog::Help,
            Dialog::SearchFiles,
            Dialog::Todos,
            Dialog::Clipboard,
            Dialog::TerminalPicker,
        ];

        let mut expected: Option<(usize, usize)> = None;
        for d in all {
            let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
            app.dialog_size_pct = 99;
            app.open_dialog(d);
            let got = dialog_frame(&mut app);
            assert!(got.0 > 0 && got.1 > 0, "{d:?}: no dialog frame drawn");
            match expected {
                None => expected = Some(got),
                Some(want) => assert_eq!(
                    got, want,
                    "{d:?} is {got:?} but the others are {want:?} — every dialog \
                     must be the one configured size"
                ),
            }
        }

        // …and the size actually tracks the setting, both ways.
        for d in all {
            let mut small = App::new(Some(dir.path().to_path_buf())).unwrap();
            let mut big = App::new(Some(dir.path().to_path_buf())).unwrap();
            small.dialog_size_pct = 50;
            big.dialog_size_pct = 99;
            small.open_dialog(d);
            big.open_dialog(d);
            let (sw, sh) = dialog_frame(&mut small);
            let (bw, bh) = dialog_frame(&mut big);
            assert!(bw > sw, "{d:?}: width ignores the setting ({sw} -> {bw})");
            assert!(bh > sh, "{d:?}: height ignores the setting ({sh} -> {bh})");
        }
    }

    /// The terminal body is painted on `terminal_bg`, whatever that's set to —
    /// so a config that lifts it actually takes effect.
    #[test]
    fn the_terminal_body_is_painted_on_the_terminal_surface() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        app.toggle_terminal_modal();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let buf = terminal.backend().buffer();
        let on_terminal_surface = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].bg == app.theme.terminal_bg)
            .count();
        assert!(
            on_terminal_surface > 100,
            "the terminal body should be drawn on `terminal_bg`, found {on_terminal_surface} cells"
        );
    }

    #[test]
    fn current_line_is_tinted_across_the_full_pane_width() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let line_hl = app.theme.line_hl;
        let bg = app.theme.bg;
        assert_ne!(line_hl, bg, "the tint has to actually differ from the background");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // Scoped to the editor pane: the scrollbar column sits outside it and
        // is deliberately not part of the highlighted row.
        let (_, (px, _, pw, _)) = app.editor_panes[0];
        let pane = |row: &[ratatui::buffer::Cell]| {
            row[px as usize..(px + pw) as usize].to_vec()
        };

        // Caret starts on line 1 (`fn main() {`); line 2 is not the caret's.
        let caret_row = pane(&row_with(&terminal, "fn main()"));
        assert!(
            caret_row.iter().all(|c| c.bg == line_hl),
            "every cell of the caret's row is tinted, gutter and trailing space included"
        );
        let other_row = pane(&row_with(&terminal, "let x = 1"));
        assert!(
            other_row.iter().all(|c| c.bg != line_hl),
            "a row the caret isn't on keeps the plain background"
        );
    }

    #[test]
    fn indent_guides_mark_the_first_level_not_just_nested_ones() {
        // The bug this pins: guides that start one level in draw nothing at
        // all for singly-indented code, which is most code.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n    let x = 1;\n    if x > 0 {\n        dbg!(x);\n    }\n}\n",
        )
        .unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let guide = app.theme.indent_guide;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let one_level = row_with(&terminal, "if x > 0");
        assert_eq!(
            one_level.iter().filter(|c| c.symbol() == "\u{2502}").count(),
            1,
            "a 4-space line gets exactly one guide, at its first level"
        );
        assert!(
            one_level.iter().any(|c| c.symbol() == "\u{2502}" && c.fg == guide),
            "drawn in the indent-guide colour"
        );

        let two_levels = row_with(&terminal, "dbg!(x)");
        assert_eq!(
            two_levels.iter().filter(|c| c.symbol() == "\u{2502}").count(),
            2,
            "an 8-space line gets one guide per level"
        );

        let flush_left = row_with(&terminal, "fn main()");
        assert_eq!(
            flush_left.iter().filter(|c| c.symbol() == "\u{2502}").count(),
            0,
            "an unindented line gets none"
        );
    }

    #[test]
    fn indent_guides_follow_the_files_own_indent_width() {
        // A 2-space file must get guides every 2 columns, not every 4 — a
        // fixed width draws them through the middle of the text.
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() {\n  let x = 1;\n  if x > 0 {\n    dbg!(x);\n  }\n}\n",
        )
        .unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        assert_eq!(app.editors[0].indent_unit(), 2, "detected from the file itself");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let two_levels = row_with(&terminal, "dbg!(x)");
        assert_eq!(
            two_levels.iter().filter(|c| c.symbol() == "\u{2502}").count(),
            2,
            "4 columns of indent at a 2-space unit is two levels, not one"
        );
    }

    #[test]
    fn finished_unseen_terminal_shows_a_yellow_dot_while_a_different_tab_is_active() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.new_terminal(); // terminal 0
        app.new_terminal(); // terminal 1, now active
        assert_eq!(app.active_terminal, 1);
        app.terminals[0].set_finished_unseen_for_test();

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        let yellow = app.theme.yellow;
        let buf = terminal.backend().buffer();
        assert!(
            buf.content().iter().any(|c| c.symbol() == "\u{25cf}" && c.fg == yellow),
            "the unseen-but-finished tab 0 should show a yellow dot while tab 1 is active"
        );
        assert!(app.terminals[0].finished_unseen(), "still unseen — nobody switched to it");
    }

    #[test]
    fn switching_to_a_finished_unseen_terminal_clears_its_indicator() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.new_terminal(); // terminal 0
        app.new_terminal(); // terminal 1, now active
        app.terminals[0].set_finished_unseen_for_test();

        // Switch to (and render) the previously-unseen tab — that's what
        // "viewing" it means.
        app.active_terminal = 0;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        assert!(!app.terminals[0].finished_unseen(), "viewing the tab clears the indicator");
        let yellow = app.theme.yellow;
        let buf = terminal.backend().buffer();
        assert!(
            !buf.content().iter().any(|c| c.symbol() == "\u{25cf}" && c.fg == yellow),
            "no yellow dot should render once the tab has been viewed"
        );
    }

    #[test]
    fn help_overlay_lists_shortcuts_by_context() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_help();
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Keyboard Shortcuts"), "panel title");
        assert!(text.contains("Global"), "a section header (top of list)");
        assert!(text.contains("Save"), "an editor shortcut description (top of list)");

        // Scrolling to the bottom reveals the later sections.
        app.help_scroll(500);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen_text(&terminal).contains("Recent folders"),
            "the last section is reachable by scrolling"
        );
    }

    #[test]
    fn help_hides_folder_only_shortcuts_with_no_folder() {
        let mut app = App::new(None).unwrap(); // no folder open
        app.open_help();
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Recent folders"), "reachable shortcuts still shown");
        assert!(!text.contains("Editor"), "editor sections hidden without a folder");
        assert!(!text.contains("Open files"), "Files shortcut hidden without a folder");
    }

    #[test]
    fn find_bar_shows_key_hints() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        app.find_open();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("next"), "find bar shows next");
        assert!(text.contains("prev"), "find bar shows prev");
        assert!(text.contains("close"), "find bar shows close");
    }

    #[test]
    fn editor_footer_lists_shortcuts_over_two_rows() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        for s in [
            "Save", "Find", "Undo", "Redo", "Select", "Paste", "Settings", "Shortcuts", "Quit",
            "saved", "Search files",
        ] {
            assert!(text.contains(s), "footer should mention {s:?}");
        }
    }

    #[test]
    fn welcome_screen_shows_the_search_files_hint() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Search in files"), "welcome screen should offer ⌘⇧F up front");
    }

    /// The footer's displayed key combo must track `App::gui` — in TUI mode
    /// (the default) a bare 2-key ⌘S can collide with something the host
    /// terminal already reserves, so Oxru requires ⌘⌥S there instead; GUI
    /// mode keeps the plain 2-key ⌘S. Showing the wrong one is worse than
    /// showing nothing, since it sends the user to a combo that won't fire.
    /// One keymap: the footer prints the same combo whichever backend is
    /// drawing it. It used to show `⌘⌥S` in the terminal and `⌘S` in the
    /// window for the same action.
    #[test]
    fn the_footer_shows_the_same_combo_in_both_modes() {
        for gui in [false, true] {
            let dir = workspace();
            let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
            app.gui = gui;
            app.open_path(&dir.path().join("src/main.rs"));
            let mut terminal = Terminal::new(TestBackend::new(160, 30)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
            let text = screen_text(&terminal);
            assert!(text.contains("\u{2318}S "), "Save shows as the plain \u{2318}S (gui = {gui})");
            assert!(
                !text.contains("\u{2318}\u{2325}S"),
                "the old 3-key \u{2318}\u{2325}S must be gone (gui = {gui})"
            );
        }
    }

    #[test]
    fn help_overlay_shows_the_plain_combo_in_terminal_mode() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        assert!(!app.gui, "defaults to TUI mode");
        // Pinned rather than left at the default: this test is about which key
        // combo the sheet prints, so it shouldn't start failing because the
        // default dialog size changed and pushed a row out of view.
        app.dialog_size_pct = 99;
        app.open_help();
        let mut terminal = Terminal::new(TestBackend::new(120, 44)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("\u{2318}S"), "Save shows as the plain \u{2318}S in the terminal too");

        // The embedded terminal's own shortcuts are written literally — they
        // mirror a real terminal app, not an Oxru shortcut. Asserted against
        // the sheet's *data* rather than a scrolled screenshot: which page a
        // given row lands on changes every time a group is added, and that has
        // nothing to do with what this test is checking.
        let groups = shortcut_groups(true);
        let terminal_group = groups
            .iter()
            .find(|(name, _)| *name == "Terminal")
            .expect("the sheet has a Terminal group");
        assert!(
            terminal_group.1.iter().any(|(chord, label)| chord == "\u{2325}N" && *label == "New terminal"),
            "terminal's New stays the plain \u{2325}N"
        );
    }

    #[test]
    fn find_highlights_all_matches() {
        use ratatui::style::Color;
        let dir = workspace();
        let f = dir.path().join("m.txt");
        fs::write(&f, "\n\n\nfoo\nfoo\nfoo").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        app.find_open();
        for c in "foo".chars() {
            app.find_input(c);
        }
        assert_eq!(app.find.matches.len(), 3, "three matches found");

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let content = terminal.backend().buffer().content();
        let amber = content
            .iter()
            .filter(|c| {
                c.bg == Color::Rgb(0x4d, 0x3c, 0x14) || c.bg == Color::Rgb(0x8a, 0x60, 0x18)
            })
            .count();
        assert!(amber >= 9, "all three 3-char matches highlighted, got {amber}");
        assert!(
            content.iter().any(|c| c.bg == Color::Rgb(0x8a, 0x60, 0x18)),
            "the current match uses the brighter highlight colour"
        );
    }

    #[test]
    fn renders_blank_then_dialog() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("OXRU"));
        assert!(text.contains("Open files"));

        app.open_file_dialog();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Files"), "dialog title");
        assert!(text.contains("src"), "empty query shows the browse tree (folders)");
        assert!(text.contains("New File"), "footer action");
        assert!(text.contains("In Folder"), "tree-mode footer action");
        assert!(text.contains("Close"), "footer action");
        assert!(text.contains('\u{2500}'), "section dividers are drawn");
        assert!(text.contains("1 file"), "file count shown on the search row");

        app.file_dialog.query = "main".to_string();
        app.dialog_refilter();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("main.rs"), "search finds nested files");
    }

    #[test]
    fn grid_layout_is_left_right_bottom() {
        let area = Rect::new(0, 0, 100, 100);
        assert_eq!(editor_grid_rects(area, 1).len(), 1);

        let r2 = editor_grid_rects(area, 2);
        assert_eq!(r2.len(), 2);
        assert!(r2[0].x < r2[1].x, "two panes are left | right");
        assert_eq!(r2[0].y, r2[1].y);

        let r3 = editor_grid_rects(area, 3);
        assert_eq!(r3.len(), 3);
        assert!(r3[0].x < r3[1].x && r3[0].y == r3[1].y, "first two on top, left+right");
        assert!(r3[2].y > r3[0].y, "third pane is below");
        assert_eq!(r3[2].width, area.width, "bottom pane spans the full width");
    }

    #[test]
    fn grid_view_renders_each_open_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("beta.rs"), "fn b() {}\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("alpha.rs"));
        app.open_path(&dir.path().join("beta.rs"));
        // Caret bar overdraws its cell; park it off the first char of the active
        // pane so the content assertions below are unobscured.
        app.active_buffer().unwrap().move_doc_end();
        app.editor_grid = true;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("alpha.rs"), "left pane titled");
        assert!(text.contains("beta.rs"), "right pane titled");
        assert!(text.contains("fn a") && text.contains("fn b"), "both files' content shown");
    }

    #[test]
    fn dialog_rect_same_size_offset_with_depth() {
        let area = Rect::new(0, 0, 100, 100);
        let top = dialog_rect_stacked(area, 0, 90);
        let b1 = dialog_rect_stacked(area, 1, 90);
        let b2 = dialog_rect_stacked(area, 2, 90);
        // Every dialog is the same configured-percent size.
        assert_eq!((top.width, top.height), (90, 90));
        assert_eq!((b1.width, b1.height), (90, 90));
        // The focused (top) one is centered; lower ones are nudged down-right.
        assert_eq!((top.x, top.y), (5, 5));
        assert!(b1.x > top.x && b1.y > top.y);
        assert!(b2.x > b1.x && b2.y > b1.y);
    }

    #[test]
    fn dialog_rect_clamps_pct_to_80_99_range() {
        let area = Rect::new(0, 0, 100, 100);
        // Out-of-range values (a corrupted config, say) clamp rather than panic.
        let low = dialog_rect_stacked(area, 0, 10);
        assert_eq!((low.width, low.height), (80, 80));
        let high = dialog_rect_stacked(area, 0, 500);
        assert_eq!((high.width, high.height), (99, 99));
        let mid = dialog_rect_stacked(area, 0, 85);
        assert_eq!((mid.width, mid.height), (85, 85));
    }

    #[test]
    fn stacked_dialogs_focused_full_color_lower_dimmed() {
        // Open Files, then Settings on top. The focused Settings dialog draws in
        // full accent colour; the Files dialog beneath peeks out (offset) with a
        // dimmed border, so colour clearly falls off with depth.
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        app.open_settings();
        assert_eq!(app.dialogs.len(), 2);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // Top (focused) dialog renders its content at full colour.
        assert!(screen_text(&terminal).contains("Settings") || screen_text(&terminal).contains("Font"));

        let accent = app.theme.accent; // full colour (focused border)
        let dim = app.theme.dimmed(1).accent; // faded colour (lower border)
        assert_ne!(accent, dim, "dimming must change the colour");
        let content = terminal.backend().buffer().content();
        assert!(
            content.iter().any(|c| c.fg == accent),
            "focused dialog border is full accent"
        );
        assert!(
            content.iter().any(|c| c.fg == dim),
            "lower dialog peeks out with a dimmed border"
        );
    }

    #[test]
    fn search_shows_filename_before_folder() {
        // VSCode quick-open style: the filename is rendered first, the parent
        // folder after it (not the whole path filename-last).
        let dir = workspace();
        fs::create_dir_all(dir.path().join("client_flutter/lib/config")).unwrap();
        fs::write(
            dir.path().join("client_flutter/lib/config/constants.dart"),
            "x",
        )
        .unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        app.file_dialog.query = "constants".to_string();
        app.dialog_refilter();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        let name_at = text.find("constants.dart").expect("filename rendered");
        let folder_at = text.find("client_flutter").expect("folder rendered");
        assert!(
            name_at < folder_at,
            "filename should render before its folder (VSCode-style), got name@{name_at} folder@{folder_at}"
        );
    }

    #[test]
    fn renders_file_source() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        // Park the caret off the first character — the thin caret bar overdraws
        // its cell, and these assertions are about the text, not the caret.
        app.active_buffer().unwrap().move_doc_end();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("fn main"));
        assert!(text.contains("let x = 1"));
    }

    #[test]
    fn applies_vscode_colors_to_cells() {
        use ratatui::style::Color;
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let content = terminal.backend().buffer().content();
        assert!(
            content.iter().any(|c| c.fg == Color::Rgb(0x56, 0x9c, 0xd6)),
            "expected a VSCode-blue keyword token"
        );

        app.open_file_dialog();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let content = terminal.backend().buffer().content();
        // The selected row uses the accent-derived selection background
        // (default accent is green, so a darkened green).
        assert!(
            content.iter().any(|c| c.bg == Color::Rgb(0x1c, 0x42, 0x1e)),
            "expected the accent selection background on the selected row"
        );
    }

    #[test]
    fn footer_tab_hints_appear_with_multiple_tabs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "1\n").unwrap();
        fs::write(dir.path().join("b.txt"), "2\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();

        app.open_path(&dir.path().join("a.txt"));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(!screen_text(&terminal).contains("Switch"), "one tab: no tab hints");

        app.open_path(&dir.path().join("b.txt"));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Switch"), "two tabs: shows Switch");
        assert!(text.contains("Split"), "two tabs: shows the split-view toggle");
        assert!(!text.contains("Go to"), "two tabs: no more Go to (use Ctrl+Tab)");
    }

    #[test]
    fn gui_records_caret_position_without_hiding_text() {
        use ratatui::style::Color;
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui = true; // GUI has no native cursor: it overlays one via the GPU
        // post-processor instead of painting a bar glyph into the cell — so it
        // must never touch the ratatui buffer, and the character under the
        // caret (here, cursor parked at col 0 — "fn main") must stay intact.
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("fn main"), "caret must not hide the character it sits on");
        assert!(
            !terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|c| c.symbol() == "\u{258f}"),
            "GUI mode should not paint a caret bar glyph into the buffer"
        );
        // The first frame is always solid (the blink resets on the just-moved
        // caret), so exactly one caret — the accent-coloured primary — is queued
        // for the GPU overlay this frame.
        assert_eq!(app.gui_carets.len(), 1);
        assert_eq!(app.gui_carets[0].color, Color::Rgb(0x4c, 0xaf, 0x50));
    }

    #[test]
    fn gui_caret_does_not_bleed_through_an_open_dialog() {
        // The GPU caret overlay is painted as a final pass over the whole
        // finished frame, after any dialog has already drawn on top of the
        // editor — so it must not queue a caret at all while a dialog (or the
        // terminal modal) is the topmost surface, or the editor's caret shows
        // up as a stray floating bar over the dialog's own content.
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui = true;
        app.open_path(&dir.path().join("src/main.rs"));
        app.open_file_dialog();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            app.gui_carets.is_empty(),
            "no editor caret should be queued while a dialog is on top"
        );
    }

    #[test]
    fn terminal_uses_native_cursor_not_a_glyph() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // Default (terminal) mode: the host's hardware bar cursor is used, so we
        // position the real cursor and never paint a bar glyph over a cell.
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let has_bar = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|c| c.symbol() == "\u{258f}");
        assert!(!has_bar, "terminal mode should not paint a caret bar glyph");
        assert!(
            terminal.get_cursor_position().is_ok(),
            "terminal mode should position the native cursor"
        );
    }

    #[test]
    fn switching_tabs_shows_each_files_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "AAA_apple\n").unwrap();
        fs::write(dir.path().join("b.txt"), "BBB_banana\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        app.open_path(&dir.path().join("a.txt"));
        // Caret bar overdraws its cell; park it past the text on each tab.
        app.active_buffer().unwrap().move_doc_end();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("AAA_apple"), "tab A shows A");

        app.open_path(&dir.path().join("b.txt"));
        app.active_buffer().unwrap().move_doc_end();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let after_b = screen_text(&terminal);
        assert!(after_b.contains("BBB_banana"), "tab B shows B");
        assert!(!after_b.contains("AAA_apple"), "tab B must NOT show A's content");

        app.prev_tab();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let back_a = screen_text(&terminal);
        assert!(back_a.contains("AAA_apple"), "back on tab A shows A");
        assert!(!back_a.contains("BBB_banana"), "tab A must NOT show B's content");
    }

    #[test]
    fn editing_two_tabs_keeps_them_independent() {
        use crate::input;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let typ = |app: &mut App, s: &str| {
            for c in s.chars() {
                input::handle_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
        };

        app.open_path(&dir.path().join("a.txt"));
        typ(&mut app, "ALPHA");
        app.open_path(&dir.path().join("b.txt"));
        typ(&mut app, "BRAVO");
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let on_b = screen_text(&terminal);
        assert!(on_b.contains("BRAVO"), "B shows its own edits");
        assert!(!on_b.contains("ALPHA"), "B must not show A's edits");

        app.prev_tab();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let on_a = screen_text(&terminal);
        assert!(on_a.contains("ALPHA"), "A shows its own edits");
        assert!(!on_a.contains("BRAVO"), "A must not show B's edits");
    }

    #[test]
    fn rename_prompt_draws_block_cursor() {
        use ratatui::style::Color;
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.prompt.open(
            PromptKind::Rename,
            dir.path().join("notes.txt"),
            "notes.txt".to_string(),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let content = terminal.backend().buffer().content();
        // The block cursor is the only thing drawn with accent as a *background*.
        assert!(
            content.iter().any(|c| c.bg == Color::Rgb(0x4c, 0xaf, 0x50)),
            "rename prompt should show a visible block cursor"
        );
    }

    #[test]
    fn toast_shows_then_disappears() {
        use crate::app::ToastKind;
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        // No toast initially.
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(!screen_text(&terminal).contains("Copied"));

        // Raise one and it appears.
        app.notify("Copied 5 chars", ToastKind::Success);
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen_text(&terminal).contains("Copied 5 chars"),
            "toast text should be on screen"
        );
    }

    #[test]
    fn dialog_browses_tree_when_query_empty() {
        // Empty query shows a collapsible tree: a nested file stays hidden until
        // its folder is expanded, then it appears. (Typing switches to the flat
        // search list — covered elsewhere.)
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("src"), "folders are shown in the tree");
        assert!(!text.contains("main.rs"), "nested file hidden until its folder is expanded");

        // Move down to the `src` folder and expand it; the file now shows.
        app.dialog_down();
        app.dialog_tree_expand();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen_text(&terminal).contains("main.rs"), "file appears once expanded");
    }

    #[test]
    fn search_bolds_matched_characters() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        // Type a query that fuzzy-matches main.rs.
        for c in "main".chars() {
            app.file_dialog.query.push(c);
        }
        app.dialog_refilter();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // The matched chars (m, a, i, n in "main.rs") render bold; the extension
        // dot/rs do not. Check at least one bold and one non-bold cell exist for
        // the result text.
        let buf = terminal.backend().buffer();
        let cells: Vec<_> = buf.content().iter().collect();
        let bold_m = cells
            .iter()
            .any(|c| c.symbol() == "m" && c.modifier.contains(ratatui::style::Modifier::BOLD));
        assert!(bold_m, "the matched 'm' should be bold");
    }

    #[test]
    fn search_highlights_matched_chars_in_accent() {
        use ratatui::style::{Color, Modifier};
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.txt"), "").unwrap();
        fs::write(dir.path().join("alphabet.md"), "").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        for c in "alp".chars() {
            app.file_dialog.query.push(c);
        }
        app.dialog_refilter();
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // With two matches, the non-selected one renders its matched chars in
        // accent green + bold (the selected one uses bold-on-highlight instead).
        let green = Color::Rgb(0x4c, 0xaf, 0x50);
        let buf = terminal.backend().buffer();
        let has_green_bold_match = buf.content().iter().any(|c| {
            c.symbol() == "a" && c.fg == green && c.modifier.contains(Modifier::BOLD)
        });
        assert!(
            has_green_bold_match,
            "matched characters in a result should be accent-green + bold"
        );
    }

    #[test]
    fn renders_at_tiny_size_without_panic() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        app.open_file_dialog();
        for (w, h) in [(1u16, 1u16), (10, 3), (40, 8), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn search_files_dialog_renders_grouped_results_with_the_match_highlighted() {
        let dir = workspace();
        fs::write(dir.path().join("src/main.rs"), "fn main() {\n    let needle = 1;\n}\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        app.run_project_search();
        // Project search now runs on a background thread (see
        // `App::run_project_search`), so poll for the result instead of
        // assuming it's ready synchronously.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !app.poll_search_results() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(app.project_search.total_matches(), 1);

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Search in Files"), "dialog title");
        assert!(text.contains("main.rs"), "file header shown");
        assert!(text.contains("needle"), "the match preview is shown");

        let buf = terminal.backend().buffer();
        let accent = app.theme.accent;
        assert!(
            buf.content().iter().any(|c| c.symbol() == "n" && c.fg == accent && c.modifier.contains(Modifier::BOLD)),
            "the matched word should be bold + accent, like the Files dialog's fuzzy highlight"
        );
    }

    #[test]
    fn search_files_dialog_survives_tiny_sizes_and_before_any_search() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_search_files();
        for (w, h) in [(1u16, 1u16), (10, 3), (40, 8), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }

    #[test]
    fn search_files_dialog_shows_a_pending_notice_while_the_debounce_is_armed() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        assert!(app.project_search.pending_search_at.is_some());

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("Searching"), "should hint that a search is pending, got {text:?}");
    }

    #[test]
    fn settings_dialog_shows_all_five_sections_and_survives_tiny_sizes() {
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        app.open_settings();

        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Row-major cell grid; join per-row so a label can't accidentally
        // concatenate with the next row's leading text.
        let width = buf.area.width as usize;
        let text: String = buf
            .content()
            .chunks(width.max(1))
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Font size"));
        assert!(text.contains("Terminal FPS"));
        assert!(text.contains("Dialog size"));
        assert!(text.contains("Word wrap"));
        assert!(text.contains("Theme Color"));
        // Every offered FPS step is listed.
        for f in crate::config::FPS_OPTIONS {
            assert!(text.contains(&f.to_string()), "missing fps option {f}");
        }

        for (w, h) in [(1u16, 1u16), (10, 3), (40, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }

    /// The exact bug word wrap fixes: without it, a line longer than the
    /// pane just runs off the right edge — its tail is clipped and never
    /// shown anywhere, not even by scrolling right (there's no horizontal
    /// scroll). Documented here as the "before" baseline the wrap tests
    /// below are the fix for.
    #[test]
    fn word_wrap_off_clips_a_long_lines_tail_off_screen() {
        let dir = workspace();
        fs::write(dir.path().join("long.txt"), "AAAA BBBB CCCC DDDD EEEE FFFF\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("long.txt"));
        assert!(!app.word_wrap, "off by default");

        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("AAAA"), "the start of the line is visible");
        assert!(!text.contains("FFFF"), "the tail is clipped off-screen, not visible anywhere");
    }

    #[test]
    fn word_wrap_on_splits_a_long_line_across_rows_at_word_boundaries() {
        let dir = workspace();
        fs::write(dir.path().join("long.txt"), "AAAA BBBB CCCC DDDD EEEE FFFF\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("long.txt"));
        app.word_wrap = true;

        // Narrow enough (30 total, ~4-5 for the gutter) that the 30-char
        // line can't fit on one row.
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let rows = screen_rows(&terminal);

        let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle} not visible"));
        let row_a = row_of("AAAA");
        let row_f = row_of("FFFF");
        assert!(row_f > row_a, "the line's tail should land on a later row than its start");

        // The wrapped continuation row's gutter is blank (no line number) —
        // only the line's first row shows "1".
        assert!(rows[row_a].trim_start().starts_with('1'), "first row shows the line number");
        let cont_row_prefix: String = rows[row_f].chars().take(5).collect();
        assert!(!cont_row_prefix.contains('1'), "continuation row's gutter stays blank, got {cont_row_prefix:?}");
    }

    #[test]
    fn word_wrap_survives_tiny_sizes_without_panic() {
        let dir = workspace();
        fs::write(dir.path().join("long.txt"), "a very long line of plain text that should wrap\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("long.txt"));
        app.word_wrap = true;
        for (w, h) in [(1u16, 1u16), (5, 3), (10, 5), (40, 8), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| render(f, &mut app)).unwrap();
        }
    }
}
