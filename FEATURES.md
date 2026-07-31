# Oxru — Feature & Test Coverage

A living map of what Oxru does and how each feature is verified. The goal is that
**every feature works identically with keyboard and mouse, in both GUI mode
(`--gui`, a real window) and TUI mode (inside another terminal).**

- ✅ **Automated** — covered by a unit/integration test (named below).
- 🟡 **Indirect** — exercised by a test, but not asserted on its own.
- ⚙️ **Manual** — verified by hand only (OS-level behavior or live rendering).

Total: **159 tests** across `app`(48) `buffer`(23) `ui`(24) `filedialog`(12)
`input`(14) `fstree`(6) `terminalpane`(6) `icons`(5) `theme`(5) `config`(4)
`syntax`(4) `instances`(2) `recent`(2) `session`(2) `termbridge`(2).

Run everything: `cargo test --release`.

## How both modes stay in sync

Keyboard and mouse are translated to **shared, window-independent logic**, so a
test of that logic proves both modes:

- **Keyboard** → both loops build a crossterm `KeyEvent` and call
  `input::handle_key(app, key)`. GUI mode's only extra step is
  `gui::translate_key` (winit → crossterm); the TUI loop already gets crossterm
  events. Everything after that is identical and tested.
- **Mouse** → both loops translate native events to a global cell `(col,row)` +
  `shift` and call `App::mouse_down / mouse_drag / mouse_up / mouse_wheel /
  mouse_drag_tick`. All routing (terminal vs editor, forward-to-program vs local
  selection) lives in `App` and is unit-tested.

`TerminalPane` has a test-only recorder (`take_sent`) so tests assert the **exact
bytes** a key/mouse action sends to the shell.

## Editor / text

| Feature | Status | Test |
|---|---|---|
| Insert / type / newline | ✅ | `insert_and_text`, `newline_and_rows` |
| Backspace / delete | ✅ | `backspace_and_delete` |
| Word movement | ✅ | `word_movement_lands_on_boundaries`, `alt_arrow_moves_by_word` |
| Delete word / to line start | ✅ | `delete_word_left_removes_one_word`, `delete_to_line_start_clears_to_bol` |
| Vertical movement keeps goal column | ✅ | `vertical_movement_keeps_goal_col`, `up_on_first_line_jumps_to_start`, `down_on_last_line_jumps_to_line_end` |
| Select all / selection / replace selection | ✅ | `select_all_and_text`, `typing_replaces_selection`, `shift_move_extends_then_delete` |
| Indent / outdent block | ✅ | `indent_and_outdent_a_block`, `outdent_single_line_without_selection` |
| Undo / redo + grouping | ✅ | `undo_redo_roundtrip`, `typing_run_undoes_together`, `moving_cursor_breaks_the_undo_group`, `a_new_edit_clears_redo`, `undo_after_cut_restores_text` |
| Clean-state (edit→revert = saved) | ✅ | `deleting_typed_text_back_to_saved_is_clean`, `undo_back_to_saved_clears_modified` |
| Save / save-all / modified flag | ✅ | `save_clears_modified`, `save_all_writes_every_modified_file` |
| Mouse click positions cursor / drag selects | ✅ | `click_switches_tabs_and_positions_cursor`, `mouse_drag_selects_text` |
| Editor scroll (keyboard + wheel) | ✅ | `editor_scroll_moves_view_not_cursor` |
| Syntax highlighting + cache | ✅ | `rust_highlights_and_preserves_text`, `highlight_cache_recomputes_on_edit`, `every_registered_grammar_builds_and_preserves_text` |

## External file changes (disk ↔ editor sync)

| Feature | Status | Test |
|---|---|---|
| Auto-reload an unmodified buffer when the file changes on disk | ✅ | `external_change_reloads_clean_buffer` |
| Conflict prompt when the file changes with unsaved edits (R reload / K keep) | ✅ | `external_change_with_unsaved_edits_prompts_and_keeps` |
| Reload answer takes the disk version | ✅ | `external_change_reload_answer_takes_disk_version` |
| Deleted-on-disk flags the buffer dirty (save restores) | ✅ | `external_delete_flags_buffer_modified` |
| Our own save isn't mistaken for an external change | ✅ | `own_save_is_not_seen_as_external_change` |

