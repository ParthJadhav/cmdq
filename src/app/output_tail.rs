//! Recognising, from a running command's recent output, that it is waiting
//! for an answer (`Password:`, `[y/N]`, `>>> `) rather than producing output.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailParser {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

#[derive(Debug, Clone)]
pub(super) struct OutputTail {
    text: String,
    parser: TailParser,
}

impl Default for OutputTail {
    fn default() -> Self {
        Self {
            text: String::new(),
            parser: TailParser::Ground,
        }
    }
}

impl OutputTail {
    pub(super) fn clear(&mut self) {
        self.text.clear();
        self.parser = TailParser::Ground;
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.parser {
                TailParser::Ground => match b {
                    0x1b => self.parser = TailParser::Esc,
                    b'\r' | b'\n' => self.text.clear(),
                    0x08 | 0x7f => {
                        self.text.pop();
                    }
                    b'\t' => self.push(' '),
                    0x20..=0x7e => self.push(b as char),
                    _ => {}
                },
                TailParser::Esc => match b {
                    b'[' => self.parser = TailParser::Csi,
                    b']' => self.parser = TailParser::Osc,
                    _ => self.parser = TailParser::Ground,
                },
                TailParser::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.parser = TailParser::Ground;
                    }
                }
                TailParser::Osc => match b {
                    0x07 => self.parser = TailParser::Ground,
                    0x1b => self.parser = TailParser::OscEsc,
                    _ => {}
                },
                TailParser::OscEsc => {
                    self.parser = TailParser::Ground;
                }
            }
        }
    }

    const MAX_TAIL_CHARS: usize = 200;

    fn push(&mut self, c: char) {
        self.text.push(c);
        // `feed` only pushes ASCII, so this byte offset is a char boundary.
        if self.text.len() > Self::MAX_TAIL_CHARS * 2 {
            let drain_to = self.text.len() - Self::MAX_TAIL_CHARS;
            self.text.drain(..drain_to);
        }
    }

    /// The last `MAX_TAIL_CHARS` characters of output on the current line.
    fn tail(&self) -> &str {
        let start = self.text.len().saturating_sub(Self::MAX_TAIL_CHARS);
        let start = (start..=self.text.len())
            .find(|&i| self.text.is_char_boundary(i))
            .unwrap_or(0);
        &self.text[start..]
    }

    pub(super) fn looks_like_input_prompt(&self) -> bool {
        let trimmed = self.tail().trim_end();
        if trimmed.is_empty() {
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("password")
            || lower.contains("passphrase")
            || lower.contains("(yes/no)")
            || lower.contains("[y/n]")
            || lower.contains("press enter")
            || lower.contains("press return")
            || lower.contains("press any key")
            || lower.contains("hit enter")
            || lower.contains("hit return")
        {
            return true;
        }
        if looks_like_status_tail(&lower) {
            return false;
        }
        if trimmed.ends_with('?') {
            return trimmed.chars().count() <= 80;
        }
        if trimmed.ends_with(':') {
            return contains_prompt_word(&lower)
                || (trimmed.chars().count() <= 24
                    && lower.split_whitespace().count() <= 3
                    && lower.chars().any(|c| c.is_ascii_alphabetic()));
        }
        looks_like_repl_prompt(trimmed)
    }

    pub(super) fn looks_like_any_key_prompt(&self) -> bool {
        let lower = self.tail().trim_end().to_ascii_lowercase();
        lower.contains("press any key") || lower.contains("hit any key")
    }
}

fn contains_prompt_word(lower: &str) -> bool {
    [
        "answer",
        "choice",
        "code",
        "confirm",
        "continue",
        "directory",
        "email",
        "enter",
        "file",
        "host",
        "input",
        "name",
        "otp",
        "passcode",
        "path",
        "port",
        "select",
        "token",
        "user",
        "username",
        "value",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn looks_like_status_tail(lower: &str) -> bool {
    let first_word = lower
        .trim_start()
        .split(|c: char| c.is_whitespace() || matches!(c, ':' | '.' | '-' | '>'))
        .find(|part| !part.is_empty())
        .unwrap_or("");
    matches!(
        first_word,
        "building"
            | "checking"
            | "compiling"
            | "connected"
            | "connecting"
            | "debug"
            | "downloaded"
            | "downloading"
            | "error"
            | "extracting"
            | "failed"
            | "fetching"
            | "finished"
            | "info"
            | "installed"
            | "installing"
            | "loaded"
            | "loading"
            | "progress"
            | "running"
            | "saved"
            | "saving"
            | "started"
            | "starting"
            | "stopped"
            | "stopping"
            | "trace"
            | "uploading"
            | "warning"
    )
}

fn looks_like_repl_prompt(trimmed: &str) -> bool {
    if matches!(trimmed, ">" | ">>>" | "..." | "=>" | "->") {
        return true;
    }
    let len = trimmed.chars().count();
    if len > 40 || trimmed.split_whitespace().count() > 1 {
        return false;
    }
    trimmed.ends_with('>') || trimmed.ends_with("=>") || trimmed.ends_with("->")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tail_detects_child_input_prompts() {
        let mut tail = OutputTail::default();
        tail.feed(b"\x1b]133;C\x07name? ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"\x1b[31mPassword: \x1b[0m");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"name: ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"mysql> ");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Press ENTER to continue");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Press any key to continue");
        assert!(tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"downloaded 42%");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Downloading: ");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"Error: ");
        assert!(!tail.looks_like_input_prompt());

        tail.clear();
        tail.feed(b"next -> ");
        assert!(!tail.looks_like_input_prompt());
    }
}
