use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::shell_syntax::{
    first_meaningful_submitted_paste_line, is_likely_complete_shell_command,
    line_may_start_shell_continuation,
};
use crate::text_edit;

/// A guess at the line the user is typing at the shell prompt, rebuilt from
/// the keys cmdq forwards. It only decides whether Enter plausibly started a
/// command before the shell's preexec marker arrives, so typing right after
/// Enter can be queued. Keys outside the readline subset it models make it
/// unreliable until the next prompt.
#[derive(Debug)]
pub(super) struct PromptMirror {
    line: String,
    cursor: usize,
    reliable: bool,
    /// The user submitted an incomplete construct (`if true`, `cat <<EOF`,
    /// `echo one |`). The shell is showing a continuation prompt, so the next
    /// Enter does not start a command.
    continuation: bool,
}

impl Default for PromptMirror {
    fn default() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
            reliable: true,
            continuation: false,
        }
    }
}

impl PromptMirror {
    /// The shell drew a fresh prompt, or started running a command.
    pub(super) fn new_prompt(&mut self) {
        *self = Self::default();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.line.is_empty()
    }

    pub(super) fn cancel_continuation(&mut self) {
        self.continuation = false;
    }

    /// Whether forwarding this key at the prompt should count as the shell
    /// starting a command.
    pub(super) fn starts_command(&self, key: &KeyEvent, submitted: Option<&str>) -> bool {
        key.code == KeyCode::Enter
            && !self.continuation
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && submitted.is_some_and(is_likely_complete_shell_command)
    }