Detection is a throttled `stat` (mtime + length) on open files, run from both
loops via `App::poll_file_changes` and also forced on GUI window-focus. No file
watcher / dependency — adequate because the common trigger is a command in
oxru's own terminal (which keeps the loop ticking).

## Tabs

| Feature | Status | Test |
|---|---|---|
| Open / dedup / navigate / move / close | ✅ | `tabs_open_navigate_move_close`, `opens_and_dedups_tabs`, `nav_wraps_both_directions` |
| Per-tab independent state | ✅ | `editing_two_tabs_keeps_them_independent`, `switching_tabs_shows_each_files_content` |
| Close prompts for unsaved (Y/N) | ✅ | `close_tab_unsaved_prompts_then_save_or_discard`, `close_all_*` |
| Reopen closed tab | ✅ | `reopen_closed_tab_restores_it` |
| Click switches tab | ✅ | `click_switches_tabs_and_positions_cursor` |

## File search / dialog (⌥F)

| Feature | Status | Test |
|---|---|---|
| Fuzzy ranking (filename > path, prefix, recency) | ✅ | `prefix_beats_mid_name_beats_path`, `filename_match_ranks_above_path_only_match`, `search_boosts_recently_opened_file`, `ranks_best_first` |
| Tree on empty query, flat on search | ✅ | `dialog_browses_tree_when_query_empty` |
| Scope into / drill out of a folder | ✅ | `dialog_scopes_search_into_a_folder`, `dialog_shift_tab_drills_out_of_folder` |
| gitignore parity (VSCode) | ✅ | `gitignore_filters_search`, `nested_gitignore_is_respected`, `search_honours_project_gitignore`, `untracked_dotfiles_show_in_search` |
| Multi-open ticked files | ✅ | `file_dialog_opens_multiple_ticked_files` |
| Binary files shown disabled / not opened | ✅ | `binary_files_are_not_opened_as_text`, `binary_detection_by_extension` |
| Opening a file dismisses dialogs | ✅ | `opening_a_file_dismisses_open_dialogs` |
| New / rename / delete in dialog | ✅ | `dialog_new_file_creates_at_root`, `dialog_rename_then_delete` |

## Terminal — keyboard (sent to the shell)

| Feature | Status | Test |
|---|---|---|
| Ctrl+letter control codes | ✅ | `ctrl_letter_is_control_code` |
| Arrows / Home / End / Delete / PageUp-Dn escapes | ✅ | `arrows_are_escape_sequences` |
| Enter variants (CR / ⌥soft / ⇧LF) | ✅ | `enter_newline_variants` |
| ⌥+char as Meta (ESC prefix) | ✅ | `alt_char_is_esc_prefixed` |
| ⌥← / ⌥→ **and** ⌃← / ⌃→ word move (ESC b/f) | ✅ | `terminal_word_move_chords` |
| ⌘← / ⌘→ line start/end; ⌘⌫ kill-to-start; ⌥⌫ delete word | ✅ | `terminal_line_edit_chords`, `option_backspace_deletes_word` |
| fn/PageUp scrolls scrollback (not sent to shell) | ✅ | `terminal_plain_pageup_scrolls_not_sent` |
| Switch / move terminals (⌃Tab, ⌃⇧↔) | ✅ | `ctrl_tab_switches_terminals`, `ctrl_shift_arrow_moves_terminal` |
| Copy mode enter/exit (⌥↑) | ✅ | `terminal_copy_mode_enter_and_exit` |
| Requires an open folder | ✅ | `terminal_requires_an_open_folder` |
| Paste (bracketed) | ✅ | `paste_delivers_text_to_shell` |

## Terminal — mouse (shared GUI + TUI dispatch)

