//! A small one-line prompt overlay used by the Explorer for file operations:
//! entering a name (new file / new folder / rename) or confirming a delete.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    NewFile,
    NewFolder,
    Rename,
    Delete,
    /// "Save changes before closing?" — a save / discard / cancel choice raised
    /// per file by the close-all flow.
    CloseUnsaved,
    /// Same choice, but for closing a single tab (Ctrl+W) with unsaved changes.
    CloseTab,
    /// "Save changes before quitting?" — raised per unsaved file by the quit
    /// flow (save / discard / cancel).
    QuitUnsaved,
    /// "A terminal is still running — close it and quit?" — raised per busy
    /// terminal by the quit flow (yes / cancel).
    QuitTerminal,
    /// "This file changed on disk while you had unsaved edits" — reload (discard
    /// my edits) or keep mine. Only raised when there's a genuine conflict.
    ExternalChange,
}

#[derive(Debug, Default)]
pub struct Prompt {
    pub active: bool,
    pub input: String,
    /// Cursor and selection anchor within `input` (char indices) — the name
    /// field is a full single-line input, edited via [`crate::editline`].
    pub cursor: usize,
    pub anchor: Option<usize>,
    /// Base directory (new file/folder) or the target entry (rename/delete).
    pub target: PathBuf,
    /// Free-form prompt text for confirmations that aren't tied to a path (the
    /// quit-flow prompts set this).
    pub message: String,
    kind: Option<PromptKind>,
}

impl Prompt {
    pub fn open(&mut self, kind: PromptKind, target: PathBuf, initial: String) {
        self.active = true;
        self.kind = Some(kind);
        self.target = target;
        self.cursor = initial.chars().count();
        self.anchor = None;
        self.input = initial;
        self.message = String::new();
    }

    /// Open a confirmation prompt that shows a free-form `message` (used by the
    /// quit flow, where the subject is a file or terminal, not a path entry).
    pub fn open_confirm(&mut self, kind: PromptKind, message: String) {
        self.active = true;
        self.kind = Some(kind);
        self.target = PathBuf::new();
        self.input = String::new();
        self.cursor = 0;
        self.anchor = None;
        self.message = message;
    }

    pub fn close(&mut self) {
        self.active = false;
        self.input.clear();
        self.cursor = 0;
        self.anchor = None;
        self.message.clear();
        self.kind = None;
    }

    pub fn kind(&self) -> Option<PromptKind> {
        self.kind
    }

    /// Delete and the choice prompts are y/n-style; the others take typed text.
    pub fn needs_input(&self) -> bool {
        !matches!(
            self.kind,
            Some(PromptKind::Delete)
                | Some(PromptKind::CloseUnsaved)
                | Some(PromptKind::CloseTab)
                | Some(PromptKind::QuitUnsaved)
                | Some(PromptKind::QuitTerminal)
                | Some(PromptKind::ExternalChange)
        )
    }

    /// The file name the prompt targets (for display).
    fn target_name(&self) -> String {
        self.target
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// The label shown on the prompt.
    pub fn title(&self) -> String {
        match self.kind {
            Some(PromptKind::NewFile) => "New file".to_string(),
            Some(PromptKind::NewFolder) => "New folder".to_string(),
            Some(PromptKind::Rename) => "Rename".to_string(),
            Some(PromptKind::Delete) => {
                format!("Delete \"{}\"?  Enter = confirm, Esc = cancel", self.target_name())
            }
            Some(PromptKind::CloseUnsaved) => {
                format!("Save changes to \"{}\"?", self.target_name())
            }
            Some(PromptKind::CloseTab) => {
                format!("\"{}\" has unsaved changes", self.target_name())
            }
            Some(PromptKind::QuitUnsaved) | Some(PromptKind::QuitTerminal) => self.message.clone(),
            Some(PromptKind::ExternalChange) => {
                format!(
                    "\"{}\" changed on disk — you have unsaved edits.  R = reload (lose edits) · K = keep mine",
                    self.target_name()
                )
            }
            None => String::new(),
        }
    }
}
