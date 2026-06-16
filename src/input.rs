//! Translate key events into actions. The UI is minimal: a prompt and the file
//! dialog capture input while open, **Ctrl+F** opens the dialog, and an open
//! file accepts basic editing.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::prompt::PromptKind;

pub fn handle_key(app: &mut App, key: KeyEvent) {
    use crate::app::Dialog;
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // On macOS the window uses Command (⌘ / Super) for shortcuts; treat it as
    // Ctrl so every Ctrl binding below also fires on ⌘ — a native Mac feel while
    // the terminal-only build keeps working on Ctrl. (The embedded terminal reads
    // its own raw modifiers, so this fold doesn't reach the shell.)
    let ctrl =
        key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::SUPER);

    // The name prompt / delete confirmation is always top-most and modal.
    if app.prompt.active {
        handle_prompt(app, key, ctrl);
        return;
    }

    // The shortcuts cheat-sheet (F1) is reachable from anywhere.
    if key.code == KeyCode::F(1) {
        app.open_help();
        return;
    }

    // App-level "open a dialog" chords work from ANY context — the editor, the
    // terminal, or on top of another dialog — so every dialog is reachable from
    // anywhere. Re-pressing one that's open just raises it (no dup).
    if alt {
        match key.code {
            KeyCode::Char('t') | KeyCode::Char('T') => {
                app.toggle_terminal_modal();
                return;
            }
            KeyCode::Char('f') | KeyCode::Char('F') => {
                app.open_file_dialog();
                return;
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                app.open_recent_dialog();
                return;
            }
            _ => {}
        }
    }
    // Settings (⌘, / Ctrl+,), Quit (⌘Q / Ctrl+Q), and Open Folder (⌘O / Ctrl+O)
    // — all reachable from anywhere. Open Folder pops the native picker, the only
    // way to open a project that isn't already in the recents list.
    if ctrl && !shift {
        match key.code {
            KeyCode::Char(',') => {
                app.open_settings();
                return;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                app.request_quit();
                return;
            }
            KeyCode::Char('o') | KeyCode::Char('O') => {
                app.request_open_folder();
                return;
            }
            _ => {}
        }
    }

    // The in-file find bar (Ctrl+F) captures keys while it's open and no dialog
    // is on top of it.
    if app.find.active && app.top_dialog().is_none() {
        handle_find(app, key, ctrl, alt, shift);
        return;
    }

    // Everything else goes to whichever dialog is focused (top of the stack),
    // or to the editor when no dialog is open.
    match app.top_dialog() {
        Some(Dialog::Files) => handle_file_dialog(app, key, ctrl, alt, shift),
        Some(Dialog::Recent) => handle_recent_dialog(app, key),
        Some(Dialog::Settings) => handle_settings_dialog(app, key),
        Some(Dialog::Terminal) => handle_terminal_modal(app, key, alt),
        Some(Dialog::Help) => handle_help_dialog(app, key),
        None => handle_editor_context(app, key, ctrl, alt, shift),
    }
}

/// The shortcuts cheat-sheet: scroll or dismiss.
fn handle_help_dialog(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) => app.close_top_dialog(),
        KeyCode::Up => app.help_scroll(-1),
        KeyCode::Down => app.help_scroll(1),
        KeyCode::PageUp => app.help_scroll(-10),
        KeyCode::PageDown => app.help_scroll(10),
        _ => {}
    }
}

/// Apply a cursor move to the active buffer, extending the selection when Shift
/// is held and dropping it otherwise.
fn move_cursor(app: &mut App, shift: bool, f: impl FnOnce(&mut crate::buffer::Buffer)) {
    if let Some(b) = app.active_buffer() {
        if shift {
            b.begin_selection();
        } else {
            b.clear_selection();
        }
        f(b);
    }
}

