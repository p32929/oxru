//! Translate key events into actions. The UI is minimal: a prompt and the file
//! dialog capture input while open, **Ctrl+F** opens the dialog, and an open
//! file accepts basic editing.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::prompt::PromptKind;

/// Ctrl(⌘) / Alt gating for Oxru's own shortcuts (editor, dialogs, prompts —
/// NOT the embedded terminal, which reads its own raw modifiers so the shell
/// keeps every chord a real terminal app would get).
///
/// **The same combo does the same thing in both modes.** TUI mode used to
/// require a 3rd key on every shortcut (`Ctrl+S` → `Ctrl+Alt+S`, `Alt+T` →
/// `Alt+Shift+T`) on the theory that a bare 2-key combo could collide with
/// something the host terminal reserves. The cost of that was one editor with
/// two different keymaps: muscle memory built in the window didn't transfer to
/// the terminal, and every hint row, cheat-sheet entry and doc line had to be
/// rendered twice. One keymap is worth more than dodging the handful of combos
/// a host terminal might intercept — and where a host *does* swallow one, it
/// swallows it before Oxru ever sees the key, so a different binding here
/// wouldn't have rescued it anyway.
fn shortcut_mods(key: &KeyEvent) -> (bool, bool, bool) {
    let alt_held = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    // On macOS the window uses Command (⌘ / Super) for shortcuts; treat it as
    // Ctrl so every Ctrl binding below also fires on ⌘ — a native Mac feel while
    // the terminal-only build keeps working on Ctrl. (The embedded terminal reads
    // its own raw modifiers, so this fold doesn't reach the shell.)
    let ctrl_held =
        key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::SUPER);
    // The two families are mutually exclusive: holding both means neither
    // fires. Without this, `Ctrl+Alt+A` would still reach the plain `Ctrl+A`
    // arm — a second combo for the same action, which is exactly what the
    // old per-mode keymap left behind. The one binding that genuinely wants
    // both modifiers (add-caret) is matched on the raw key state in
    // [`handle_key`], before this ever applies.
    (ctrl_held && !alt_held, alt_held && !ctrl_held, shift)
}

