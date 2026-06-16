//! All ratatui rendering. The UI is intentionally minimal: a blank screen with
//! one hint (or an open file), a thin status footer, and the file dialog /
//! prompt overlays.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span as TSpan};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Dialog, ToastKind};
use crate::prompt::PromptKind;
use crate::theme::Theme;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(app.theme.bg)),
        area,
    );

    // Full-screen main area with a status footer: two rows while editing (so all
    // the shortcuts fit without being cut off), one line otherwise.
    let footer_h = if app.active_editor.is_some() { 2 } else { 1 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_h)])
        .split(area);

    if app.active_editor.is_some() {
        // Tab strip on top, editor below.
        let ed = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(rows[0]);
        render_tabs(f, ed[0], app);
        render_editor(f, ed[1], app);
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
    for (depth, d) in stack.into_iter().enumerate() {
        let below = n - 1 - depth; // 0 = focused/top
        app.theme = if below == 0 {
            base_theme.clone()
        } else {
            base_theme.dimmed(below as u32)
        };
        match d {
            Dialog::Terminal => render_terminal_modal(f, area, app, below),
            Dialog::Files => render_file_dialog(f, area, app, below),
            Dialog::Settings => render_settings(f, area, app, below),
            Dialog::Recent => render_recent(f, area, app, below),
            Dialog::Help => render_help(f, area, app, below),
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

fn conv_color(c: vt100::Color, default: Color) -> Color {
    match c {
        vt100::Color::Default => default,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// The terminal modal: a centered ~80%×80% dialog holding a tab strip, the
/// terminal body (one terminal or an auto-arranged grid of all of them), and a
/// footer of Alt shortcuts.
fn render_terminal_modal(f: &mut Frame, area: Rect, app: &mut App, below: usize) {
    let t = app.theme.clone();
    // Re-recorded by render_one_terminal for the active pane each frame. Only the
    // focused (top) terminal accepts mouse selection, so leave it cleared when
    // the terminal is buried under another dialog.
    app.terminal_view = None;
    let focused = app.top_dialog() == Some(Dialog::Terminal);

    // Centered dialog, smaller the higher it sits in the stack.
    let rect = dialog_rect_stacked(area, below);

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
        .style(Style::default().bg(t.bg));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);

    // Tab strip (duplicate names get a disambiguating index). Scrolls
    // horizontally so the active tab is always visible.
    let labels = app.terminal_labels();
    let mut groups: Vec<Vec<TSpan>> = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let active = i == app.active_terminal;
        let style = if active {
            Style::default().fg(t.accent).bg(t.bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.fg_dim).bg(t.bg_dark)
        };
        groups.push(vec![
            TSpan::styled(format!(" {label} "), style),
            TSpan::styled("\u{2502}", Style::default().fg(t.bg_light).bg(t.bg_dark)),
        ]);
    }
    let line = scrolling_tab_line(groups, app.active_terminal, rows[0].width as usize, &t);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(t.bg_dark)),
        rows[0],
    );

    // Body: a grid of all terminals, or just the active one.
    let body = rows[1];
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
                    let label = labels[idx].clone();
                    render_one_terminal(f, *cr, app, idx, &label, &t, idx == app.active_terminal, true);
                    idx += 1;
                }
            }
        }
    } else if app.active_terminal < app.terminals.len() {
        let i = app.active_terminal;
        let label = labels[i].clone();
        render_one_terminal(f, body, app, i, &label, &t, true, false);
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
    let mid = refs.len().div_ceil(2);
    let fw = rows[2].width;
    f.render_widget(
        Paragraph::new(vec![
            justified_actions(&refs[..mid], fw, &t),
            justified_actions(&refs[mid..], fw, &t),
        ])
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
            .style(Style::default().bg(t.bg));
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

    let mut lines = Vec::with_capacity(inner.height as usize);
    for row in 0..srows.min(inner.height) {
        let mut spans = Vec::with_capacity(inner.width as usize);
        for col in 0..scols.min(inner.width) {
            match screen.cell(row, col) {
                Some(cell) => {
                    let raw = cell.contents();
                    let contents = if raw.is_empty() {
                        " ".to_string()
                    } else {
                        raw.to_string()
                    };
                    let mut style = Style::default()
                        .fg(conv_color(cell.fgcolor(), t.fg))
                        .bg(conv_color(cell.bgcolor(), t.bg));
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
                    spans.push(TSpan::styled(contents, style));
                }
                None => spans.push(TSpan::raw(" ")),
            }
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

/// OS-styled Control chord for an arbitrary key label, e.g. `⌃Tab` / `Ctrl+Tab`.
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

/// Command-key notation (⌘ on macOS, "Ctrl+" elsewhere) for the shortcuts that
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
fn justified_actions(items: &[(&str, &str)], width: u16, t: &Theme) -> Line<'static> {
    let accent = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(t.fg_dim);
    let widths: Vec<usize> = items
        .iter()
        .map(|(k, l)| k.chars().count() + 1 + l.chars().count())
        .collect();
    let total: usize = widths.iter().sum();
    let n = items.len();
    // Slack to spread across the n-1 inter-item gaps (min 2 if it doesn't fit).
    let (base_gap, extra) = if n > 1 && (width as usize) > total {
        let slack = width as usize - total;
        (slack / (n - 1), slack % (n - 1))
    } else {
        (2, 0)
    };
    let mut spans = Vec::with_capacity(items.len() * 4);
    for (i, (k, l)) in items.iter().enumerate() {
        if i > 0 {
            let gap = base_gap + usize::from(i <= extra);
            spans.push(TSpan::styled(" ".repeat(gap), dim));
        }
        spans.push(TSpan::styled(k.to_string(), accent));
        spans.push(TSpan::styled(" ", dim));
        spans.push(TSpan::styled(l.to_string(), dim));
    }
    Line::from(spans)
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

/// Rect for a dialog `below` levels under the focused one (0 = focused). Every
/// dialog is the same 90% box; the focused one is centered and each one below it
/// is nudged down-right by a couple of cells so it peeks out behind like a stack
/// of cards. Clamped so the offset never runs off the screen.
fn dialog_rect_stacked(area: Rect, below: usize) -> Rect {
    let w = (area.width as u32 * 90 / 100) as u16;
    let h = (area.height as u32 * 90 / 100) as u16;
    let cx = area.x + (area.width.saturating_sub(w)) / 2;
    let cy = area.y + (area.height.saturating_sub(h)) / 2;
    let max_x = area.x + area.width.saturating_sub(w);
    let max_y = area.y + area.height.saturating_sub(h);
    let x = (cx + below as u16 * 2).min(max_x);
    let y = (cy + below as u16).min(max_y);
    Rect::new(x, y, w, h)
}

/// The empty starting screen: a centred title and the two shortcuts.
fn render_blank(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let hint = |label: &str, chord: String| {
        Line::from(vec![
            TSpan::styled("Press ", Style::default().fg(t.fg_dim)),
            TSpan::styled(chord, Style::default().fg(t.accent).add_modifier(Modifier::BOLD)),
            TSpan::styled(format!(" to {label}"), Style::default().fg(t.fg_dim)),
        ])
    };

    let mut lines = vec![Line::from(""); (area.height / 2).saturating_sub(2) as usize];
    lines.push(Line::from(TSpan::styled(
        "Oxru",
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    if app.has_folder() {
        lines.push(hint("open files", alt_str("F")));
        lines.push(hint("open a folder", cmd_str("O")));
        lines.push(hint("open recent folders", alt_str("O")));
        lines.push(hint("open a terminal", alt_str("T")));
    } else {
        // No folder open — only show what actually works here. Files and the
        // terminal both need a folder, so they're hidden until one is open.
        lines.push(Line::from(TSpan::styled(
            "No folder open",
            Style::default().fg(t.fg_dim),
        )));
        lines.push(Line::from(""));
        lines.push(hint("open a folder", cmd_str("O")));
        lines.push(hint("open a recent folder", alt_str("O")));
    }
    lines.push(hint("settings", cmd_str(",")));
    lines.push(hint("shortcuts", "F1".to_string()));
    lines.push(hint("quit", cmd_key('q')));
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(Style::default().bg(t.bg)),
        area,
    );
}

/// The editor tab strip: open files, with the active one highlighted and a dot
/// on any with unsaved changes. The strip scrolls horizontally so the active
/// tab is always visible (see [`scrolling_tab_line`]).
fn render_tabs(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    f.render_widget(Block::default().style(Style::default().bg(t.bg_dark)), area);

    let mut groups: Vec<Vec<TSpan>> = Vec::new();
    for (i, buf) in app.editors.iter().enumerate() {
        let active = Some(i) == app.active_editor;
        let bg = if active { t.bg } else { t.bg_dark };
        let name_style = if active {
            Style::default().fg(t.accent).bg(bg).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.fg_dim).bg(bg)
        };
        let mut g = vec![
            TSpan::styled(" ", Style::default().bg(bg)),
            TSpan::styled(buf.name(), name_style),
        ];
        if buf.modified {
            g.push(TSpan::styled(" \u{25cf}", Style::default().fg(t.yellow).bg(bg)));
        }
        g.push(TSpan::styled(" ", Style::default().bg(bg)));
        g.push(TSpan::styled(
            "\u{2502}",
            Style::default().fg(t.bg_light).bg(t.bg_dark),
        ));
        groups.push(g);
    }
    let line = scrolling_tab_line(groups, app.active_editor.unwrap_or(0), area.width as usize, t);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(t.bg_dark)),
        area,
    );
}

/// Assemble a horizontally-scrollable tab strip. `groups` holds the spans for
/// each tab (including its trailing separator). When the tabs are wider than
/// `width`, the strip scrolls so the active tab stays fully visible, and chevrons
/// (`‹`/`›`) mark the sides where tabs are hidden. Reserves one column on each
/// edge for those chevrons so the visible tabs never shift as you scroll.
fn scrolling_tab_line<'a>(
    groups: Vec<Vec<TSpan<'a>>>,
    active: usize,
    width: usize,
    t: &Theme,
) -> Line<'a> {
    let widths: Vec<usize> = groups
        .iter()
        .map(|g| g.iter().map(|s| s.width()).sum())
        .collect();
    let total: usize = widths.iter().sum();

    // Everything fits — render the whole strip, no chevrons.
    if groups.is_empty() || total <= width {
        return Line::from(groups.into_iter().flatten().collect::<Vec<_>>());
    }

    let avail = width.saturating_sub(2); // leave a column for each chevron
    let active = active.min(groups.len() - 1);

    // Grow a window outward from the active tab until nothing more fits. Prefer
    // revealing later tabs first, then earlier ones.
    let mut lo = active;
    let mut hi = active + 1;
    let mut used = widths[active].min(avail);
    loop {
        let mut grew = false;
        if hi < groups.len() && used + widths[hi] <= avail {
            used += widths[hi];
            hi += 1;
            grew = true;
        }
        if lo > 0 && used + widths[lo - 1] <= avail {
            lo -= 1;
            used += widths[lo];
            grew = true;
        }
        if !grew {
            break;
        }
    }

    let chev = Style::default().fg(t.fg_dim).bg(t.bg_dark);
    let mut spans: Vec<TSpan> = Vec::new();
    spans.push(TSpan::styled(if lo > 0 { "\u{2039}" } else { " " }, chev));
    for g in groups.into_iter().take(hi).skip(lo) {
        spans.extend(g);
    }
    spans.push(TSpan::styled(if hi < widths.len() { "\u{203a}" } else { " " }, chev));
    Line::from(spans)
}

/// A thin status line. While editing it shows the key shortcuts on the left and
/// the active file's save state + cursor position on the right; otherwise it
/// shows any status message.
fn render_footer(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let dim = Style::default().fg(t.fg_dim);
    let accent = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);

    // No file open: show any status message on a single line.
    let Some(i) = app.active_editor else {
        let line = if app.status.is_empty() {
            Line::from("")
        } else {
            Line::from(TSpan::styled(format!(" {}", app.status), dim))
        };
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(t.bg_dark)),
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
    // how to add a column caret and how to collapse back to one.
    if b.has_extra_carets() {
        hints.push((cmd_str("\u{2325}\u{2195}"), "Add caret"));
        hints.push(("Esc".to_string(), "One caret"));
    } else {
        hints.push((cmd_key('d'), "Multi-cursor"));
    }
    hints.push((cmd_str("\u{21e7}C"), "Copy path"));
    hints.push((alt_str("F"), "Files"));
    hints.push((alt_str("T"), "Term"));
    hints.push((cmd_str(","), "Settings"));
    hints.push(("F1".to_string(), "Shortcuts"));
    hints.push((cmd_key('q'), "Quit"));

    // Status (bottom-right): Ln/Col · language · indent · saved state.
    let lang = b.lang.map(|l| l.name).unwrap_or("Plain Text");
    let (state, scolor) = if b.modified {
        ("\u{25cf} unsaved", t.yellow)
    } else {
        ("\u{2713} saved", t.green)
    };
    // With multiple carets, surface the count instead of a single Ln/Col.
    let pos = if b.has_extra_carets() {
        format!("{} carets   {lang}   Spaces: 4   ", b.caret_count())
    } else {
        format!(
            "Ln {}, Col {}   {lang}   Spaces: 4   ",
            b.cursor_row() + 1,
            b.cursor_col() + 1
        )
    };
    let status: Vec<TSpan> = vec![
        TSpan::styled(pos, dim),
        TSpan::styled(format!("{state} "), Style::default().fg(scolor)),
    ];
    let status_w: usize = status.iter().map(|s| s.content.chars().count()).sum();

    // One styled "⟨key⟩ label" segment, with its display width.
    let seg = |chord: &str, label: &str| -> (Vec<TSpan<'static>>, usize) {
        (
            vec![
                TSpan::styled(format!(" {chord} "), accent),
                TSpan::styled(format!("{label}  "), dim),
            ],
            chord.chars().count() + label.chars().count() + 4,
        )
    };

    let w = area.width as usize;
    let two = area.height >= 2;
    let mut idx = 0;

    // Top row: flow hints until the width is full.
    let mut line0: Vec<TSpan> = Vec::new();
    let mut w0 = 0;
    while idx < hints.len() {
        let (spans, sw) = seg(&hints[idx].0, hints[idx].1);
        if w0 + sw > w {
            break;
        }
        line0.extend(spans);
        w0 += sw;
        idx += 1;
    }

    if two {
        // Bottom row: remaining hints on the left, status pinned to the right.
        let mut line1: Vec<TSpan> = Vec::new();
        let mut w1 = 0;
        let limit = w.saturating_sub(status_w + 1);
        while idx < hints.len() {
            let (spans, sw) = seg(&hints[idx].0, hints[idx].1);
            if w1 + sw > limit {
                break;
            }
            line1.extend(spans);
            w1 += sw;
            idx += 1;
        }
        line1.push(TSpan::raw(" ".repeat(w.saturating_sub(w1 + status_w))));
        line1.extend(status);

        let r0 = Rect::new(area.x, area.y, area.width, 1);
        let r1 = Rect::new(area.x, area.y + 1, area.width, 1);
        f.render_widget(Paragraph::new(Line::from(line0)).style(Style::default().bg(t.bg_dark)), r0);
        f.render_widget(Paragraph::new(Line::from(line1)).style(Style::default().bg(t.bg_dark)), r1);
    } else {
        // Single row: status on the right of the one line we have.
        line0.push(TSpan::raw(" ".repeat(w.saturating_sub(w0 + status_w))));
        line0.extend(status);
        f.render_widget(Paragraph::new(Line::from(line0)).style(Style::default().bg(t.bg_dark)), area);
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

/// The editor area: a single file, or — in grid view — every open file tiled.
fn render_editor(f: &mut Frame, area: Rect, app: &mut App) {
    app.editor_panes.clear();
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
            if active {
                app.ensure_cursor_visible(inner.height);
            }
            app.editor_panes
                .push((idx, (inner.x, inner.y, inner.width, inner.height)));
            render_editor_pane(f, inner, app, idx, active);
        }
    } else if let Some(idx) = app.active_editor {
        app.ensure_cursor_visible(area.height);
        app.editor_panes
            .push((idx, (area.x, area.y, area.width, area.height)));
        render_editor_pane(f, area, app, idx, true);
    }
    // The find bar floats over the top-right of the editor area when open.
    if app.find.active {
        render_find_bar(f, area, app);
    }
}