    /// Apply a key forwarded to the shell prompt. Returns the submitted line
    /// when the key is Enter and the mirror is still reliable.
    pub(super) fn on_key(&mut self, key: &KeyEvent) -> Option<String> {
        type Edit = fn(&mut String, &mut usize);
        let word_left: Edit =
            |line, cursor| *cursor = text_edit::previous_word_start(line, *cursor);
        let word_right: Edit = |line, cursor| *cursor = text_edit::next_word_end(line, *cursor);
        let left: Edit = |line, cursor| *cursor = text_edit::prev_boundary(line, *cursor);
        let right: Edit = |line, cursor| *cursor = text_edit::next_boundary(line, *cursor);
        let home: Edit = |_, cursor| *cursor = 0;
        let end: Edit = |line, cursor| *cursor = line.len();
        let delete: Edit = |line, cursor| text_edit::delete_at_cursor(line, *cursor);

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let edit: Edit = match key.code {
            KeyCode::Char(c) if !ctrl && !alt => {
                if self.reliable {
                    let mut buf = [0u8; 4];
                    text_edit::insert_str(
                        &mut self.line,
                        &mut self.cursor,
                        c.encode_utf8(&mut buf),
                    );
                }
                return None;
            }
            KeyCode::Backspace => text_edit::backspace,
            KeyCode::Delete => delete,
            KeyCode::Left if alt => word_left,
            KeyCode::Right if alt => word_right,
            KeyCode::Left => left,
            KeyCode::Right => right,
            KeyCode::Home => home,
            KeyCode::End => end,
            KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
                'a' => home,
                'e' => end,
                'b' => left,
                'f' => right,
                'h' => text_edit::backspace,
                'd' => delete,
                'k' => |line, cursor| line.truncate(*cursor),
                // Typically clears a recalled history line, whose cursor sits
                // at the end, so the line is known to be empty afterwards.
                'u' if !self.reliable => {
                    self.line.clear();
                    self.cursor = 0;
                    self.reliable = true;
                    return None;
                }
                'u' => text_edit::kill_to_start,
                'w' => text_edit::delete_previous_word,
                'l' => return None,
                'c' => {
                    self.new_prompt();
                    return None;
                }
                _ => {
                    self.invalidate();
                    return None;
                }
            },
            KeyCode::Char('b' | 'B') => word_left,
            KeyCode::Char('f' | 'F') => word_right,
            KeyCode::Enter => {
                let submitted = self.reliable.then(|| self.line.clone());
                self.submit(submitted.as_deref());
                return submitted;
            }
            _ => {
                self.invalidate();
                return None;
            }
        };
        if self.reliable {
            edit(&mut self.line, &mut self.cursor);
        }
        None
    }

    /// Apply a paste forwarded to the shell prompt. Returns the first line the
    /// paste submits, if any.
    pub(super) fn on_paste(&mut self, text: &str, bracketed: bool) -> Option<String> {
        if !self.reliable {
            return None;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if !normalized.contains('\n') {
            text_edit::insert_str(&mut self.line, &mut self.cursor, &normalized);
            return None;
        }
        // Readline's bracketed-paste mode inserts line breaks into the edit
        // buffer without submitting it. Multiline buffers are outside this
        // model, so wait for the shell's own marker instead.
        if bracketed {
            self.invalidate();
            return None;
        }
        let submitted = first_meaningful_submitted_paste_line(&normalized, &self.line, self.cursor);
        self.line.clear();
        self.cursor = 0;
        self.submit(submitted.as_deref());
        submitted
    }

    fn submit(&mut self, submitted: Option<&str>) {
        self.cursor = 0;
        self.line.clear();
        self.reliable = true;
        if submitted.is_some_and(line_may_start_shell_continuation) {
            self.continuation = true;
        }
    }

    fn invalidate(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.reliable = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
            .collect()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Vec<KeyEvent> {
        vec![KeyEvent::new(code, modifiers)]
    }

    fn submitted(keys: Vec<Vec<KeyEvent>>) -> Option<String> {
        let mut mirror = PromptMirror::default();
        for key in keys.concat() {
            assert_eq!(mirror.on_key(&key), None, "{key:?} submitted early");
        }
        mirror.on_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn follows_midline_readline_edits() {
        let left = key(KeyCode::Left, KeyModifiers::NONE);
        let line = submitted(vec![
            typed("echo wrld"),
            left.clone(),
            left.clone(),
            left,
            typed("o"),
        ]);
        assert_eq!(line.as_deref(), Some("echo world"));
    }

    #[test]
    fn follows_ctrl_w_word_kill() {
        let line = submitted(vec![
            typed("sleep 5"),
            key(KeyCode::Char('w'), KeyModifiers::CONTROL),
            typed("10"),
        ]);
        assert_eq!(line.as_deref(), Some("sleep 10"));
    }

    #[test]
    fn follows_alt_word_movement() {
        let line = submitted(vec![
            typed("echo one three"),
            key(KeyCode::Char('b'), KeyModifiers::ALT),
            typed("two "),
            key(KeyCode::Char('f'), KeyModifiers::ALT),
            typed("!"),
        ]);
        assert_eq!(line.as_deref(), Some("echo one two three!"));

        let line = submitted(vec![
            typed("echo alph beta"),
            key(KeyCode::Left, KeyModifiers::ALT),
            key(KeyCode::Left, KeyModifiers::NONE),
            typed("a"),
            key(KeyCode::Right, KeyModifiers::ALT),
            typed("!"),
        ]);
        assert_eq!(line.as_deref(), Some("echo alpha beta!"));
    }

    #[test]
    fn unmodelled_keys_make_the_line_unknown_until_cleared() {
        let up = key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(submitted(vec![typed("echo stale"), up.clone()]), None);
        let line = submitted(vec![
            up,
            key(KeyCode::Char('u'), KeyModifiers::CONTROL),
            typed("ls"),
        ]);
        assert_eq!(line.as_deref(), Some("ls"));
    }

    #[test]
    fn incomplete_lines_hold_off_command_start_until_next_prompt() {
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let mut mirror = PromptMirror::default();
        for key in typed("if true") {
            mirror.on_key(&key);
        }
        let line = mirror.on_key(&enter);
        assert!(!mirror.starts_command(&enter, line.as_deref()));
        for key in typed("then echo hi; fi") {
            mirror.on_key(&key);
        }
        let line = mirror.on_key(&enter);
        assert!(!mirror.starts_command(&enter, line.as_deref()));

        mirror.new_prompt();
        for key in typed("echo hi") {
            mirror.on_key(&key);
        }
        let line = mirror.on_key(&enter);
        assert!(mirror.starts_command(&enter, line.as_deref()));
    }
}
