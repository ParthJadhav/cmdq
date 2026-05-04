//! Bottom-strip panel painter for cmdq.
//!
//! Path A architecture: cmdq is **not** a terminal emulator. The shell's
//! output streams straight through to the user's real terminal. cmdq only
//! owns the bottom few rows and paints them via direct crossterm calls.
//!
//! The painter handles three lifecycle moments:
//!
//! * [`reserve`] sets a DECSTBM scrolling region above the panel rows and
//!   clears the panel rows so they're a known blank slate. Future shell
//!   output will scroll *within* the region above.
//! * [`paint`]   draws the panel chrome (header, queue list, input line,
//!   hints) into the reserved rows. It restores the shell cursor by absolute
//!   position instead of using terminal-global save/restore slots.
//! * [`release`] resets the scrolling region to the full screen and blanks
//!   the panel rows, so the user's shell prompt comes back to a clean tail.
//!
//! Help is rendered in-panel by expanding panel height — there is no
//! floating overlay because we aren't compositing into a virtual screen.

use std::io::{self, Write};

use crossterm::{
    QueueableCommand,
    cursor::{Hide, MoveTo, Show},
    style::{Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{Clear, ClearType},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::queue::Queue;

/// The largest help panel we'll render. The actual height is clamped to
/// what fits above the bottom edge.
const HELP_MAX_ROWS: u16 = 28;

/// Snapshot of the cmdq state the panel needs to render itself. Borrowed
/// per-frame; no allocation in the hot path beyond what crossterm does.
pub struct PanelState<'a> {
    pub queue: &'a Queue,
    pub running: bool,
    pub force_queue: bool,
    pub passthrough_to_child: bool,
    pub child_input_prompt: bool,
    pub input_buffer: &'a str,
    pub input_cursor: usize,
    pub editing_index: Option<usize>,
    pub status: &'a str,
    pub pending_quit: bool,
    pub show_help: bool,
    pub max_queue_visible: u16,
}

/// Compute how many rows the panel needs given the current state.
///
/// Layout:  header(1) + queue rows + input(1) + hints(1).
///
/// When help is open, the panel expands to fill most of the screen.
pub fn panel_height(view: &PanelState<'_>, total_rows: u16) -> u16 {
    if view.show_help {
        // Leave at least 2 rows of shell visible above so the user keeps
        // some context (e.g. their prompt) while reading the help.
        return total_rows.saturating_sub(2).min(HELP_MAX_ROWS);
    }
    let n = (view.queue.len() as u16).min(view.max_queue_visible);
    1 + n + 1 + 1
}

/// Reserve the bottom `panel_height` rows. Sets the scrolling region above
/// the panel and clears the panel rows. After return, the cursor is left in
/// the top-left of the scrolling region (where the shell expects to write).
pub fn reserve(
    out: &mut impl Write,
    panel_height: u16,
    total_rows: u16,
    total_cols: u16,
) -> io::Result<()> {
    if panel_height == 0 || panel_height >= total_rows {
        return reset_scroll_region(out, total_rows);
    }
    let scroll_bottom = total_rows - panel_height;
    // DECSTBM: confine scrolling to rows 1..=scroll_bottom (1-indexed).
    // NOTE: this also resets the cursor to home (1,1) on xterm-class
    // terminals; the explicit MoveTo below pins it back inside the region.
    write!(out, "\x1b[1;{}r", scroll_bottom)?;
    out.queue(MoveTo(0, scroll_bottom.saturating_sub(1)))?;
    // Clear each panel row so we start from a known blank slate.
    clear_panel_rows(out, panel_height, total_rows, total_cols)?;
    // clear_panel_rows leaves the cursor in the panel area; bring it back
    // inside the scroll region so any subsequent shell bytes / SIGWINCH
    // redraw land where the shell expects.
    out.queue(MoveTo(0, scroll_bottom.saturating_sub(1)))?;
    out.flush()
}

/// Reset the scrolling region to the full screen and blank the panel rows,
/// so the next shell prompt has a clean tail. After return, the cursor sits
/// on the bottom row of the now-full screen — that's where the inner shell's
/// SIGWINCH-driven prompt redraw will land, so it shows up at the bottom of
/// the terminal (where the user expects) rather than at home (1,1) which is
/// where DECSTBM otherwise parks the cursor.
pub fn release(
    out: &mut impl Write,
    panel_height: u16,
    total_rows: u16,
    total_cols: u16,
) -> io::Result<()> {
    if panel_height > 0 && panel_height < total_rows {
        clear_panel_rows(out, panel_height, total_rows, total_cols)?;
    }
    reset_scroll_region(out, total_rows)?;
    // CRITICAL: DECSTBM (\x1b[r) moves the cursor to home (1,1) on
    // xterm-class terminals (incl. tmux). If we leave the cursor there, the
    // shell's next bytes — and any SIGWINCH-driven prompt redraw — write
    // at row 1 of the terminal, producing an orphan prompt at the top and
    // visually shifting prior content down by one row. Pin the cursor to
    // the bottom of the now-full screen instead.
    out.queue(MoveTo(0, total_rows.saturating_sub(1)))?;
    out.flush()
}

fn reset_scroll_region(out: &mut impl Write, total_rows: u16) -> io::Result<()> {
    // Empty parameters mean "reset to full screen" on every conformant
    // terminal — but be explicit about the bounds anyway, so the call
    // produces the same result on terminals that interpret `\x1b[r` as a
    // no-op.
    write!(out, "\x1b[1;{}r", total_rows)?;
    Ok(())
}

fn clear_panel_rows(
    out: &mut impl Write,
    panel_height: u16,
    total_rows: u16,
    _total_cols: u16,
) -> io::Result<()> {
    let top = total_rows - panel_height;
    for row in 0..panel_height {
        out.queue(MoveTo(0, top + row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

/// Paint the panel into the bottom `panel_height` rows.
///
/// `cursor_in_input` controls whether the cursor is left on the input line
/// (true: cmdq has keyboard focus, user is typing) or restored to wherever
/// the shell last placed it (false: shell has focus, e.g. prompt cursor).
pub fn paint(
    out: &mut impl Write,
    view: &PanelState<'_>,
    panel_height: u16,
    total_rows: u16,
    total_cols: u16,
    cursor_in_input: bool,
    shell_cursor: (u16, u16),
) -> io::Result<()> {
    if panel_height == 0 || panel_height >= total_rows {
        return Ok(());
    }
    let top = total_rows - panel_height;

    out.queue(Hide)?;

    if view.show_help {
        paint_help(out, view, top, panel_height, total_cols)?;
    } else {
        paint_normal(out, view, top, panel_height, total_cols)?;
    }

    // Cursor placement:
    //  * cursor_in_input → put it where the user is editing.
    //  * else            → restore the shell cursor by absolute position.
    if cursor_in_input && !view.show_help {
        let prompt = input_prompt_prefix(view);
        let input_row = top + panel_height.saturating_sub(2);
        let remaining = (total_cols as usize).saturating_sub(display_width(prompt));
        let (display_input, display_cursor) =
            display_control_chars_with_cursor(view.input_buffer, view.input_cursor);
        let (_, input_cursor_col) = input_window(&display_input, display_cursor, remaining);
        // Cursor sits after the prompt + the visual cursor offset within
        // the input buffer. Clamp to viewport width.
        let col = (display_width(prompt) as u16)
            .saturating_add(input_cursor_col as u16)
            .min(total_cols.saturating_sub(1));
        out.queue(MoveTo(col, input_row))?;
        out.queue(Show)?;
    } else {
        let (col, row) = shell_cursor;
        out.queue(MoveTo(
            col.min(total_cols.saturating_sub(1)),
            row.min(total_rows.saturating_sub(1)),
        ))?;
        out.queue(Show)?;
    }

    out.flush()
}

/// Standard panel layout: header / queue rows / input / hints.
fn paint_normal(
    out: &mut impl Write,
    view: &PanelState<'_>,
    top: u16,
    panel_height: u16,
    total_cols: u16,
) -> io::Result<()> {
    let mut row = top;

    // Header: dim divider that fills the row, optionally embedding the
    // current status message.
    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    paint_header(out, view, total_cols)?;
    row += 1;

    // Queue list. Reserve `panel_height - 3` rows for items (header(1) +
    // input(1) + hints(1) accounted for).
    let list_capacity = panel_height.saturating_sub(3) as usize;
    let queue_start = queue_window_start(view, list_capacity);
    for (i, item) in view
        .queue
        .items()
        .iter()
        .enumerate()
        .skip(queue_start)
        .take(list_capacity)
        .take(view.max_queue_visible as usize)
    {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
        let prefix = if view.editing_index == Some(i) {
            " ✎ "
        } else if i == 0 {
            " ▸ "
        } else {
            "   "
        };
        let cond = if item.conditional { "↪ " } else { "  " };
        let style_color = if i == 0 {
            Color::Yellow
        } else if item.conditional {
            Color::Cyan
        } else {
            Color::Grey
        };
        out.queue(SetForegroundColor(style_color))?;
        if i == 0 {
            out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
        }
        let text = format!("{prefix}{cond}{}", display_control_chars(&item.command));
        out.queue(Print(clip_to_width(&text, total_cols as usize)))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
        out.queue(ResetColor)?;
        row += 1;
    }

    // Pad blank queue rows so the input / hints land in a stable place
    // regardless of queue length.
    while row < top + panel_height - 2 {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
        row += 1;
    }

    // Input line.
    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    let prompt = input_prompt_prefix(view);
    let prompt_color = if view.passthrough_to_child {
        Color::Red
    } else {
        Color::Green
    };
    out.queue(SetForegroundColor(prompt_color))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
    out.queue(Print(prompt))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
    out.queue(ResetColor)?;
    let remaining = (total_cols as usize).saturating_sub(display_width(prompt));
    let (display_input, display_cursor) =
        display_control_chars_with_cursor(view.input_buffer, view.input_cursor);
    let (visible_input, _) = input_window(&display_input, display_cursor, remaining);
    out.queue(Print(visible_input))?;
    row += 1;

    // Hints line.
    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    paint_hints(out, view, total_cols)?;

    Ok(())
}

fn queue_window_start(view: &PanelState<'_>, list_capacity: usize) -> usize {
    if list_capacity == 0 {
        return 0;
    }
    let max_start = view.queue.len().saturating_sub(list_capacity);
    view.editing_index
        .map(|idx| idx.saturating_add(1).saturating_sub(list_capacity))
        .unwrap_or(0)
        .min(max_start)
}

fn paint_header(out: &mut impl Write, view: &PanelState<'_>, total_cols: u16) -> io::Result<()> {
    let header = if view.status.is_empty() {
        " cmdq ".to_string()
    } else {
        format!(" cmdq │ {} ", display_control_chars(view.status))
    };
    let header_width = display_width(&header);
    let cols = total_cols as usize;
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    if header_width >= cols {
        out.queue(Print(clip_to_width(&header, cols)))?;
    } else {
        out.queue(Print(header))?;
        let pad = cols - header_width;
        out.queue(Print("─".repeat(pad)))?;
    }
    out.queue(ResetColor)?;
    Ok(())
}

fn paint_hints(out: &mut impl Write, view: &PanelState<'_>, total_cols: u16) -> io::Result<()> {
    if total_cols < 88 {
        let hint = if view.pending_quit {
            "[^D again to quit]"
        } else if view.editing_index.is_some() {
            "[Esc cancel] [Enter save] [^D delete] [Alt-Up/Down reorder]"
        } else if view.running {
            "[Enter add] [Up edit] [Tab chain] [^X pause] [^\\ SIGQUIT] [? help]"
        } else {
            "[Enter add] [Up edit] [Tab chain] [^X pause] [Esc Esc raw] [? help]"
        };
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print(clip_to_width(hint, total_cols as usize)))?;
        out.queue(ResetColor)?;
        return Ok(());
    }

    out.queue(Print(" "))?;
    if view.pending_quit {
        chip(out, "^D", "again to quit")?;
        sep(out)?;
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print("any other key keeps working"))?;
        out.queue(ResetColor)?;
        return Ok(());
    }
    if view.editing_index.is_some() {
        chip(out, "Esc", "cancel")?;
        gap(out)?;
        chip(out, "⏎", "save")?;
        gap(out)?;
        chip(out, "^D", "delete")?;
        gap(out)?;
        chip(out, "Alt-↑↓", "reorder")?;
        gap(out)?;
        chip(out, "⇥", "chain")?;
        return Ok(());
    }

    let pause_label = if view.queue.paused { "resume" } else { "pause" };
    chip(out, "⏎", "add")?;
    gap(out)?;
    chip(out, "↑", "edit")?;
    gap(out)?;
    chip(out, "⇥", "chain")?;
    sep(out)?;
    chip(out, "^X", pause_label)?;
    gap(out)?;
    chip(out, "^K", "clear")?;
    gap(out)?;
    if view.running {
        chip(out, "^\\", "SIGQUIT")?;
    } else {
        chip(out, "Esc Esc", "raw")?;
    }
    sep(out)?;
    chip(out, "?", "help")?;
    Ok(())
}

fn chip(out: &mut impl Write, key: &str, label: &str) -> io::Result<()> {
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    out.queue(Print("["))?;
    out.queue(SetForegroundColor(Color::Cyan))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
    out.queue(Print(key))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
    out.queue(SetForegroundColor(Color::Grey))?;
    out.queue(Print(format!(" {label}")))?;
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    out.queue(Print("]"))?;
    out.queue(ResetColor)?;
    Ok(())
}

fn sep(out: &mut impl Write) -> io::Result<()> {
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    out.queue(Print("  ·  "))?;
    out.queue(ResetColor)?;
    Ok(())
}

fn gap(out: &mut impl Write) -> io::Result<()> {
    out.queue(Print(" "))?;
    Ok(())
}

fn input_prompt_prefix(view: &PanelState<'_>) -> &'static str {
    if view.child_input_prompt {
        "child input> "
    } else if view.passthrough_to_child {
        "raw input> "
    } else if view.editing_index.is_some() {
        "edit> "
    } else if view.queue.paused && view.force_queue {
        "force-queue (paused)> "
    } else if view.queue.paused {
        "queue (paused)> "
    } else if view.force_queue {
        "force-queue> "
    } else {
        "queue> "
    }
}

/// In-panel help: a tall list of shortcuts. When help is shown the panel
/// occupies most of the screen so we have room for everything.
fn paint_help(
    out: &mut impl Write,
    _view: &PanelState<'_>,
    top: u16,
    panel_height: u16,
    total_cols: u16,
) -> io::Result<()> {
    // Title row.
    out.queue(MoveTo(0, top))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    out.queue(SetForegroundColor(Color::Yellow))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
    out.queue(Print(" cmdq · keyboard shortcuts "))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    let pad = (total_cols as usize).saturating_sub(" cmdq · keyboard shortcuts ".chars().count());
    out.queue(Print("─".repeat(pad)))?;
    out.queue(ResetColor)?;

    // Help is laid out to fit in HELP_MAX_ROWS = 28 lines including the
    // title row at the top. Section headers + entries below; spacer rows
    // are minimal so nothing falls off the bottom.
    let lines: &[(&str, &str)] = &[
        (
            "",
            "panel appears 1.5s into a long command. ↑ recalls the QUEUE.",
        ),
        ("", ""),
        ("add to queue", ""),
        ("Enter", "add the typed command to the queue"),
        ("Tab", "chain — only run if previous succeeded"),
        ("Esc", "clear the input buffer"),
        ("", ""),
        ("edit a queued item", ""),
        ("↑ / ↓", "open previous / next queued item for edit"),
        ("Enter", "save the edit"),
        ("Esc", "cancel the edit (item unchanged)"),
        ("Ctrl-D", "delete the item being edited"),
        ("Alt-↑ / Alt-↓", "reorder the item being edited"),
        ("", ""),
        ("queue control", ""),
        ("Ctrl-X", "pause / resume auto-dispatch"),
        ("Ctrl-K", "clear the entire queue"),
        ("", ""),
        ("modes", ""),
        ("Ctrl-Q", "force the panel open at the shell prompt"),
        ("Esc Esc", "raw input — keys go to the running app"),
        ("Ctrl-\\", "SIGQUIT running command · exits raw input"),
        ("", ""),
        ("misc", ""),
        ("Ctrl-C", "SIGINT the running command (pauses queue)"),
        ("Ctrl-D", "quit cmdq (twice if queue is non-empty)"),
        ("Ctrl/Alt edit keys", "line movement, backspace, word-jump"),
        ("F1 / ?", "show this help · Esc / Enter dismisses"),
    ];

    let avail = panel_height.saturating_sub(1) as usize;
    for (i, (k, d)) in lines.iter().take(avail).enumerate() {
        let row = top + 1 + i as u16;
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
        if k.is_empty() && d.is_empty() {
            continue;
        }
        if k.is_empty() {
            // Section note / preamble
            out.queue(SetForegroundColor(Color::Grey))?;
            out.queue(SetAttribute(crossterm::style::Attribute::Italic))?;
            out.queue(Print(clip_to_width(
                &format!("  {}", d),
                total_cols as usize,
            )))?;
            out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
            out.queue(ResetColor)?;
            continue;
        }
        if d.is_empty() {
            // Section header
            out.queue(SetForegroundColor(Color::Yellow))?;
            out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
            out.queue(Print(clip_to_width(k, total_cols as usize)))?;
            out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
            out.queue(ResetColor)?;
            continue;
        }
        let key_width = 20usize.min(total_cols as usize);
        out.queue(SetForegroundColor(Color::Cyan))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
        out.queue(Print(clip_to_width(&format!("  {:<18}", k), key_width)))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print(clip_to_width(
            d,
            (total_cols as usize).saturating_sub(key_width),
        )))?;
        out.queue(ResetColor)?;
    }
    Ok(())
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

fn display_control_chars(s: &str) -> String {
    display_control_chars_with_cursor(s, s.len()).0
}

fn display_control_chars_with_cursor(s: &str, cursor: usize) -> (String, usize) {
    let mut out = String::new();
    let mut display_cursor = 0;
    let mut cursor_recorded = false;
    let cursor = cursor.min(s.len());

    for (idx, c) in s.char_indices() {
        if !cursor_recorded && idx >= cursor {
            display_cursor = out.len();
            cursor_recorded = true;
        }
        match c {
            '\n' => out.push('⏎'),
            '\r' => out.push('␍'),
            '\t' => out.push_str("  "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }

    if !cursor_recorded {
        display_cursor = out.len();
    }
    (out, display_cursor)
}

fn clip_to_width(s: &str, max_width: usize) -> String {
    if display_width(s) <= max_width {
        s.to_string()
    } else {
        let mut out = String::new();
        let mut width = 0;
        for c in s.chars() {
            let cw = char_width(c);
            if width + cw > max_width {
                break;
            }
            width += cw;
            out.push(c);
        }
        out
    }
}

fn input_window(buffer: &str, cursor: usize, max_width: usize) -> (String, usize) {
    if max_width == 0 {
        return (String::new(), 0);
    }

    let mut cursor = cursor.min(buffer.len());
    while cursor > 0 && !buffer.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let (before, after) = buffer.split_at(cursor);
    let before_width = display_width(before);
    let total_width = before_width + display_width(after);
    if total_width <= max_width {
        return (buffer.to_string(), before_width);
    }

    if before_width >= max_width {
        let suffix_budget = max_width.saturating_sub(1);
        let mut suffix = String::new();
        let mut suffix_width = 0;
        for c in before.chars().rev() {
            let cw = char_width(c);
            if suffix_width + cw > suffix_budget {
                break;
            }
            suffix_width += cw;
            suffix.insert(0, c);
        }
        let mut visible = String::from("…");
        visible.push_str(&suffix);
        return (visible, 1 + suffix_width);
    }

    let remaining = max_width.saturating_sub(before_width);
    if remaining == 0 {
        return (before.to_string(), before_width);
    }
    let tail_budget = remaining.saturating_sub(1);
    let mut tail = String::new();
    let mut tail_width = 0;
    for c in after.chars() {
        let cw = char_width(c);
        if tail_width + cw > tail_budget {
            break;
        }
        tail_width += cw;
        tail.push(c);
    }
    let mut visible = before.to_string();
    visible.push_str(&tail);
    visible.push('…');
    (visible, before_width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_input_view_tracks_cursor_tail() {
        let (visible, cursor_col) = input_window("abcdefghijklmnopqrstuvwxyz", 26, 10);
        assert_eq!(visible, "…rstuvwxyz");
        assert_eq!(cursor_col, 10);
    }

    #[test]
    fn input_view_uses_display_width_for_wide_chars() {
        let input = "abc漢字";
        let (visible, cursor_col) = input_window(input, input.len(), 5);
        assert_eq!(visible, "…漢字");
        assert_eq!(cursor_col, 5);
        assert_eq!(display_width(&visible), 5);
    }

    #[test]
    fn clip_to_width_never_splits_wide_chars() {
        assert_eq!(clip_to_width("ab漢字", 4), "ab漢");
        assert_eq!(display_width(&clip_to_width("ab漢字", 4)), 4);
    }

    #[test]
    fn display_control_chars_keeps_panel_single_line() {
        let (display, cursor) = display_control_chars_with_cursor("cat <<EOF\nhi\nEOF", 10);
        assert_eq!(display, "cat <<EOF⏎hi⏎EOF");
        assert_eq!(cursor, "cat <<EOF⏎".len());
        assert!(!display.contains('\n'));
    }

    #[test]
    fn paint_does_not_use_terminal_global_cursor_save_restore() {
        let queue = Queue::new();
        let view = PanelState {
            queue: &queue,
            running: true,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint(&mut out, &view, 3, 30, 100, false, (12, 4)).unwrap();

        assert!(!out.windows(2).any(|w| w == b"\x1b7"));
        assert!(!out.windows(2).any(|w| w == b"\x1b8"));
        assert!(!out.windows(3).any(|w| w == b"\x1b[s"));
        assert!(!out.windows(3).any(|w| w == b"\x1b[u"));
        assert!(
            out.windows(b"\x1b[5;13H".len()).any(|w| w == b"\x1b[5;13H"),
            "expected absolute cursor restore to shell cursor, output: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn wide_status_header_stays_within_terminal_width() {
        let queue = Queue::new();
        let view = PanelState {
            queue: &queue,
            running: true,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "added: echo 界🙂",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_header(&mut out, &view, 18).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            display_width(&printable) <= 18,
            "header should not wrap; printable={printable:?}"
        );
    }

    #[test]
    fn hints_do_not_exceed_width_at_threshold() {
        let queue = Queue::new();
        let view = PanelState {
            queue: &queue,
            running: true,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_hints(&mut out, &view, 72).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            display_width(&printable) <= 72,
            "hints should not wrap; printable={printable:?}"
        );
    }

    #[test]
    fn hints_use_sigquit_label_for_running_commands() {
        let queue = Queue::new();
        let view = PanelState {
            queue: &queue,
            running: true,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_hints(&mut out, &view, 100).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(printable.contains("SIGQUIT"), "hints={printable:?}");
        assert!(!printable.contains("raw"), "hints={printable:?}");
    }

    #[test]
    fn hints_use_raw_toggle_label_when_prompt_owned() {
        let queue = Queue::new();
        let view = PanelState {
            queue: &queue,
            running: false,
            force_queue: true,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_hints(&mut out, &view, 100).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(printable.contains("Esc Esc"), "hints={printable:?}");
        assert!(printable.contains("raw"), "hints={printable:?}");
        assert!(!printable.contains("SIGQUIT"), "hints={printable:?}");
    }

    #[test]
    fn queue_window_follows_edited_item() {
        let mut queue = Queue::new();
        for i in 0..12 {
            queue.push(format!("cmd {i}"), false);
        }
        let view = PanelState {
            queue: &queue,
            running: true,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: Some(11),
            status: "",
            pending_quit: false,
            show_help: false,
            max_queue_visible: 8,
        };

        assert_eq!(queue_window_start(&view, 8), 4);
    }

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\x1b' {
                out.push(ch);
                continue;
            }
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        }
        out
    }
}
