//! Readline-style editing of one line with a byte cursor. The queue editor and
//! the shell-prompt mirror both use these, so cmdq's idea of where a word
//! starts matches between what it edits and what it predicts the shell edits.

pub(crate) fn prev_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

pub(crate) fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| cursor + i)
        .unwrap_or(text.len())
}

pub(crate) fn previous_word_start(text: &str, cursor: usize) -> usize {
    let char_before = |at: usize| text[..at].chars().next_back().unwrap_or(' ');
    let mut start = cursor;
    while start > 0 && char_before(start).is_whitespace() {
        start = prev_boundary(text, start);
    }
    while start > 0 && !char_before(start).is_whitespace() {
        start = prev_boundary(text, start);
    }
    start
}

pub(crate) fn next_word_end(text: &str, cursor: usize) -> usize {
    let char_at = |at: usize| text[at..].chars().next().unwrap_or(' ');
    let mut end = cursor;
    while end < text.len() && char_at(end).is_whitespace() {
        end = next_boundary(text, end);
    }
    while end < text.len() && !char_at(end).is_whitespace() {
        end = next_boundary(text, end);
    }
    end
}

pub(crate) fn insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    text.insert_str(*cursor, s);
    *cursor += s.len();
}

pub(crate) fn backspace(text: &mut String, cursor: &mut usize) {
    let prev = prev_boundary(text, *cursor);
    text.replace_range(prev..*cursor, "");
    *cursor = prev;
}

pub(crate) fn delete_at_cursor(text: &mut String, cursor: usize) {
    let next = next_boundary(text, cursor);
    text.replace_range(cursor..next, "");
}

pub(crate) fn delete_previous_word(text: &mut String, cursor: &mut usize) {
    let start = previous_word_start(text, *cursor);
    text.replace_range(start..*cursor, "");
    *cursor = start;
}

pub(crate) fn kill_to_start(text: &mut String, cursor: &mut usize) {
    text.replace_range(..*cursor, "");
    *cursor = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_motion_skips_whitespace_then_one_word() {
        let text = "git  commit -m 界🙂x";
        assert_eq!(
            previous_word_start(text, text.len()),
            "git  commit -m ".len()
        );
        assert_eq!(previous_word_start(text, "git  ".len()), 0);
        assert_eq!(next_word_end(text, 3), "git  commit".len());
        assert_eq!(next_word_end(text, text.len()), text.len());
    }

    #[test]
    fn edits_respect_multibyte_characters() {
        let mut text = String::from("a界b");
        let mut cursor = "a界".len();
        backspace(&mut text, &mut cursor);
        assert_eq!((text.as_str(), cursor), ("ab", 1));
        delete_at_cursor(&mut text, cursor);
        assert_eq!(text, "a");
        insert_str(&mut text, &mut cursor, "🙂 x");
        delete_previous_word(&mut text, &mut cursor);
        assert_eq!((text.as_str(), cursor), ("a🙂 ", "a🙂 ".len()));
        kill_to_start(&mut text, &mut cursor);
        assert_eq!((text.as_str(), cursor), ("", 0));
    }
}