pub fn handle_key(app: &mut App, key: KeyEvent) {
    use crate::app::Dialog;
    let (ctrl, alt, shift) = shortcut_mods(&key);

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

    // Add-caret is the only binding that deliberately holds Ctrl *and* Alt, so
    // it reads the raw key state — `shortcut_mods` has already cancelled both
    // families out by this point. Everything else in that shape is swallowed
    // rather than allowed to fall through: `Ctrl+Alt+A` must do nothing at all,
    // not select-all (the old alias) and not type an "a".
    let raw_ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
        || key.modifiers.contains(KeyModifiers::SUPER);
    let raw_alt = key.modifiers.contains(KeyModifiers::ALT);
    if raw_ctrl && raw_alt && !matches!(app.top_dialog(), Some(Dialog::Terminal)) {
        if app.top_dialog().is_none() && !app.find.active {
            match key.code {
                KeyCode::Up => {
                    app.add_caret_above();
                    return;
                }
                KeyCode::Down => {
                    app.add_caret_below();
                    return;
                }
                _ => {}
            }
        }
        return;
    }

    // App-level chords that work from ANY context — the editor, the terminal,
    // or on top of another dialog. The dialog ones just raise it if it's
    // already open (no dup); word wrap is a plain toggle either way.
    //
    // `!shift` matters: `Alt+Shift+T` was the old terminal-mode combo for
    // "open terminal", and leaving it firing here would keep exactly the
    // duplicate binding this change is removing. (`Alt+Shift+N` for new-folder
    // is a real binding, but it lives in the Files dialog, not this block.)
    if alt && !shift {
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
            // Word wrap (⌥Z, matching VSCode's Alt+Z).
            KeyCode::Char('z') | KeyCode::Char('Z') => {
                app.toggle_word_wrap();
                return;
            }
            // The global to-do list.
            KeyCode::Char('d') | KeyCode::Char('D') => {
                app.open_todos_dialog();
                return;
            }
            // Clipboard history.
            KeyCode::Char('v') | KeyCode::Char('V') => {
                app.open_clipboard_dialog();
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
    // Search in Files (⌘⇧F / Ctrl+Shift+F) — VSCode's project-wide search,
    // also reachable from anywhere.
    if ctrl && shift && matches!(key.code, KeyCode::Char('f') | KeyCode::Char('F')) {
        app.open_search_files();
        return;
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
        // The embedded terminal reads the raw physical Alt rather than
        // `shortcut_mods`'s folded view: it mirrors a real terminal app's
        // shell-emulation chords (Ctrl+←/→ word-jump, Option+↑/↓ copy mode,
        // …), which must reach the shell exactly as pressed.
        Some(Dialog::Terminal) => {
            handle_terminal_modal(app, key, key.modifiers.contains(KeyModifiers::ALT))
        }
        Some(Dialog::TerminalPicker) => handle_terminal_picker_dialog(app, key),
        Some(Dialog::Help) => handle_help_dialog(app, key),
        Some(Dialog::SearchFiles) => handle_search_files_dialog(app, key, ctrl, alt, shift),
        Some(Dialog::Todos) => handle_todos_dialog(app, key, ctrl, alt, shift),
        Some(Dialog::Clipboard) => handle_clipboard_dialog(app, key, ctrl),
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
/// Shift+Enter/Up = previous, Esc closes. Ctrl+R toggles the replace field
/// (Ctrl+H is avoided — most terminals send the same byte as Backspace, so it
/// can never reach here as a distinct chord); while replace is showing, Tab
/// swaps keyboard focus between query and replace.
fn handle_find(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;

    if ctrl && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
        app.find_toggle_replace();
        return;
    }
    if key.code == KeyCode::Tab && app.find.replace_active {
        app.find_toggle_focus();
        return;
    }
    if app.find.replace_focus {
        handle_replace_field(app, key, ctrl, alt, shift);
        return;
    }

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

/// Keys while the replace field has focus: type to edit the replacement
/// text, Enter replaces the current match and advances, Shift+Enter replaces
/// every match.
fn handle_replace_field(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;
    macro_rules! r {
        () => {
            (&app.find.replace, &mut app.find.replace_cursor, &mut app.find.replace_anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.find_close(),
        KeyCode::Enter if shift => app.replace_all(),
        KeyCode::Enter => app.replace_current(),
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => el::select_all(
            &app.find.replace,
            &mut app.find.replace_cursor,
            &mut app.find.replace_anchor,
        ),
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => app.replace_copy(),
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => app.replace_cut(),
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.replace_paste(),
        KeyCode::Left if alt => { let (t, c, a) = r!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = r!(); el::word_right(t, c, a, shift) }
        KeyCode::Left if ctrl => { let (t, c, a) = r!(); el::home(t, c, a, shift) }
        KeyCode::Right if ctrl => { let (t, c, a) = r!(); el::end(t, c, a, shift) }
        KeyCode::Left => { let (t, c, a) = r!(); el::left(t, c, a, shift) }
        KeyCode::Right => { let (t, c, a) = r!(); el::right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = r!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = r!(); el::end(t, c, a, shift) }
        KeyCode::Backspace if alt => el::delete_word_left(
            &mut app.find.replace,
            &mut app.find.replace_cursor,
            &mut app.find.replace_anchor,
        ),
        KeyCode::Backspace => app.replace_backspace(),
        KeyCode::Delete => el::delete_fwd(
            &mut app.find.replace,
            &mut app.find.replace_cursor,
            &mut app.find.replace_anchor,
        ),
        KeyCode::Char(c) if !ctrl && !alt => app.replace_input(c),
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
        // Quit flow: unsaved file — Y save / N don't save / A save all /
        // D discard all / Esc cancel.
        Some(PromptKind::QuitUnsaved) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.quit_save_current(),
                KeyCode::Char('n') | KeyCode::Char('N') => app.quit_skip_current(),
                KeyCode::Char('a') | KeyCode::Char('A') => app.quit_save_all(),
                KeyCode::Char('d') | KeyCode::Char('D') => app.quit_discard_all(),
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
        // A link clicked in a terminal: Enter/Y opens it, Esc/N doesn't.
        Some(PromptKind::OpenUrl) => {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    app.open_url_answer(true)
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    app.open_url_answer(false)
                }
                _ => {}
            }
            return;
        }
        // Quit flow: running terminal — close & quit / close all / cancel.
        Some(PromptKind::QuitTerminal) => {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => app.quit_skip_current(),
                KeyCode::Char('a') | KeyCode::Char('A') => app.quit_discard_all(),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.cancel_quit(),
                _ => {}
            }
            return;
        }
        _ => {}
    }
    use crate::editline as el;
    let (_, alt, shift) = shortcut_mods(&key);
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
        // ⌥R reveals the selected file/folder in Finder — ⌥ (not ⌘R, which
        // renames) avoids a collision within this dialog.
        KeyCode::Char('r') | KeyCode::Char('R') if alt => app.dialog_reveal_in_finder(),
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

/// Project-wide "Search in Files" (⌘⇧F): type a query and results appear on
/// their own a moment after you stop typing (see `App::poll_pending_search`);
/// ↑↓ move through them, Enter opens whichever is selected (and closes the
/// dialog) — it never triggers a search itself.
fn handle_search_files_dialog(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;
    macro_rules! q {
        () => {
            (&app.project_search.query, &mut app.project_search.cursor, &mut app.project_search.anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.search_files_up(),
        KeyCode::Down => app.search_files_down(),
        KeyCode::Enter => app.search_files_open_selected(),
        KeyCode::Left if alt => { let (t, c, a) = q!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = q!(); el::word_right(t, c, a, shift) }
        KeyCode::Left if ctrl => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::Right if ctrl => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Left => { let (t, c, a) = q!(); el::left(t, c, a, shift) }
        KeyCode::Right => { let (t, c, a) = q!(); el::right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            el::select_all(&app.project_search.query, &mut app.project_search.cursor, &mut app.project_search.anchor)
        }
        KeyCode::Char('c') | KeyCode::Char('C') if ctrl => app.search_files_copy(),
        KeyCode::Char('x') | KeyCode::Char('X') if ctrl => app.search_files_cut(),
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.search_files_paste(),
        KeyCode::Backspace if alt => {
            el::delete_word_left(&mut app.project_search.query, &mut app.project_search.cursor, &mut app.project_search.anchor);
            app.search_files_query_changed();
        }
        KeyCode::Backspace => app.search_files_backspace(),
        KeyCode::Delete => {
            el::delete_fwd(&mut app.project_search.query, &mut app.project_search.cursor, &mut app.project_search.anchor);
            app.search_files_query_changed();
        }
        KeyCode::Char(c) if !ctrl && !alt => app.search_files_input(c),
        _ => {}
    }
}

/// The terminal quick-switcher (⌥K): type to filter, ↑↓ to move, Enter jumps.
fn handle_terminal_picker_dialog(app: &mut App, key: KeyEvent) {
    let no_mods = !key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::SUPER);
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.terminal_picker_move(-1),
        KeyCode::Down => app.terminal_picker_move(1),
        KeyCode::Enter => app.terminal_picker_confirm(),
        KeyCode::Backspace => app.terminal_picker_backspace(),
        KeyCode::Char(c) if no_mods => app.terminal_picker_input(c),
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
        KeyCode::Delete | KeyCode::Backspace => app.recent_delete_selected(),
        _ => {}
    }
}

/// The settings dialog. Esc/Enter persists and closes (handled in teardown).
fn handle_settings_dialog(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => app.close_top_dialog(),
        KeyCode::Up | KeyCode::BackTab => app.settings_move_focus(-1),
        KeyCode::Down | KeyCode::Tab => app.settings_move_focus(1),
        KeyCode::Left => app.settings_adjust(-1),
        KeyCode::Right => app.settings_adjust(1),
        KeyCode::Char('-') | KeyCode::Char('_') => app.settings_adjust(-1),
        KeyCode::Char('+') | KeyCode::Char('=') => app.settings_adjust(1),
        _ => {}
    }
}

/// The to-do list (⌥D). Typing goes to the "add a task" line — the list is
/// short enough that adding matters more than filtering — and the row keys act
/// on the highlighted task.
fn handle_todos_dialog(app: &mut App, key: KeyEvent, ctrl: bool, alt: bool, shift: bool) {
    use crate::editline as el;
    macro_rules! q {
        () => {
            (&app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor)
        };
    }
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.todo_move(-1),
        KeyCode::Down => app.todo_move(1),
        KeyCode::Enter => app.todo_add(),
        // Space toggles the highlighted task — but only when it isn't being
        // typed into the input line, where it's just a space.
        KeyCode::Char(' ') if app.todo_input.is_empty() => app.todo_toggle(),
        // ⌃E hands the raw file to the real editor.
        KeyCode::Char('e') | KeyCode::Char('E') if ctrl => app.todo_edit_as_file(),
        KeyCode::Char('d') | KeyCode::Char('D') if ctrl && shift => app.todo_clear_done(),
        KeyCode::Char('d') | KeyCode::Char('D') if ctrl => app.todo_remove(),
        // Input-line editing, same chords as every other dialog's text field.
        KeyCode::Left if alt => { let (t, c, a) = q!(); el::word_left(t, c, a, shift) }
        KeyCode::Right if alt => { let (t, c, a) = q!(); el::word_right(t, c, a, shift) }
        KeyCode::Left if ctrl => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::Right if ctrl => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Left => { let (t, c, a) = q!(); el::left(t, c, a, shift) }
        KeyCode::Right => { let (t, c, a) = q!(); el::right(t, c, a, shift) }
        KeyCode::Home => { let (t, c, a) = q!(); el::home(t, c, a, shift) }
        KeyCode::End => { let (t, c, a) = q!(); el::end(t, c, a, shift) }
        KeyCode::Backspace if alt => {
            el::delete_word_left(&mut app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor)
        }
        KeyCode::Backspace => {
            el::backspace(&mut app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor)
        }
        KeyCode::Delete => {
            el::delete_fwd(&mut app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor)
        }
        KeyCode::Char('a') | KeyCode::Char('A') if ctrl => {
            el::select_all(&app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor)
        }
        // A multi-line paste becomes one task per line — see `App::todo_paste`.
        KeyCode::Char('v') | KeyCode::Char('V') if ctrl => app.todo_paste(),
        KeyCode::Char(c) if !ctrl && !alt => {
            el::insert(&mut app.todo_input, &mut app.todo_input_cursor, &mut app.todo_input_anchor, c)
        }
        _ => {}
    }
}

/// Clipboard history (⌥V): pick an entry, Enter pastes it where you were.
fn handle_clipboard_dialog(app: &mut App, key: KeyEvent, ctrl: bool) {
    match key.code {
        KeyCode::Esc => app.close_top_dialog(),
        KeyCode::Up => app.clip_move(-1),
        KeyCode::Down => app.clip_move(1),
        KeyCode::Enter => app.clip_paste_selected(),
        KeyCode::Char('d') | KeyCode::Char('D') if ctrl => app.clip_clear(),
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
            // ⌘⌥↑/↓ (add caret above / below) is handled in `handle_key` on the
            // raw modifiers — Ctrl and Alt cancel each other out by the time
            // this block runs.
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

    // Terminal management lives on Alt, the whole family together (⌥N new,
    // ⌥W close, ⌥G grid, ⌥K quick-switch, ⌥1-9 jump). These were on ⌘ — which
    // reads nicely on a Mac but is never delivered to a program running inside
    // a terminal emulator, so quick-switch and jump-to-N simply did not exist
    // in the TUI build, and ⌘W was a second way to do ⌥W's job. One binding
    // each, reachable in both.
    if alt {
        if matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K')) {
            app.open_terminal_picker();
            return;
        }
        if let KeyCode::Char(c @ '1'..='9') = key.code {
            app.jump_to_terminal(c as usize - '0' as usize);
            return;
        }
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

    /// One keymap, both modes: the same combo must do the same thing whether
    /// Oxru is drawing its own window or running inside a host terminal. TUI
    /// mode used to demand a 3rd key on every shortcut, which meant muscle
    /// memory built in one mode simply didn't work in the other.
    #[test]
    fn ctrl_shortcuts_fire_on_the_same_combo_in_both_modes() {
        for gui in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let f = dir.path().join("a.txt");
            std::fs::write(&f, "hello world").unwrap();
            let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
            app.gui = gui;
            app.open_path(&f);

            handle_key(&mut app, KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            assert!(
                app.active_buffer().unwrap().selection().is_some(),
                "plain Ctrl+A must select all (gui = {gui})"
            );
        }
    }

    #[test]
    fn alt_shortcuts_fire_on_the_same_combo_in_both_modes() {
        for gui in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
            app.gui = gui;

            handle_key(&mut app, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
            assert!(app.terminal_modal, "plain Alt+T must open the terminal (gui = {gui})");
        }
    }

    /// One action, one combo. The old terminal-mode aliases (`Ctrl+Alt+…` for
    /// a Ctrl binding, `Alt+Shift+…` for an Alt binding) must be dead — a
    /// second combo reaching the same action is the same "two keymaps"
    /// problem, just hidden.
    #[test]
    fn the_old_three_key_aliases_no_longer_fire() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "hello world").unwrap();

        // Alt+Shift+T was terminal-mode's "open terminal".
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        );
        assert!(!app.terminal_modal, "Alt+Shift+T must no longer open the terminal");

        // Ctrl+Alt+A was terminal-mode's "select all".
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL | KeyModifiers::ALT),
        );
        assert!(
            app.active_buffer().unwrap().selection().is_none(),
            "Ctrl+Alt+A must no longer select all"
        );
        // …and must not fall through to the text-insert path either.
        assert_eq!(
            app.active_buffer().unwrap().rope.to_string(),
            "hello world",
            "a dead combo must type nothing"
        );
    }

    /// The other multi-modifier binding: Alt+Shift+N in the Files dialog. Alt
    /// keeps working alongside Shift — only the *global* Alt chords require
    /// Shift to be absent, since that's where the old alias lived.
    #[test]
    fn alt_d_opens_the_todo_list_and_alt_v_the_clipboard() {
        use crate::app::Dialog;
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(app.top_dialog(), Some(Dialog::Todos), "⌥D opens the to-do list");
        handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT));
        assert_eq!(app.top_dialog(), Some(Dialog::Clipboard), "⌥V opens the history");
    }

    /// Typing goes to the add-line, Enter commits, Space toggles.
    #[test]
    fn typing_then_enter_adds_a_task_and_space_toggles_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // Point the store at the scratch dir so this exercises the real
        // write-through path without touching the machine's own list.
        app.todos_path = Some(dir.path().join("todos.md"));
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        for c in "ship it".chars() {
            handle_key(&mut app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.todo_input, "ship it");
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(crate::todo::counts(&app.todos), (0, 1));
        assert!(app.todo_input.is_empty(), "the input clears after adding");

        handle_key(&mut app, KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(crate::todo::counts(&app.todos), (1, 1), "Space checks it off");

        // Every change is on disk immediately, at the injected path only.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("todos.md")).unwrap(),
            "- [x] ship it\n"
        );
    }

    /// The reported bug: pasting a list into the to-do input appeared to do
    /// nothing, because the dialog had no paste binding at all.
    #[test]
    fn pasting_a_list_into_the_todo_input_adds_one_task_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.todos_path = Some(dir.path().join("todos.md"));
        app.open_todos_dialog();
        app.set_clipboard(
            "- search a word in all files\n- copying multiples in clipboard?\nplain line\n".into(),
        );

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

        assert_eq!(crate::todo::counts(&app.todos), (0, 3), "one task per line");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("todos.md")).unwrap(),
            "- [ ] search a word in all files\n- [ ] copying multiples in clipboard?\n- [ ] plain line\n",
            "bullets stripped, all normalised, written straight through"
        );
    }

    #[test]
    fn pasting_one_line_goes_into_the_input_not_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.todos_path = Some(dir.path().join("todos.md"));
        app.open_todos_dialog();
        app.set_clipboard("refactor the parser".into());

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert_eq!(app.todo_input, "refactor the parser", "single line is editable first");
        assert_eq!(crate::todo::counts(&app.todos), (0, 0), "nothing committed yet");

        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(crate::todo::counts(&app.todos), (0, 1));
    }

    #[test]
    fn a_multiline_paste_keeps_whatever_was_half_typed() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.todos_path = Some(dir.path().join("todos.md"));
        app.open_todos_dialog();
        for c in "typed first".chars() {
            handle_key(&mut app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.set_clipboard("one\ntwo\n".into());
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

        let text = std::fs::read_to_string(dir.path().join("todos.md")).unwrap();
        assert_eq!(text, "- [ ] typed first\n- [ ] one\n- [ ] two\n", "nothing dropped");
    }

    #[test]
    fn alt_shift_n_still_starts_a_new_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_file_dialog();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        );
        assert!(app.prompt.active, "⇧⌥N opens the new-folder prompt");
        assert!(!app.terminal_modal, "and must not have hit the global Alt block");
    }

    /// The one binding that legitimately holds Ctrl and Alt together. It has
    /// to keep working even though the two families now cancel out.
    #[test]
    fn ctrl_alt_arrows_still_add_a_caret() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        assert_eq!(app.active_buffer().unwrap().caret_count(), 1);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL | KeyModifiers::ALT),
        );
        assert_eq!(app.active_buffer().unwrap().caret_count(), 2, "⌘⌥↓ adds a caret");
    }

    /// Shift+Alt+arrows extends the selection by word — an Alt binding that
    /// genuinely wants Shift, so the alias removal must not catch it.
    #[test]
    fn shift_alt_arrows_still_extend_the_selection_by_word() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "hello world").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_path(&f);
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::ALT | KeyModifiers::SHIFT),
        );
        assert!(
            app.active_buffer().unwrap().selection().is_some(),
            "⇧⌥→ still selects a word"
        );
    }

    /// The embedded terminal's own key handling deliberately stays exempt
    /// from the TUI 3-key rule (it emulates a real terminal app, not an
    /// Oxru shortcut) — Option+Up must still enter copy mode with just 2
    /// keys even while the rest of the app is in TUI mode.
    #[test]
    fn terminal_pane_alt_chords_stay_two_key_even_in_tui_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        assert!(!app.gui, "defaults to TUI mode");
        app.toggle_terminal_modal();
        assert!(!app.terminal_copy_mode());

        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
        assert!(app.terminal_copy_mode(), "Option+Up alone still enters copy mode in TUI");
    }

    #[test]
    fn cmd_super_acts_like_ctrl() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "hello world").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui = true; // this test is about the Cmd->Ctrl fold, not the TUI 3-key rule
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
        app.gui = true; // this test is about word-jump, not the TUI 3-key rule
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
    fn ctrl_shift_f_opens_search_files_from_anywhere_in_gui_mode() {
        use crate::app::Dialog;
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.gui = true;
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(app.top_dialog(), Some(Dialog::SearchFiles));
    }

    #[test]
    fn alt_z_toggles_word_wrap_in_gui_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        // toggle_word_wrap persists immediately — point it at a scratch file
        // so this test never touches the real machine config.
        app.config_path = Some(dir.path().join("scratch-config.toml"));
        app.gui = true;
        assert!(!app.word_wrap, "off by default");
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert!(app.word_wrap);
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert!(!app.word_wrap);
    }

    #[test]
    fn alt_z_toggles_word_wrap_in_terminal_mode_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap(); // gui = false by default
        app.config_path = Some(dir.path().join("scratch-config.toml"));
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert!(app.word_wrap, "the same Alt+Z fires in the terminal build");
    }

    #[test]
    fn ctrl_shift_f_opens_search_files_in_terminal_mode_too() {
        use crate::app::Dialog;
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap(); // gui = false by default
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL | KeyModifiers::SHIFT));
        assert_eq!(app.top_dialog(), Some(Dialog::SearchFiles), "same combo, both modes");
    }

    #[test]
    fn search_files_enter_opens_a_result_without_needing_to_trigger_a_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "needle\n").unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_search_files();
        for c in "needle".chars() {
            app.search_files_input(c);
        }
        // Force the debounced auto-search to have already produced a result
        // (real usage: this just happens on its own, no key needed).
        app.project_search.pending_search_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));
        app.poll_pending_search();
        // Project search now runs on a background thread; poll for the
        // result instead of assuming it's ready synchronously.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline && !app.poll_search_results() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.project_search.searched);

        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.top_dialog(), None, "Enter opened the result and closed the dialog");
        assert_eq!(app.active_editor, Some(0));
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

    #[test]
    fn alt_k_opens_the_terminal_picker_and_typing_filters_it() {
        // Fixed-name root: the picker fuzzy-matches "folder · command", so a
        // random tempdir name containing the filtered digit makes both
        // terminals match and fails this intermittently.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        let mut app = App::new(Some(root)).unwrap();
        app.toggle_terminal_modal(); // term 0
        app.new_terminal(); // term 1, "<folder> 2"
        assert_eq!(app.top_dialog(), Some(crate::app::Dialog::Terminal));

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT));
        assert_eq!(app.top_dialog(), Some(crate::app::Dialog::TerminalPicker));

        // Typing routes to the picker's query, not the shell underneath.
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.terminal_picker.matches, vec![1]);

        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.active_terminal, 1);
        assert_eq!(app.top_dialog(), Some(crate::app::Dialog::Terminal), "back to the terminal dialog");
    }

    #[test]
    fn cmd_digit_jumps_straight_to_that_terminal_tab() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.toggle_terminal_modal(); // term 0
        app.new_terminal(); // term 1
        app.new_terminal(); // term 2, now active
        assert_eq!(app.active_terminal, 2);

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT));
        assert_eq!(app.active_terminal, 0, "⌘1 jumps to the first terminal");

        handle_key(&mut app, KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT));
        assert_eq!(app.active_terminal, 2, "⌘3 jumps to the third terminal");

        // Past the last tab: no-op, doesn't panic or wrap.
        handle_key(&mut app, KeyEvent::new(KeyCode::Char('9'), KeyModifiers::ALT));
        assert_eq!(app.active_terminal, 2, "⌘9 with only 3 terminals does nothing");
    }

    #[test]
    fn settings_up_and_down_move_focus_in_opposite_directions() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(Some(dir.path().to_path_buf())).unwrap();
        app.open_settings();
        assert_eq!(app.settings_focus, 0);

        handle_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.settings_focus, 1, "Down moves to the next section");

        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.settings_focus, 0, "Up moves back to the previous section");

        handle_key(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.settings_focus, 4, "Up from the first section wraps to the last");
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