/// Keys while the find bar is open: type to refine, Enter/Down = next match,
/// Shift+Enter/Up = previous, Esc closes.
fn handle_find(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;
    // Borrow the three fields together for the in-place cursor moves (no text
    // change, so no re-search needed).
    macro_rules! q {
        () => {
            (&app.find.query, &mut app.find.cursor, &mut app.find.anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.find_close(),
        // Enter / Up / Down navigate matches (Shift+Enter = previous).
        KeyCode::Enter => {
            if shift {
                app.find_prev()
            } else {
                app.find_next()
            }
        }
        KeyCode::Down => app.find_next(),
        KeyCode::Up => app.find_prev(),
        // Select-all / copy / cut / paste (⌘ or Ctrl).
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            el::select_all(&app.find.query, &mut app.find.cursor, &mut app.find.anchor)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => app.find_copy(),
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => app.find_cut(),
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.find_paste(),
        // Cursor motion: Option+Arrow by word, ⌘/Ctrl+Arrow to the ends, plain
        // Left/Right/Home/End by char — Shift extends the selection throughout.
        KeyCode::Left if alt => { let (t, c, a) = q!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = q!(); el::word_right(t, c, a, shift) }
        KeyCode::Left if ctrl => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::Right if ctrl => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Left => { let (t, c, a) = q!(); el::left(t, c, a, shift) }
        KeyCode::Right => { let (t, c, a) = q!(); el::right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        // Editing.
        KeyCode::Backspace if alt => {
            el::delete_word_left(&mut app.find.query, &mut app.find.cursor, &mut app.find.anchor);
            app.find_changed();
        }
        KeyCode::Backspace => app.find_backspace(),
        KeyCode::Delete => {
            el::delete_fwd(&mut app.find.query, &mut app.find.cursor, &mut app.find.anchor);
            app.find_changed();
        }
        // Plain typed characters refine the query; ignore chord combinations.
        KeyCode::Char(c) if !ctrl && !alt => app.find_input(c),
        _ => {}
    }
}

/// The name prompt / delete confirmation / save-before-close / quit choices.
fn handle_prompt(app: &mut App, key: KeyEvent, ctrl: bool) {
    match app.prompt.kind() {
        Some(PromptKind::CloseUnsaved) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    app.close_unsaved_answer(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => app.close_unsaved_answer(false),
                KeyCode::Esc => app.cancel_close_all(),
                _ => {}
            }
            return;
        }
        // Single tab close: Y save / N don't save / Esc cancel.
        Some(PromptKind::CloseTab) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    app.close_tab_answer(true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => app.close_tab_answer(false),
                KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('C') => app.prompt.close(),
                _ => {}
            }
            return;
        }
        // Quit flow: unsaved file — Y save / N don't save / Esc cancel.
        Some(PromptKind::QuitUnsaved) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.quit_save_current(),
                KeyCode::Char('n') | KeyCode::Char('N') => app.quit_skip_current(),
                KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('C') => app.cancel_quit(),
                _ => {}
            }
            return;
        }
        // External change with unsaved edits: R reload / K keep mine / Esc keep.
        Some(PromptKind::ExternalChange) => {
            match key.code {
                KeyCode::Char('r') | KeyCode::Char('R') => app.external_change_answer(true),
                KeyCode::Char('k') | KeyCode::Char('K') | KeyCode::Esc => {
                    app.external_change_answer(false)
                }
                _ => {}
            }
            return;
        }
        // Quit flow: running terminal — close & quit / cancel.
        Some(PromptKind::QuitTerminal) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.quit_skip_current(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.cancel_quit(),
                _ => {}
            }
            return;
        }
        _ => {}
    }
    use crate::editline as el;
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    macro_rules! p {
        () => {
            (&app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.prompt.close(),
        KeyCode::Enter => app.confirm_prompt(),
        // The remaining keys edit the name field — only the input prompts have one.
        _ if !app.prompt.needs_input() => {}
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            el::select_all(&app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => app.prompt_copy(),
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => app.prompt_cut(),
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.prompt_paste(),
        // Cursor motion: Option+Arrow by word, ⌘/Ctrl+Arrow to the ends, plain
        // arrows by char — Shift extends the selection.
        KeyCode::Left if alt => { let (t, c, a) = p!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = p!(); el::word_right(t, c, a, shift) }
        KeyCode::Left if ctrl => { let (t, c, a) = p!(); el::home(t, c, a, shift) }
        KeyCode::Right if ctrl => { let (t, c, a) = p!(); el::end(t, c, a, shift) }
        KeyCode::Left => { let (t, c, a) = p!(); el::left(t, c, a, shift) }
        KeyCode::Right => { let (t, c, a) = p!(); el::right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = p!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = p!(); el::end(t, c, a, shift) }
        KeyCode::Backspace if alt => {
            el::delete_word_left(&mut app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor)
        }
        KeyCode::Backspace => {
            el::backspace(&mut app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor)
        }
        KeyCode::Delete => {
            el::delete_fwd(&mut app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor)
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            el::insert(&mut app.prompt.input, &mut app.prompt.cursor, &mut app.prompt.anchor, c)
        }
        _ => {}
    }
}

/// The fuzzy file search / browse tree, plus Ctrl/⌘ file-management actions.
fn handle_file_dialog(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;
    macro_rules! q {
        () => {
            (&app.file_dialog.query, &mut app.file_dialog.cursor, &mut app.file_dialog.anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.dialog_up(),
        KeyCode::Down => app.dialog_down(),
        // Plain Left/Right drive the tree; Option+Arrow moves the search cursor by
        // word, Home/End jump its ends (Shift extends the selection).
        KeyCode::Left if alt => { let (t, c, a) = q!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = q!(); el::word_right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Right => app.dialog_tree_expand(),
        KeyCode::Left => app.dialog_tree_collapse(),
        KeyCode::Enter => app.dialog_open_selected(),
        // Tab: in the tree, scope the search into the highlighted folder; in the
        // search list, tick the current file and advance (fzf-style). Shift+Tab
        // is the mirror — drill back out of the folder (tick-and-up in search).
        // In GUI mode Shift+Tab arrives as Tab+Shift (not BackTab), so match both.
        KeyCode::BackTab => app.dialog_backtab(),
        KeyCode::Tab if shift => app.dialog_backtab(),
        KeyCode::Tab => app.dialog_tab(),
        // Ctrl/⌘+Shift+C/R copy the selected result's full / relative path. These
        // must precede the plain shortcuts below so they win on Shift.
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl && shift => app.copy_dialog_path(false),
        KeyCode::Char('r') | KeyCode::Char('R') if ctrl && shift => app.copy_dialog_path(true),
        // ⌥H toggles whether search includes the heavy build / dep dirs
        // (node_modules, build, …). They stay browsable in the tree; this only
        // changes the search corpus. (⌥ avoids ⌘H, which macOS uses to hide the
        // app, and the Ctrl+H/I/J terminal control-code clashes.)
        KeyCode::Char('h') | KeyCode::Char('H') if alt => app.dialog_toggle_junk(),
        // New folder is Shift+Option+N (New file stays ⌘N / Ctrl+N).
        KeyCode::Char('n') | KeyCode::Char('N') if alt && shift => app.dialog_new_folder(),
        KeyCode::Char('n') if ctrl => app.dialog_new_file(),
        KeyCode::Char('d') if ctrl => app.dialog_delete(),
        KeyCode::Char('r') if ctrl => app.dialog_rename(),
        // Query-box select-all / copy / cut / paste (⌘ or Ctrl).
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            el::select_all(&app.file_dialog.query, &mut app.file_dialog.cursor, &mut app.file_dialog.anchor)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => app.dialog_copy_text(),
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => app.dialog_cut_text(),
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.dialog_paste_text(),
        KeyCode::Backspace if alt => {
            el::delete_word_left(&mut app.file_dialog.query, &mut app.file_dialog.cursor, &mut app.file_dialog.anchor);
            app.dialog_query_changed();
        }
        KeyCode::Backspace => app.dialog_backspace(),
        KeyCode::Delete => {
            el::delete_fwd(&mut app.file_dialog.query, &mut app.file_dialog.cursor, &mut app.file_dialog.anchor);
            app.dialog_query_changed();
        }
        KeyCode::Char(c) if !ctrl => app.dialog_input(c),
        _ => {}
    }
}

/// The "Recent folders" dialog: multi-select, then open each in a window.
fn handle_recent_dialog(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.recent_move(-1),
        KeyCode::Down => app.recent_move(1),
        KeyCode::Char(' ') => app.recent_toggle(),
        KeyCode::Enter => app.recent_open_selected(),
        _ => {}
    }
}

/// The settings dialog. Esc/Enter persists and closes (handled in teardown).
fn handle_settings_dialog(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.close_top_dialog(),
        KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab => app.settings_toggle_focus(),
        KeyCode::Left => app.settings_adjust(-1),
        KeyCode::Right => app.settings_adjust(1),
        KeyCode::Char('-') | KeyCode::Char('_') => app.settings_adjust(-1),
        KeyCode::Char('+') | KeyCode::Char('=') => app.settings_adjust(1),
        _ => {}
    }
}

/// No dialog open: editor Control/⌘ shortcuts and basic editing.
fn handle_editor_context(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    // Esc collapses multiple carets back to one (no-op otherwise).
    if key.code == KeyCode::Esc {
        app.clear_editor_carets();
        return;
    }
    if ctrl {
        match key.code {
            // ⌘⌥↑/↓ add a caret above / below for column editing (checked before
            // the plain ⌘↑/↓ document-jump below).
            KeyCode::Up if alt => app.add_caret_above(),
            KeyCode::Down if alt => app.add_caret_below(),
            // ⌘D selects the next occurrence of the word/selection, multi-cursor.
            KeyCode::Char('d') | KeyCode::Char('D') => app.add_next_occurrence(),
            // ⌘/Ctrl navigation: line start/end, document start/end, and
            // delete-to-line-start (⇧ extends the selection on the moves).
            KeyCode::Left => move_cursor(app, shift, |b| b.move_home()),
            KeyCode::Right => move_cursor(app, shift, |b| b.move_end()),
            KeyCode::Up => move_cursor(app, shift, |b| b.move_doc_start()),
            KeyCode::Down => move_cursor(app, shift, |b| b.move_doc_end()),
            KeyCode::Backspace => {
                if let Some(b) = app.active_buffer() {
                    b.delete_to_line_start();
                }
            }
            // Shift makes save/close affect *all* files.
            KeyCode::Char('s') | KeyCode::Char('S') if shift => app.save_all(),
            KeyCode::Char('s') | KeyCode::Char('S') => app.save_active(),
            KeyCode::Char('w') | KeyCode::Char('W') if shift => app.close_all(),
            KeyCode::Char('w') | KeyCode::Char('W') => app.close_tab(),
            // Find a word in the current file (case-insensitive).
            KeyCode::Char('f') | KeyCode::Char('F') => app.find_open(),
            // Reopen the most-recently-closed tab.
            KeyCode::Char('t') | KeyCode::Char('T') if shift => app.reopen_closed_tab(),
            // Toggle the split / grid view (tile all open files).
            KeyCode::Char('\\') => app.toggle_editor_grid(),
            KeyCode::Char('a') | KeyCode::Char('A') => app.select_all(),
            // Ctrl+Shift+C/R copy the open file's full / relative path.
            KeyCode::Char('c') | KeyCode::Char('C') if shift => app.copy_active_path(false),
            KeyCode::Char('r') | KeyCode::Char('R') if shift => app.copy_active_path(true),
            KeyCode::Char('c') | KeyCode::Char('C') => app.copy_selection(),
            KeyCode::Char('x') | KeyCode::Char('X') => app.cut_selection(),
            KeyCode::Char('v') | KeyCode::Char('V') => app.paste(),
            // Undo / redo. Ctrl+Z undo, Ctrl+Shift+Z or Ctrl+Y redo.
            KeyCode::Char('z') | KeyCode::Char('Z') if shift => app.redo(),
            KeyCode::Char('z') | KeyCode::Char('Z') => app.undo(),
            KeyCode::Char('y') | KeyCode::Char('Y') => app.redo(),
            // Ctrl+Shift+,/. (i.e. < / >) move the active tab.
            KeyCode::Char('<') => app.move_tab_left(),
            KeyCode::Char('>') => app.move_tab_right(),
            KeyCode::Char(',') if shift => app.move_tab_left(),
            KeyCode::Char('.') if shift => app.move_tab_right(),
            // Ctrl+Tab next tab, Ctrl+Shift+Tab previous.
            KeyCode::Tab if shift => app.prev_tab(),
            KeyCode::Tab => app.next_tab(),
            KeyCode::BackTab => app.prev_tab(),
            _ => {} // swallow other Ctrl combos (don't type them)
        }
        return;
    }

    // A file open in the editor accepts basic editing.
    if app.active_editor.is_some() {
        handle_editor(app, key, ctrl, alt, shift);
    }
}

/// Inside the terminal modal: Alt+letter/digit manages terminals, everything
/// else is forwarded to the focused shell.
fn handle_terminal_modal(app: &mut App, key: KeyEvent, alt: bool) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let sup = key.modifiers.contains(KeyModifiers::SUPER);

    // Copy mode owns a free cursor for selecting text — keys drive it instead of
    // reaching the shell, until Esc/copy leaves the mode.
    if app.terminal_copy_mode() {
        handle_terminal_copy_mode(app, key, ctrl, shift, sup, alt);
        return;
    }

    // Ctrl+Tab / Ctrl+Shift+Tab switch terminals (same as editor tabs).
    if ctrl && matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        if key.code == KeyCode::BackTab || shift {
            app.prev_terminal();
        } else {
            app.next_terminal();
        }
        return;
    }

    // Move the active terminal: Ctrl+Shift+←/→ or Ctrl+Shift+,/. (i.e. < / >),
    // mirroring the editor's move-tab chords.
    if ctrl {
        match key.code {
            KeyCode::Left if shift => {
                app.move_terminal_left();
                return;
            }
            KeyCode::Right if shift => {
                app.move_terminal_right();
                return;
            }
            KeyCode::Char('<') => {
                app.move_terminal_left();
                return;
            }
            KeyCode::Char('>') => {
                app.move_terminal_right();
                return;
            }
            KeyCode::Char(',') if shift => {
                app.move_terminal_left();
                return;
            }
            KeyCode::Char('.') if shift => {
                app.move_terminal_right();
                return;
            }
            // Ctrl+←/→ jump the shell cursor by word — same as Option+←/→
            // (readline meta-b / meta-f), matching iTerm2 / VS Code.
            KeyCode::Left => {
                if let Some(term) = app.active_terminal_mut() {
                    term.clear_selection();
                    term.scroll_to_bottom();
                    term.send_input(&[0x1b, b'b']);
                }
                return;
            }
            KeyCode::Right => {
                if let Some(term) = app.active_terminal_mut() {
                    term.clear_selection();
                    term.scroll_to_bottom();
                    term.send_input(&[0x1b, b'f']);
                }
                return;
            }
            _ => {}
        }
    }

    // Copy the mouse selection: Cmd+C (macOS) or Ctrl+Shift+C.
    if (sup || (ctrl && shift)) && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        app.copy_terminal_selection();
        return;
    }

    // Paste the clipboard: Cmd+V (macOS) or Ctrl+Shift+V.
    if (sup || (ctrl && shift)) && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) {
        app.paste_terminal();
        return;
    }

    // Cmd+W closes the focused terminal (mirrors ⌥W). Uses ⌘, which the shell
    // never needs, so it won't clobber a running program.
    if sup && matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W')) {
        app.close_terminal();
        return;
    }

    // macOS line-editing chords on Command, mapped to the shell's readline
    // controls so they work in any shell:
    //   Cmd+←  -> start of line (Ctrl+A)   Cmd+→  -> end of line (Ctrl+E)
    //   Cmd+Backspace -> delete to start of line (Ctrl+U)
    if sup {
        let ctl: Option<&[u8]> = match key.code {
            KeyCode::Left => Some(&[0x01]),      // Ctrl+A
            KeyCode::Right => Some(&[0x05]),     // Ctrl+E
            KeyCode::Backspace => Some(&[0x15]), // Ctrl+U
            _ => None,
        };
        if let Some(seq) = ctl {
            if let Some(term) = app.active_terminal_mut() {
                term.clear_selection();
                term.scroll_to_bottom();
                term.send_input(seq);
            }
            return;
        }
    }

    // Shift+PageUp/Down scroll the scrollback by a page; Shift+arrows mark text
    // for copying (Cmd+C / Ctrl+Shift+C). Neither disturbs the running program.
    if shift {
        match key.code {
            KeyCode::PageUp => {
                app.terminal_scroll_page(1);
                return;
            }
            KeyCode::PageDown => {
                app.terminal_scroll_page(-1);
                return;
            }
            KeyCode::Up => {
                app.terminal_select(-1, 0);
                return;
            }
            KeyCode::Down => {
                app.terminal_select(1, 0);
                return;
            }
            // Shift+Option+←/→ extends the mark by a whole word.
            KeyCode::Left => {
                if alt {
                    app.terminal_select_word(-1);
                } else {
                    app.terminal_select(0, -1);
                }
                return;
            }
            KeyCode::Right => {
                if alt {
                    app.terminal_select_word(1);
                } else {
                    app.terminal_select(0, 1);
                }
                return;
            }
            _ => {}
        }
    }

    if alt {
        match key.code {
            // Option+←/→ jump the shell cursor by word (readline meta-b/meta-f).
            KeyCode::Left => {
                if let Some(term) = app.active_terminal_mut() {
                    term.clear_selection();
                    term.scroll_to_bottom();
                    term.send_input(&[0x1b, b'b']);
                }
                return;
            }
            KeyCode::Right => {
                if let Some(term) = app.active_terminal_mut() {
                    term.clear_selection();
                    term.scroll_to_bottom();
                    term.send_input(&[0x1b, b'f']);
                }
                return;
            }
            // Option+Up/Down enter copy mode (a free cursor to select & copy from
            // the screen, instead of the shell seeing arrow keys / history).
            KeyCode::Up => {
                app.terminal_enter_copy_mode();
                app.terminal_copy_move(-1, 0, false);
                return;
            }
            KeyCode::Down => {
                app.terminal_enter_copy_mode();
                app.terminal_copy_move(1, 0, false);
                return;
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                app.toggle_terminal_modal();
                return;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                app.new_terminal();
                return;
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                app.close_terminal();
                return;
            }
            KeyCode::Char('g') | KeyCode::Char('G') => {
                app.toggle_terminal_grid();
                return;
            }
            _ => {} // other Alt combos fall through to the shell
        }
    }

    // fn+↑ / fn+↓ arrive as plain PageUp / PageDown: scroll our scrollback at
    // the shell (like macOS Terminal), but hand the keys to a full-screen
    // program (vim, less, htop) that owns the alternate screen.
    if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
        let on_alt = app
            .active_terminal_mut()
            .map(|t| t.on_alternate_screen())
            .unwrap_or(false);
        if !on_alt {
            app.terminal_scroll_page(if key.code == KeyCode::PageUp { 1 } else { -1 });
            return;
        }
    }

    if let Some(bytes) = key_to_bytes(&key) {
        if let Some(term) = app.active_terminal_mut() {
            // Typing (or plain cursor keys) drops any mark and snaps the view back
            // to the live bottom, like every terminal.
            term.clear_selection();
            term.scroll_to_bottom();
            term.send_input(&bytes);
        }
    }
}

