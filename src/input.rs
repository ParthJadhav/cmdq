//! Input handling — line editor for the queue input buffer + key routing
//! decisions for the app.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::queue::Queue;

/// What the app should do in response to a key, while the queue panel owns
/// keystrokes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Nothing,
    /// Forward these raw bytes to the PTY (ie. send to the running child).
    ForwardToChild(Vec<u8>),
    /// Enqueue a new command (committed from the input buffer).
    EnqueueCurrent { command: String, conditional: bool },
    /// Commit an edit on the queue item at the given index.
    CommitEdit { index: usize, command: String, conditional: bool },
    /// Cancel the current edit (without removing the item).
    CancelEdit,
    /// Remove the item being edited.
    DeleteEdited,
    /// Move the edited item up by one position.
    MoveEditedUp,
    /// Move the edited item down by one position.
    MoveEditedDown,
    /// Toggle the queue paused/running state.
    TogglePause,
    /// Clear the entire queue.
    ClearQueue,
    /// Toggle "force queue mode" (capture even at prompt).
    ToggleForceQueue,
    /// Show the help overlay.
    ToggleHelp,
    /// Toggle the chain ("conditional") flag on the in-progress draft/edit.
    /// `now_on` is the new state of the flag after the toggle.
    ToggleChain { now_on: bool },
}

