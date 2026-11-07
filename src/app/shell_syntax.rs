//! Guessing from one line of shell input whether the shell will run it or
//! wait for more (open quotes, heredocs, `if` without `fi`, trailing `|`).

pub(super) fn first_meaningful_submitted_paste_line(
    normalized: &str,
    prompt_buffer: &str,
    prompt_cursor: usize,
) -> Option<String> {
    let mut parts = normalized.split('\n');
    let head = parts.next().unwrap_or_default();
    let before = &prompt_buffer[..prompt_cursor];
    let after = &prompt_buffer[prompt_cursor..];
    let first = format!("{before}{head}{after}");
    if !is_shell_noop_line(&first) {
        return Some(first);
    }

    let rest: Vec<&str> = parts.collect();
    let submitted_rest = if normalized.ends_with('\n') {
        rest.len()
    } else {
        rest.len().saturating_sub(1)
    };
    rest.into_iter()
        .take(submitted_rest)
        .find(|line| !is_shell_noop_line(line))
        .map(str::to_string)
}

fn is_shell_noop_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

pub(super) fn is_likely_complete_shell_command(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.ends_with('\\')
        || trimmed.ends_with('|')
        || trimmed.ends_with("&&")
        || trimmed.ends_with("||")
        || has_unclosed_quote(trimmed)
        || contains_heredoc_operator(trimmed)
        || has_unclosed_compound_construct(trimmed)
        || has_unclosed_shell_grouping(trimmed)
    {
        return false;
    }

    let last_word = trimmed
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
        .rfind(|s| !s.is_empty())
        .unwrap_or("");
    !matches!(last_word, "do" | "then" | "else" | "elif" | "case")
}

pub(super) fn line_may_start_shell_continuation(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !is_likely_complete_shell_command(trimmed)
}

fn has_unclosed_compound_construct(line: &str) -> bool {
    match first_shell_word(line) {
        Some("if") => !shell_keyword_present(line, "fi"),
        Some("for" | "while" | "until" | "select") => !shell_keyword_present(line, "done"),
        Some("case") => !shell_keyword_present(line, "esac"),
        _ => has_unclosed_brace_block(line),
    }
}

fn first_shell_word(line: &str) -> Option<&str> {
    shell_words(line).into_iter().next()
}

fn shell_keyword_present(line: &str, needle: &str) -> bool {
    shell_words(line).into_iter().any(|word| word == needle)
}

fn shell_words(line: &str) -> Vec<&str> {
    line.split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '{' | '}'))
        .filter(|s| !s.is_empty())
        .collect()
}

fn has_unclosed_brace_block(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;

    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '{' if !in_single && !in_double => depth += 1,
            '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            _ => {}
        }
    }

    depth > 0
}

fn has_unclosed_shell_grouping(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut depth = 0i32;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '$' if !in_single && chars.peek() == Some(&'(') => {
                chars.next();
                depth += 1;
            }
            '(' if !in_single && !in_double => depth += 1,
            ')' if !in_single && depth > 0 => depth -= 1,
            _ => {}
        }
    }

    depth > 0
}

fn has_unclosed_quote(line: &str) -> bool {
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    for c in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && !in_single {
            escaped = true;
            continue;
        }
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            _ => {}
        }
    }

    in_single || in_double || escaped
}

fn contains_heredoc_operator(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut escaped = false;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'\\' && !in_single {
            escaped = true;
            i += 1;
            continue;
        }
        match b {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'<' if !in_single && !in_double && bytes.get(i + 1) == Some(&b'<') => {
                return bytes.get(i + 2) != Some(&b'<');
            }
            _ => {}
        }
        i += 1;
    }
    false
}