/// Terminal copy mode: a free cursor over the screen for selecting text, with no
/// keys reaching the shell. Arrows / hjkl move; Space or `v` toggles the
/// selection; Enter / `y` / ⌘C copy and exit; Esc / `q` exit.
fn handle_terminal_copy_mode(
    app: &mut App,
    key: KeyEvent,
    ctrl: bool,
    shift: bool,
    sup: bool,
    alt: bool,
) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => app.terminal_exit_copy_mode(),
        // Copy the marked text and leave: Enter, y, ⌘C, or Ctrl+Shift+C.
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => app.terminal_copy_and_exit(),
        KeyCode::Char('c') | KeyCode::Char('C') if sup || (ctrl && shift) => {
            app.terminal_copy_and_exit()
        }
        // Option+arrow jumps by word (Shift marks while moving).
        KeyCode::Left if alt => app.terminal_copy_move_word(-1, shift),
        KeyCode::Right if alt => app.terminal_copy_move_word(1, shift),
        // Move the cursor (arrows or vim h/j/k/l). Shift+arrow marks/extends the
        // selection as it moves; a plain move drops the mark.
        KeyCode::Left | KeyCode::Char('h') => app.terminal_copy_move(0, -1, shift),
        KeyCode::Right | KeyCode::Char('l') => app.terminal_copy_move(0, 1, shift),
        KeyCode::Up | KeyCode::Char('k') => app.terminal_copy_move(-1, 0, shift),
        KeyCode::Down | KeyCode::Char('j') => app.terminal_copy_move(1, 0, shift),
        // Scroll the view through scrollback while selecting.
        KeyCode::PageUp => app.terminal_scroll_page(1),
        KeyCode::PageDown => app.terminal_scroll_page(-1),
        _ => {} // swallow everything else — copy mode never leaks to the shell
    }
}