#[derive(Debug, Default)]
pub struct LineEditor {
    pub buffer: String,
    pub cursor: usize,
    /// If Some, we are editing the queue item at this index; Enter commits.
    pub editing_index: Option<usize>,
    /// Conditional flag for the in-progress draft / edit.
    pub conditional: bool,
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.editing_index = None;
        self.conditional = false;
    }

    pub fn load_for_edit(&mut self, index: usize, item_command: &str, conditional: bool) {
        self.buffer = item_command.to_string();
        self.cursor = self.buffer.len();
        self.editing_index = Some(index);
        self.conditional = conditional;
    }

    fn prev_boundary(&self) -> usize {
        self.buffer[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.buffer[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.buffer.len())
    }

    fn insert_char(&mut self, c: char) {
        self.buffer.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.buffer.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary();
        self.buffer.replace_range(prev..self.cursor, "");
        self.cursor = prev;
    }

    fn delete_at_cursor(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.next_boundary();
        self.buffer.replace_range(self.cursor..next, "");
    }

    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor = self.prev_boundary();
    }

    fn move_right(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        self.cursor = self.next_boundary();
    }

    /// Translate a key event into an InputAction. `queue` is consulted only
    /// for navigation (Up/Down) — does not mutate it.
    pub fn handle_key(&mut self, key: KeyEvent, queue: &Queue) -> InputAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Char(c) if ctrl => match c {
                'c' | 'C' => {
                    if !self.buffer.is_empty() {
                        self.reset();
                        InputAction::Nothing
                    } else {
                        InputAction::ForwardToChild(vec![0x03])
                    }
                }
                'd' | 'D' => {
                    if self.editing_index.is_some() {
                        InputAction::DeleteEdited
                    } else {
                        InputAction::Nothing
                    }
                }
                'k' | 'K' => InputAction::ClearQueue,
                'x' | 'X' => InputAction::TogglePause,
                'q' | 'Q' => InputAction::ToggleForceQueue,
                'a' | 'A' => {
                    self.cursor = 0;
                    InputAction::Nothing
                }
                'e' | 'E' => {
                    self.cursor = self.buffer.len();
                    InputAction::Nothing
                }
                'u' | 'U' => {
                    // Kill back-to-start.
                    self.buffer.replace_range(..self.cursor, "");
                    self.cursor = 0;
                    InputAction::Nothing
                }
                _ => InputAction::Nothing,
            },
            KeyCode::Char(_) if alt => InputAction::Nothing,
            KeyCode::Char('?')
                if !ctrl && self.buffer.is_empty() && self.editing_index.is_none() =>
            {
                InputAction::ToggleHelp
            }
            KeyCode::Char(c) => {
                self.insert_char(c);
                InputAction::Nothing
            }
            KeyCode::Backspace => {
                self.backspace();
                InputAction::Nothing
            }
            KeyCode::Delete => {
                self.delete_at_cursor();
                InputAction::Nothing
            }
            KeyCode::Left => {
                self.move_left();
                InputAction::Nothing
            }
            KeyCode::Right => {
                self.move_right();
                InputAction::Nothing
            }
            KeyCode::Home => {
                self.cursor = 0;
                InputAction::Nothing
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                InputAction::Nothing
            }
            KeyCode::Up if alt && self.editing_index.is_some() => InputAction::MoveEditedUp,
            KeyCode::Up if alt => InputAction::Nothing,
            KeyCode::Up => {
                self.navigate_up(queue);
                InputAction::Nothing
            }
            KeyCode::Down if alt && self.editing_index.is_some() => InputAction::MoveEditedDown,
            KeyCode::Down if alt => InputAction::Nothing,
            KeyCode::Down => {
                self.navigate_down(queue);
                InputAction::Nothing
            }
            KeyCode::Tab => {
                self.conditional = !self.conditional;
                InputAction::ToggleChain { now_on: self.conditional }
            }
            KeyCode::Enter => {
                let cmd = self.buffer.trim().to_string();
                if cmd.is_empty() {
                    InputAction::Nothing
                } else if let Some(idx) = self.editing_index {
                    let cond = self.conditional;
                    self.reset();
                    InputAction::CommitEdit {
                        index: idx,
                        command: cmd,
                        conditional: cond,
                    }
                } else {
                    let cond = self.conditional;
                    self.reset();
                    InputAction::EnqueueCurrent {
                        command: cmd,
                        conditional: cond,
                    }
                }
            }
            KeyCode::Esc => {
                if self.editing_index.is_some() {
                    self.reset();
                    InputAction::CancelEdit
                } else {
                    self.reset();
                    InputAction::Nothing
                }
            }
            _ => InputAction::Nothing,
        }
    }

    fn navigate_up(&mut self, queue: &Queue) {
        if queue.is_empty() {
            return;
        }
        let target = match self.editing_index {
            None => queue.len() - 1,
            Some(i) => i.saturating_sub(1),
        };
        if let Some(item) = queue.items().get(target) {
            self.load_for_edit(target, &item.command, item.conditional);
        }
    }

    fn navigate_down(&mut self, queue: &Queue) {
        if queue.is_empty() {
            return;
        }
        match self.editing_index {
            None => {}
            Some(i) => {
                if i + 1 >= queue.len() {
                    self.reset();
                } else if let Some(item) = queue.items().get(i + 1) {
                    self.load_for_edit(i + 1, &item.command, item.conditional);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn typing_appends_and_enter_enqueues() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        ed.handle_key(key(KeyCode::Char('l')), &q);
        ed.handle_key(key(KeyCode::Char('s')), &q);
        let action = ed.handle_key(key(KeyCode::Enter), &q);
        match action {
            InputAction::EnqueueCurrent { command, .. } => assert_eq!(command, "ls"),
            other => panic!("got {:?}", other),
        }
        assert!(ed.buffer.is_empty());
    }

    #[test]
    fn backspace_removes_char() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        for c in "abc".chars() {
            ed.handle_key(key(KeyCode::Char(c)), &q);
        }
        ed.handle_key(key(KeyCode::Backspace), &q);
        assert_eq!(ed.buffer, "ab");
    }

    #[test]
    fn navigate_up_loads_last_queue_item_for_edit() {
        let mut q = Queue::new();
        q.push("first", false);
        q.push("second", false);
        let mut ed = LineEditor::new();
        ed.handle_key(key(KeyCode::Up), &q);
        assert_eq!(ed.editing_index, Some(1));
        assert_eq!(ed.buffer, "second");
        ed.handle_key(key(KeyCode::Up), &q);
        assert_eq!(ed.editing_index, Some(0));
        assert_eq!(ed.buffer, "first");
    }

    #[test]
    fn ctrl_c_clears_input_when_non_empty() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        ed.handle_key(key(KeyCode::Char('x')), &q);
        let action = ed.handle_key(ctrl('c'), &q);
        assert_eq!(action, InputAction::Nothing);
        assert!(ed.buffer.is_empty());
    }

    #[test]
    fn ctrl_c_forwards_when_empty() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        let action = ed.handle_key(ctrl('c'), &q);
        assert_eq!(action, InputAction::ForwardToChild(vec![0x03]));
    }

    #[test]
    fn esc_during_edit_cancels() {
        let mut q = Queue::new();
        q.push("x", false);
        let mut ed = LineEditor::new();
        ed.handle_key(key(KeyCode::Up), &q);
        assert!(ed.editing_index.is_some());
        let action = ed.handle_key(key(KeyCode::Esc), &q);
        assert_eq!(action, InputAction::CancelEdit);
        assert!(ed.editing_index.is_none());
    }

    #[test]
    fn enter_during_edit_commits() {
        let mut q = Queue::new();
        q.push("orig", false);
        let mut ed = LineEditor::new();
        ed.handle_key(key(KeyCode::Up), &q);
        ed.handle_key(key(KeyCode::Backspace), &q);
        ed.handle_key(key(KeyCode::Char('!')), &q);
        let action = ed.handle_key(key(KeyCode::Enter), &q);
        match action {
            InputAction::CommitEdit { index, command, .. } => {
                assert_eq!(index, 0);
                assert_eq!(command, "ori!");
            }
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn ctrl_d_deletes_edited_item_with_text_in_buffer() {
        let mut q = Queue::new();
        q.push("orig", false);
        let mut ed = LineEditor::new();
        ed.handle_key(key(KeyCode::Up), &q);
        assert_eq!(ed.buffer, "orig");
        let action = ed.handle_key(ctrl('d'), &q);
        assert_eq!(action, InputAction::DeleteEdited);
    }

    #[test]
    fn ctrl_d_outside_edit_is_noop_in_editor() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        ed.handle_key(key(KeyCode::Char('x')), &q);
        let action = ed.handle_key(ctrl('d'), &q);
        assert_eq!(action, InputAction::Nothing);
    }

    #[test]
    fn ctrl_x_toggles_pause() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        let action = ed.handle_key(ctrl('x'), &q);
        assert_eq!(action, InputAction::TogglePause);
    }

    #[test]
    fn ctrl_k_clears_queue() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        let action = ed.handle_key(ctrl('k'), &q);
        assert_eq!(action, InputAction::ClearQueue);
    }

    #[test]
    fn empty_enter_is_noop() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        let action = ed.handle_key(key(KeyCode::Enter), &q);
        assert_eq!(action, InputAction::Nothing);
    }

    #[test]
    fn question_mark_with_empty_buffer_opens_help() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        let action = ed.handle_key(key(KeyCode::Char('?')), &q);
        assert_eq!(action, InputAction::ToggleHelp);
    }

    #[test]
    fn question_mark_with_text_in_buffer_inserts_normally() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        ed.handle_key(key(KeyCode::Char('e')), &q);
        ed.handle_key(key(KeyCode::Char('c')), &q);
        ed.handle_key(key(KeyCode::Char('h')), &q);
        let action = ed.handle_key(key(KeyCode::Char('?')), &q);
        assert_eq!(action, InputAction::Nothing);
        assert_eq!(ed.buffer, "ech?");
    }

    #[test]
    fn question_mark_during_edit_inserts_normally() {
        // While editing a queued item, ? must be insertable as part of the cmd.
        let mut q = Queue::new();
        q.push("orig", false);
        let mut ed = LineEditor::new();
        ed.handle_key(key(KeyCode::Up), &q); // start editing
        let action = ed.handle_key(key(KeyCode::Char('?')), &q);
        assert_eq!(action, InputAction::Nothing);
        assert!(ed.buffer.ends_with('?'));
    }

    #[test]
    fn tab_toggles_conditional() {
        let mut ed = LineEditor::new();
        let q = Queue::new();
        assert!(!ed.conditional);
        let a = ed.handle_key(key(KeyCode::Tab), &q);
        assert_eq!(a, InputAction::ToggleChain { now_on: true });
        assert!(ed.conditional);
        let a = ed.handle_key(key(KeyCode::Tab), &q);
        assert_eq!(a, InputAction::ToggleChain { now_on: false });
        assert!(!ed.conditional);
    }
}