| Feature | Status | Test |
|---|---|---|
| Drag selects text | ✅ | `drag_selects_terminal_text_without_mouse_mode`, `mouse_drag_selects_text` |
| Shift+Click extends selection across scroll | ✅ | `shift_click_extends_selection_across_scroll` |
| Selection spans scrollback; copy reads it | ✅ | `selection_reads_across_scrollback` |
| Wheel scrolls scrollback at the shell | ✅ | `wheel_scrolls_local_scrollback_at_shell` |
| Wheel sends arrow keys to alt-screen pagers | ✅ | `wheel_sends_arrows_on_alt_screen_pager` |
| Wheel forwarded to mouse-reporting programs (SGR) | ✅ | `wheel_forwards_to_program_in_mouse_mode` |
| Click forwarded in mouse mode; Shift = local select | ✅ | `click_forwards_in_mouse_mode_but_shift_selects_locally` |
| Detects program mouse-mode request | ✅ | `detects_mouse_reporting_request` |
| Drag-edge auto-scroll conveyor | 🟡 | exercised via `mouse_drag_tick`; not asserted alone |

## Terminal — rendering / process

| Feature | Status | Test |
|---|---|---|
| Captures + renders shell output | ✅ | `captures_command_output`, `grid_view_renders_each_open_file` |
| Foreground-process label / disambiguation | ✅ | `display_name_reflects_running_command`, `terminal_labels_disambiguate_duplicates`, `term_title_from_cd_command` |
| Grid vs tabs layout | ✅ | `grid_layout_is_left_right_bottom` |
| New terminal placement | ✅ | `new_terminal_opens_beside_current_and_moves` |

## UI rendering (TestBackend)

| Feature | Status | Test |
|---|---|---|
| Footers list shortcuts, full width, 2 rows | ✅ | `editor_footer_lists_shortcuts_over_two_rows`, `justified_actions_span_full_width`, `footer_tab_hints_appear_with_multiple_tabs` |
| Find bar + all-match highlight + key hints | ✅ | `find_highlights_all_matches`, `find_bar_shows_key_hints`, `find_is_case_insensitive_and_selects_current_match`, `find_no_results_then_close` |
| F1 help overlay, context-aware | ✅ | `help_overlay_lists_shortcuts_by_context`, `help_hides_folder_only_shortcuts_with_no_folder`, `f1_opens_shortcuts_help` |
| Block cursor draw | ✅ | `editor_draws_block_cursor`, `rename_prompt_draws_block_cursor` |
| Stacked dialogs dim/focus | ✅ | `stacked_dialogs_focused_full_color_lower_dimmed` |
| Tiny-size safety | ✅ | `renders_at_tiny_size_without_panic` |
| Toasts | ✅ | `toast_shows_then_disappears` |
| File icons share glyph, differ by color | ✅ | `file_types_share_glyph_differ_by_color`, `rust_file_has_distinct_color` |

## Config / theme / session / instances

| Feature | Status | Test |
|---|---|---|
| Settings persist + merge; font/accent | ✅ | `save_prefs_roundtrips_and_merges`, `settings_adjusts_font_and_accent` |
| Theme parse / overrides / accent sync | ✅ | `parses_hex_with_and_without_hash`, `overrides_replace_named_colours`, `project_overrides_apply`, `accent_override_syncs_selection_bg` |
| Session save/restore, stale paths dropped | ✅ | `save_then_load_roundtrips_existing_files`, `missing_files_are_dropped_and_active_clamped` |
| Recent folders / multiselect | ✅ | `record_moves_to_front_and_dedupes`, `recent_dialog_multiselect` |
| Multi-instance markers | ✅ | `live_process_is_listed`, `dead_process_marker_is_pruned` |
| Terminal shim / `do script` bridge | ✅ | `shim_routes_terminal_do_script_to_request_file`, `zdotdir_keeps_shim_ahead_of_usr_bin` |

## Known gaps (manual-only)

| Area | Status | Why / how it's checked |
|---|---|---|
| `gui::translate_key` (winit → crossterm) | ⚙️ | winit `KeyEvent` can't be constructed in a unit test; covered by the shared `handle_key` logic it feeds. |
| Pixel → cell mapping (`cursor_cell`) | ⚙️ | trivial arithmetic over live window size; verified by hand. |
| Real GPU window render (ratatui-wgpu) | ⚙️ | no display in test env; UI asserted via `TestBackend` instead. |
| App-Nap / no-freeze when backgrounded | ⚙️ | OS scheduler behavior; verified with a logged 10-minute backgrounded stream test. |
| Live macOS focus/occlusion repaint | ⚙️ | window-server events; verified by hand. |