/// Encode a key event as the byte sequence a PTY expects.
fn key_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Newline shortcuts that CLIs (Claude Code, REPLs, …) use to insert a soft
    // newline instead of submitting:
    //   Option/Alt+Enter -> ESC + CR  (what "Option as Meta" terminals send)
    //   Shift+Enter       -> LF        (the other common soft-return)
    // Plain Enter still submits with CR. The shell accepts all of these.
    if key.code == KeyCode::Enter {
        if alt {
            return Some(vec![0x1b, b'\r']);
        }
        if shift {
            return Some(vec![b'\n']);
        }
    }

    // Option+Backspace deletes the previous word (readline meta-DEL: ESC + DEL),
    // matching macOS Terminal. Plain Backspace stays a single DEL below.
    if key.code == KeyCode::Backspace && alt {
        return Some(vec![0x1b, 0x7f]);
    }

    let mut bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl && c.is_ascii_alphabetic() {
                vec![(c.to_ascii_lowercase() as u8) - b'a' + 1]
            } else {
                let mut b = [0u8; 4];
                c.encode_utf8(&mut b).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        _ => return None,
    };
    // Alt on a plain character is sent as an ESC prefix (Meta).
    if alt && !ctrl && matches!(key.code, KeyCode::Char(_)) {
        let mut v = vec![0x1b];
        v.append(&mut bytes);
        return Some(v);
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_letter_is_control_code() {
        let ev = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_to_bytes(&ev), Some(vec![3]));
    }

    #[test]
    fn enter_newline_variants() {
        // Plain Enter submits (CR).
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(&plain), Some(vec![b'\r']));
        // Option/Alt+Enter -> ESC + CR (soft newline in CLIs like Claude Code).
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(key_to_bytes(&alt), Some(vec![0x1b, b'\r']));
        // Shift+Enter -> LF.
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(key_to_bytes(&shift), Some(vec![b'\n']));
    }

    #[test]
    fn arrows_are_escape_sequences() {
        let ev = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(&ev), Some(vec![0x1b, b'[', b'A']));
    }

    #[test]
    fn option_backspace_deletes_word() {
        // Option+Backspace -> ESC + DEL (readline backward-kill-word).
        let ev = KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(key_to_bytes(&ev), Some(vec![0x1b, 0x7f]));
        // Plain Backspace is still a single DEL.
        let plain = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(key_to_bytes(&plain), Some(vec![0x7f]));
    }

    /// An App with one embedded terminal open + focused, for driving real key
    /// chords through `handle_key` and asserting the exact bytes sent to the shell.
    fn terminal_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // opens the terminal modal + spawns a shell
        (dir, app)
    }

    /// Drain whatever the active terminal has sent to its shell since last call.
    fn sent(app: &mut App) -> Vec<u8> {
        let i = app.active_terminal;
        app.terminals[i].take_sent()
    }

    #[test]
    fn terminal_word_move_chords() {
        let (_d, mut app) = terminal_app();
        let _ = sent(&mut app); // ignore any startup noise
        // Option+←/→ and Ctrl+←/→ all jump by word: ESC b / ESC f.
        handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(sent(&mut app), vec![0x1b, b'b']);
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(sent(&mut app), vec![0x1b, b'f']);
        handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(sent(&mut app), vec![0x1b, b'b']);
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(sent(&mut app), vec![0x1b, b'f']);
    }

    #[test]
    fn terminal_line_edit_chords() {
        let (_d, mut app) = terminal_app();
        let _ = sent(&mut app);
        // Cmd+← / Cmd+→ -> line start / end (Ctrl+A / Ctrl+E).
        handle_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER));
        assert_eq!(sent(&mut app), vec![0x01]);
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER));
        assert_eq!(sent(&mut app), vec![0x05]);
        // Cmd+Backspace -> delete to line start (Ctrl+U).
        handle_key(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER));
        assert_eq!(sent(&mut app), vec![0x15]);
        // Option+Backspace -> delete word (ESC DEL).
        handle_key(&mut app, KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(sent(&mut app), vec![0x1b, 0x7f]);
    }

    #[test]
    fn terminal_plain_pageup_scrolls_not_sent() {
        let (_d, mut app) = terminal_app();
        let _ = sent(&mut app);
        // On the normal screen, PageUp scrolls oxru's scrollback — it must NOT
        // reach the shell (that would page the wrong thing).
        handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert!(
            sent(&mut app).is_empty(),
            "plain PageUp must not be forwarded to the shell"
        );
    }

    #[test]
    fn alt_char_is_esc_prefixed() {
        let ev = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT);
        assert_eq!(key_to_bytes(&ev), Some(vec![0x1b, b'x']));
    }

    #[test]
    fn f1_opens_shortcuts_help() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        handle_key(&mut app, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert_eq!(app.top_dialog(), Some(crate::app::Dialog::Help));
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.top_dialog(), None);
    }

    #[test]
    fn cmd_super_acts_like_ctrl() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "hello world").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        // ⌘A (Super+A) selects all, exactly like Ctrl+A.
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SUPER));
        assert!(
            app.active_buffer().unwrap().selection().is_some(),
            "⌘A selects all via the Cmd→Ctrl fold"
        );
    }

    #[test]
    fn alt_arrow_moves_by_word() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "foo bar").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        {
            let b = app.active_buffer().unwrap();
            b.cursor = 0;
            b.goal_col = 0;
        }
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(app.active_buffer().unwrap().cursor, 3, "⌥→ jumps to end of 'foo'");
    }

    #[test]
    fn terminal_copy_mode_enter_and_exit() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // spawn a terminal + open the modal
        assert!(!app.terminal_copy_mode());

        // ⌥↑ enters copy mode (a free cursor, instead of the shell seeing arrows).
        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert!(app.terminal_copy_mode(), "⌥↑ enters copy mode");

        // Plain arrows move the cursor; Shift+arrow marks — both stay in copy mode
        // and never reach the shell.
        handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        handle_key(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT)); // mark
        assert!(app.terminal_copy_mode(), "still in copy mode while navigating");

        // Esc leaves copy mode.
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.terminal_copy_mode(), "Esc exits copy mode");
    }

    #[test]
    fn ctrl_shift_arrow_moves_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // term 0
        app.new_terminal(); // term 1, now active
        assert_eq!(app.active_terminal, 1);
        // Ctrl+Shift+Left moves the active terminal left in the strip.
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
        );
        assert_eq!(app.active_terminal, 0, "⌃⇧← moved the terminal left");
    }

    #[test]
    fn ctrl_tab_switches_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // opens modal + spawns term 1
        app.new_terminal(); // term 2, now active
        assert_eq!(app.terminals.len(), 2);
        assert_eq!(app.active_terminal, 1);

        // Ctrl+Tab cycles to the next terminal (wraps 1 -> 0).
        handle_key(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL));
        assert_eq!(app.active_terminal, 0);

        // Ctrl+Shift+Tab cycles to the previous (wraps 0 -> 1).
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
        );
        assert_eq!(app.active_terminal, 1);
    }
}