/// The in-file find bar: a compact box at the editor's top-right showing the
/// query, a live "N of M" count, and the navigation hint.
fn render_find_bar(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let w = 48u16.min(area.width);
    if w < 12 {
        return;
    }
    let x = area.x + area.width.saturating_sub(w);
    // Two inner rows: the query (+ count), then the navigation key hints.
    let rect = Rect::new(x, area.y, w, 4);
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

    // Key hints on the second inner row.
    if inner.height >= 2 {
        let acc = Style::default().fg(t.accent).add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(t.fg_dim);
        let hints = Line::from(vec![
            TSpan::styled("\u{21b5}/\u{2193}", acc),
            TSpan::styled(" next  ", dim),
            TSpan::styled("\u{21e7}\u{21b5}/\u{2191}", acc),
            TSpan::styled(" prev  ", dim),
            TSpan::styled("Esc", acc),
            TSpan::styled(" close", dim),
        ]);
        f.render_widget(
            Paragraph::new(hints).style(Style::default().bg(t.bg_dark)),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }

    // Right side: match count (or "No results").
    let count = if app.find.query.is_empty() {
        String::new()
    } else if app.find.matches.is_empty() {
        "No results".to_string()
    } else {
        format!("{} of {}", app.find.current + 1, app.find.matches.len())
    };
    let count_w = count.chars().count() as u16;

    // Left side: the query as a full single-line input (cursor + selection),
    // leaving room on the right for the count.
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
            inner,
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

    let hl = app.highlighted_for(idx);
    let height = area.height as usize;
    let mut lines = Vec::with_capacity(height);
    for r in scroll_row..(scroll_row + height) {
        let num_style = if r == cursor_row {
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.fg_dim)
        };
        let mut spans = vec![TSpan::styled(
            format!("{:>width$} ", r + 1, width = (gutter_w - 1) as usize),
            num_style,
        )];
        if let Some(line) = hl.get(r) {
            for (text, style) in line {
                spans.push(TSpan::styled(text.clone(), *style));
            }
        } else {
            spans.push(TSpan::styled("~", Style::default().fg(t.bg_light)));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(t.bg)),
        area,
    );

    // Paint the selection background over the affected cells — the primary
    // selection plus every secondary caret's selection (multi-cursor).
    {
        let b = &app.editors[idx];
        let buf = f.buffer_mut();
        if let Some((s, e)) = b.selection() {
            paint_selection(buf, b, area, gutter_w, scroll_row, height, s, e, t.sel_bg);
        }
        for caret in b.extra_carets() {
            let (s, e) = caret.bounds();
            if e > s {
                paint_selection(buf, b, area, gutter_w, scroll_row, height, s, e, t.sel_bg);
            }
        }
    }

    // Highlight every in-file find match (the current one brighter), painted
    // over the selection so all hits are visible while searching.
    if active && app.find.active && !app.find.matches.is_empty() {
        let b = &app.editors[idx];
        let last_visible = (scroll_row + height).min(b.line_count());
        let cur = app.find.current;
        let buf = f.buffer_mut();
        for (mi, &(ms, me)) in app.find.matches.iter().enumerate() {
            if me <= ms {
                continue;
            }
            let first_row = b.rope.char_to_line(ms);
            let last_row = b.rope.char_to_line(me - 1);
            // Skip matches entirely above or below the viewport.
            if last_row < scroll_row || first_row >= last_visible {
                continue;
            }
            let color = if mi == cur { t.find_current } else { t.find_match };
            for r in first_row.max(scroll_row)..=last_row.min(last_visible.saturating_sub(1)) {
                let line_start = b.rope.line_to_char(r);
                let row_end = line_start + b.line_len_chars(r);
                let from = ms.max(line_start);
                let to = me.min(row_end);
                if from >= to {
                    continue;
                }
                let y = area.y + (r - scroll_row) as u16;
                for col in (from - line_start)..(to - line_start) {
                    let x = area.x + gutter_w + col as u16;
                    if x < area.x + area.width && y < area.y + area.height {
                        buf[(x, y)].set_bg(color);
                    }
                }
            }
        }
    }

    // Thin blinking caret — a VSCode-style I-beam drawn as a left-edge vertical
    // bar (`▏`) sitting just before the cursor's character, so the glyph stays
    // readable. Focused pane only; the GUI backend renders no native cursor.
    // Drawing only `fg` keeps the cell's background (and the char when "off").
    if blink_on && cursor_row >= scroll_row {
        let cx = area.x + gutter_w + cursor_col as u16;
        let cy = area.y + (cursor_row - scroll_row) as u16;
        if cx < area.x + area.width && cy < area.y + area.height {
            f.render_widget(
                Paragraph::new(Line::from(TSpan::styled(
                    "\u{258f}",
                    Style::default().fg(t.accent),
                ))),
                Rect::new(cx, cy, 1, 1),
            );
        }
    }

    // Secondary carets (multi-cursor): the same bar in a dimmer colour, blinking
    // in sync with the primary.
    if blink_on {
        let b = &app.editors[idx];
        let last_visible = (scroll_row + height).min(b.line_count());
        for caret in b.extra_carets() {
            let row = b.rope.char_to_line(caret.cursor);
            if row < scroll_row || row >= last_visible {
                continue;
            }
            let col = caret.cursor - b.rope.line_to_char(row);
            let cx = area.x + gutter_w + col as u16;
            let cy = area.y + (row - scroll_row) as u16;
            if cx < area.x + area.width && cy < area.y + area.height {
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

/// Paint a selection's background over the cells it covers in the visible
/// viewport. Shared by the primary selection and every secondary caret.
#[allow(clippy::too_many_arguments)]
fn paint_selection(
    buf: &mut ratatui::buffer::Buffer,
    b: &crate::buffer::Buffer,
    area: Rect,
    gutter_w: u16,
    scroll_row: usize,
    height: usize,
    sel_s: usize,
    sel_e: usize,
    color: Color,
) {
    for r in scroll_row..(scroll_row + height).min(b.line_count()) {
        let line_start = b.rope.line_to_char(r);
        let line_chars = b.line_len_chars(r);
        // Intersect the selection with this row's character span. Selecting a
        // line's trailing newline shows one extra cell, like most editors.
        let row_end = line_start + line_chars;
        let from = sel_s.max(line_start);
        let to = sel_e.min(row_end + 1).min(b.rope.len_chars());
        if from >= to {
            continue;
        }
        let y = area.y + (r - scroll_row) as u16;
        for col in (from - line_start)..(to - line_start) {
            let x = area.x + gutter_w + col as u16;
            if x < area.x + area.width && y < area.y + area.height {
                buf[(x, y)].set_bg(color);
            }
        }
    }
}

/// The Explorer file-operation prompt (new file / folder, rename, delete). A
/// compact box near the top so the list it acts on stays visible underneath,
/// rather than a full-screen overlay.
fn render_prompt(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    // Height: borders + an input row (when typing) + a footer row.
    let h = if app.prompt.needs_input() { 4 } else { 3 };
    let w = (area.width as u32 * 7 / 10).clamp(44, 88).min(area.width as u32) as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    // Sit a little below the top so it reads as floating above the content.
    let y = area.y + (area.height / 6).min(area.height.saturating_sub(h));
    let rect = Rect::new(x, y, w, h);

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
        let key = |k: &str| TSpan::styled(
            format!(" {k} "),
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        );
        let lbl = |s: &str| TSpan::styled(s.to_string(), Style::default().fg(t.fg_dim));
        let foot = match app.prompt.kind() {
            Some(PromptKind::CloseUnsaved) => Line::from(vec![
                key("Y"), lbl("save   "),
                key("N"), lbl("discard   "),
                key("Esc"), lbl("cancel"),
            ]),
            Some(PromptKind::CloseTab) | Some(PromptKind::QuitUnsaved) => Line::from(vec![
                key("Y"), lbl("save   "),
                key("N"), lbl("don't save   "),
                key("Esc"), lbl("cancel"),
            ]),
            _ => Line::from(vec![
                key("Enter"), lbl("confirm   "),
                key("Esc"), lbl("cancel"),
            ]),
        };
        let foot_rect = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        f.render_widget(
            Paragraph::new(foot).style(Style::default().bg(t.bg_dark)),
            foot_rect,
        );
    }
}

/// The "Recent folders" dialog: a multi-select list; Enter opens each checked
/// folder (or the one under the cursor) in its own window.
fn render_recent(f: &mut Frame, area: Rect, app: &App, below: usize) {
    let t = &app.theme;
    let rect = dialog_rect_stacked(area, below);

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
            "Pick folders to open — Space to check, Enter opens each in its own window",
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
        ("\u{2318}O", "Open other\u{2026}"),
        ("Esc", "Close"),
    ];
    f.render_widget(
        Paragraph::new(justified_actions(&foot, parts[4].width, t)).style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

/// The Settings dialog: live font size and a theme-colour picker.
fn render_settings(f: &mut Frame, area: Rect, app: &App, below: usize) {
    use crate::theme::ACCENT_PALETTE;
    let t = &app.theme;
    let rect = dialog_rect_stacked(area, below);

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
    let color_focused = app.settings_focus == 1;

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
            Paragraph::new(justified_actions(&foot, foot_rect.width, t)).style(Style::default().bg(t.bg_dark)),
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
fn dialog_tree_lines(app: &App, t: &Theme, list_h: usize) -> Vec<Line<'static>> {
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
    // Scroll so the selected row stays in view (selection pinned near the bottom
    // of the window, matching the flat list).
    let start = sel.saturating_sub(list_h.saturating_sub(1));
    for (i, e) in entries.iter().enumerate().skip(start).take(list_h) {
        let selected = i == sel;
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
    let rect = dialog_rect_stacked(area, below);

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
            Constraint::Length(2), // 4 two-line action footer
        ])
        .split(inner);

    let query_empty = app.file_dialog.query.is_empty();
    let list_h = parts[2].height as usize;

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
        f.render_widget(
            Paragraph::new(Line::from(TSpan::styled(
                "Type to search, or browse below\u{2026}",
                Style::default().fg(t.fg_dim),
            )))
            .style(Style::default().bg(t.bg_dark)),
            field,
        );
        draw_block_cursor(f, field.x, field.y, ' ', &t);
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
        dialog_tree_lines(app, &t, list_h)
    } else {
        Vec::new()
    };
    if !query_empty {
        let start = app.file_dialog.selected.saturating_sub(list_h.saturating_sub(1));
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
            let (display, is_dir) = match app.dialog_entries.get(src) {
                Some((_, d)) => (app.dialog_display[src].clone(), *d),
                None => (String::new(), false),
            };
            // The display string carries a trailing '/' for folders; strip it so
            // the filename/folder split is clean.
            let rel = display.strip_suffix('/').unwrap_or(&display);
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
                (
                    Style::default().fg(t.fg),
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

    // Two-line action footer, each line justified across the full width. The
    // shown actions differ between browsing the tree and searching.
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
            ("\u{2325}H", junk_label),
            ("Esc", "Close"),
            ("\u{2318}Q", "Quit"),
        ]
    };
    let mid = actions.len().div_ceil(2);
    let fw = parts[4].width;
    f.render_widget(
        Paragraph::new(vec![
            justified_actions(&actions[..mid], fw, &t),
            justified_actions(&actions[mid..], fw, &t),
        ])
        .style(Style::default().bg(t.bg_dark)),
        parts[4],
    );

}

/// Every keyboard shortcut, grouped by where it applies — the data behind the
/// F1 cheat-sheet. `⌘` entries also fire on Ctrl (noted in the overlay). When no
/// folder is open the folder-requiring sections are dropped, so only shortcuts
/// that actually work are shown.
fn shortcut_groups(has_folder: bool) -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    // Global section: hide Files / Terminal entries when there's no folder.
    let mut global = vec![
        ("\u{2318}O", "Open folder\u{2026}"),
        ("\u{2325}O", "Recent folders"),
    ];
    if has_folder {
        global.insert(0, ("\u{2325}F", "Open files"));
        global.push(("\u{2325}T", "Toggle terminal"));
    }
    global.push(("\u{2318},", "Settings"));
    global.push(("\u{2318}Q", "Quit"));
    global.push(("F1", "This shortcuts list"));

    let mut groups: Vec<(&'static str, Vec<(&'static str, &'static str)>)> =
        vec![("Global", global)];

    // The editor, file dialog, terminal and find bar all need an open folder.
    if has_folder {
        groups.extend(folder_shortcut_groups());
    }
    groups.push((
        "Settings (\u{2318},)",
        vec![
            ("\u{2191} \u{2193}  Tab", "Switch section"),
            ("\u{2190} \u{2192}  + \u{2212}", "Adjust value"),
            ("Esc / \u{21b5}", "Close"),
        ],
    ));
    groups.push((
        "Recent folders (\u{2325}O)",
        vec![
            ("\u{2191} \u{2193}", "Navigate"),
            ("Space", "Toggle selection"),
            ("\u{21b5}", "Open"),
        ],
    ));
    groups
}

/// The shortcut sections that only make sense once a folder is open.
fn folder_shortcut_groups() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    vec![
        (
            "Editor \u{2014} files & tabs",
            vec![
                ("\u{2318}S  \u{2318}\u{21e7}S", "Save \u{b7} Save all"),
                ("\u{2318}W  \u{2318}\u{21e7}W", "Close tab \u{b7} Close all"),
                ("\u{2318}\u{21e7}T", "Reopen closed tab"),
                ("\u{2318}F", "Find in file"),
                ("\u{2318}\\", "Split / unsplit view"),
                ("\u{2303}Tab  \u{2303}\u{21e7}Tab", "Next \u{b7} previous tab"),
                ("\u{2303}<  \u{2303}>", "Move tab left \u{b7} right"),
                ("\u{2318}C  \u{2318}X  \u{2318}V", "Copy \u{b7} cut \u{b7} paste"),
                ("\u{2318}Z  \u{2318}\u{21e7}Z", "Undo \u{b7} redo"),
                ("\u{2318}A", "Select all"),
                ("\u{2318}\u{21e7}C  \u{2318}\u{21e7}R", "Copy path \u{b7} relative path"),
            ],
        ),
        (
            "Editor \u{2014} cursor & selection",
            vec![
                ("\u{2190} \u{2192} \u{2191} \u{2193}", "Move the cursor"),
                ("\u{21e7} + any move below", "Extend the selection (mark text)"),
                ("Home  End", "Line start \u{b7} end"),
                ("\u{2318}\u{2190}  \u{2318}\u{2192}", "Line start \u{b7} end"),
                ("\u{2325}\u{2190}  \u{2325}\u{2192}", "Word left \u{b7} right"),
                ("\u{2318}\u{2191}  \u{2318}\u{2193}", "Document start \u{b7} end"),
                ("\u{2318}A", "Select all"),
                ("\u{2325}\u{232b}  \u{2318}\u{232b}", "Delete word \u{b7} to line start"),
                ("Tab  \u{21e7}Tab", "Indent \u{b7} outdent selection"),
                ("\u{2318}D", "Add caret at next match (multi-cursor)"),
                ("\u{2318}\u{2325}\u{2191}  \u{2318}\u{2325}\u{2193}", "Add caret above \u{b7} below"),
                ("Esc", "Collapse to one caret"),
            ],
        ),
        (
            "Files dialog (\u{2325}F)",
            vec![
                ("\u{2191} \u{2193}", "Navigate"),
                ("\u{2192}  \u{2190}", "Expand \u{b7} collapse folder"),
                ("\u{21b5}", "Open file / fold folder"),
                ("Tab  \u{21e7}Tab", "Search into \u{b7} out of folder"),
                ("\u{2318}N", "New file"),
                ("\u{21e7}\u{2325}N", "New folder"),
                ("\u{2318}R", "Rename"),
                ("\u{2318}D", "Delete"),
                ("\u{2318}\u{21e7}C  \u{2318}\u{21e7}R", "Copy path \u{b7} relative path"),
                ("\u{2325}H", "Search: show / hide node_modules, build\u{2026}"),
                ("\u{232b}", "Delete query / out of folder"),
                ("type\u{2026}", "Search files"),
            ],
        ),
        (
            "Terminal (\u{2325}T)",
            vec![
                ("\u{2303}Tab  \u{2303}\u{21e7}Tab", "Next \u{b7} previous terminal"),
                ("\u{2325}N", "New terminal"),
                ("\u{2325}W  \u{2318}W", "Close terminal"),
                ("\u{2325}G", "Grid layout"),
                ("\u{2303}\u{21e7}\u{2190} / \u{2192}", "Move terminal left \u{b7} right"),
                ("\u{2325}\u{2190}\u{2192}  \u{2303}\u{2190}\u{2192}", "Shell cursor by word"),
                ("\u{2318}\u{2190}  \u{2318}\u{2192}", "Cursor to line start \u{b7} end"),
                ("\u{2325}\u{232b}  \u{2318}\u{232b}", "Delete word \u{b7} to line start"),
                ("\u{2325}\u{2191}  \u{2325}\u{2193}", "Copy mode (free cursor + select)"),
                ("\u{2318}C  \u{2318}V", "Copy selection \u{b7} paste"),
                ("\u{21e7}PgUp/Dn  fn\u{2191}/\u{2193}", "Scroll history"),
                ("\u{21e7}\u{2190} \u{2191} \u{2192} \u{2193}", "Mark text (\u{21e7}\u{2325} by word)"),
                ("Drag \u{b7} \u{21e7}Click", "Select w/ mouse (\u{21e7}Click extends)"),
                ("\u{2325}\u{21b5}  \u{21e7}\u{21b5}", "Soft newline (don't submit)"),
            ],
        ),
        (
            "Terminal copy mode (\u{2325}\u{2191})",
            vec![
                ("\u{2190}\u{2191}\u{2193}\u{2192} / hjkl", "Move cursor"),
                ("\u{21e7} + arrows", "Mark / extend selection"),
                ("\u{2325}\u{2190}  \u{2325}\u{2192}", "Move by word"),
                ("\u{21b5} / y / \u{2318}C", "Copy & exit"),
                ("Esc / q", "Exit copy mode"),
            ],
        ),
        (
            "Find bar (\u{2318}F)",
            vec![
                ("\u{21b5}  \u{2193}", "Next match"),
                ("\u{21e7}\u{21b5}  \u{2191}", "Previous match"),
                ("Esc", "Close find"),
            ],
        ),
    ]
}

/// The F1 cheat-sheet: every shortcut, grouped by where it works, in a scrollable
/// panel with the key column aligned so rows are spaced cleanly across the width.
fn render_help(f: &mut Frame, area: Rect, app: &mut App, below: usize) {
    let t = app.theme.clone();
    let rect = dialog_rect_stacked(area, below);
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
            "\u{2318} (Command) works anywhere Ctrl does.",
            Style::default().fg(t.fg_dim),
        )))
        .style(Style::default().bg(t.bg_dark)),
        parts[0],
    );
    render_divider(f, parts[1], &t);

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
        Paragraph::new(justified_actions(&foot, parts[4].width, &t))
            .style(Style::default().bg(t.bg_dark)),
        parts[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::fs;

    #[test]
    fn justified_actions_span_full_width() {
        let t = Theme::default();
        let items = [("\u{21b5}", "Open"), ("^N", "New"), ("Esc", "Close")];
        let line = justified_actions(&items, 60, &t);
        let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(w, 60, "the row fills the full width");
        // First glyph is the first key, last glyph the last label — flush edges.
        assert!(line.spans.first().unwrap().content.starts_with('\u{21b5}'));
        assert!(line.spans.last().unwrap().content.ends_with("Close"));
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
            "saved",
        ] {
            assert!(text.contains(s), "footer should mention {s:?}");
        }
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
        assert!(text.contains("Oxru"));
        assert!(text.contains("to open files"));

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
        let top = dialog_rect_stacked(area, 0);
        let b1 = dialog_rect_stacked(area, 1);
        let b2 = dialog_rect_stacked(area, 2);
        // Every dialog is the same 90% size.
        assert_eq!((top.width, top.height), (90, 90));
        assert_eq!((b1.width, b1.height), (90, 90));
        // The focused (top) one is centered; lower ones are nudged down-right.
        assert_eq!((top.x, top.y), (5, 5));
        assert!(b1.x > top.x && b1.y > top.y);
        assert!(b2.x > b1.x && b2.y > b1.y);
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
    fn editor_draws_caret() {
        use ratatui::style::Color;
        let dir = workspace();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&dir.path().join("src/main.rs"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let content = terminal.backend().buffer().content();
        // The caret is a thin vertical bar (`▏`) drawn in the accent colour. The
        // first frame is always solid (the blink resets on the just-moved caret).
        assert!(
            content
                .iter()
                .any(|c| c.symbol() == "\u{258f}" && c.fg == Color::Rgb(0x4c, 0xaf, 0x50)),
            "expected a visible thin caret bar"
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
}