fn handle_editor(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    // Deleting a selection goes through the app so it can toast "Deleted N".
    if matches!(key.code, KeyCode::Delete | KeyCode::Backspace)
        && app.active_buffer().is_some_and(|b| b.selection().is_some())
    {
        app.delete_selection();
        return;
    }
    let Some(buf) = app.active_buffer() else {
        return;
    };
    // Option+Backspace deletes the previous word (no selection — that's above).
    if alt && key.code == KeyCode::Backspace {
        buf.delete_word_left();
        return;
    }
    // Movement keys extend the selection while Shift is held, otherwise they
    // drop any selection first.
    let is_move = matches!(
        key.code,
        KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
    );
    if is_move {
        if shift {
            buf.begin_selection();
        } else {
            buf.clear_selection();
        }
    }
    match key.code {
        KeyCode::Char(c) if !ctrl => buf.insert_char(c),
        KeyCode::Enter => buf.newline(),
        // Shift+Tab outdents; Tab indents a multi-line selection, else inserts a
        // soft tab (four spaces).
        KeyCode::Tab if shift => buf.outdent_selection(),
        KeyCode::BackTab => buf.outdent_selection(),
        KeyCode::Tab => {
            let multiline = buf
                .selection()
                .map(|(s, e)| buf.rope.char_to_line(s) != buf.rope.char_to_line(e))
                .unwrap_or(false);
            if multiline {
                buf.indent_selection();
            } else {
                buf.insert_str("    ");
            }
        }
        KeyCode::Backspace => buf.backspace(),
        KeyCode::Delete => buf.delete(),
        // Option+Arrow moves by word; plain arrows by character.
        KeyCode::Left if alt => buf.move_word_left(),
        KeyCode::Right if alt => buf.move_word_right(),
        KeyCode::Left => buf.move_left(),
        KeyCode::Right => buf.move_right(),
        KeyCode::Up => buf.move_up(),
        KeyCode::Down => buf.move_down(),
        KeyCode::Home => buf.move_home(),
        KeyCode::End => buf.move_end(),
        _ => {}
    }
}
