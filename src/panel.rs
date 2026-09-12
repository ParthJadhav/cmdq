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
const HELP_MAX_ROWS: u16 = 29;

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
    pub pending_clear: bool,
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

/// Number of physical rows occupied by the currently painted panel after a
/// terminal width change. Terminals reflow screen cells before delivering the
/// resize event, so the old logical panel rows may span several new rows.
pub fn reflowed_panel_height(
    view: &PanelState<'_>,
    panel_height: u16,
    old_cols: u16,
    new_cols: u16,
) -> u16 {
    let new_cols = new_cols.max(1) as usize;
    if view.show_help {
        // Help owns nearly the whole viewport and its title fills the row.
        // Conservatively clear its resized footprint; at most the two shell
        // context rows above it are affected on an extreme shrink.
        let wraps = (old_cols.max(1) as usize).div_ceil(new_cols);
        return (panel_height as usize)
            .saturating_mul(wraps)
            .min(u16::MAX as usize) as u16;
    }

    let mut widths = Vec::with_capacity(panel_height as usize);
    widths.push(old_cols.max(1) as usize); // divider-filled header

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
        let prefix = if view.editing_index == Some(i) {
            " ✎ "
        } else if i == 0 {
            " ▸ "
        } else {
            "   "
        };
        let cond = if item.conditional { "↪ " } else { "  " };
        let text = format!("{prefix}{cond}{}", display_control_chars(&item.command));
        widths.push(display_width(&clip_to_width(&text, old_cols as usize)));
    }
    while widths.len() < 1 + list_capacity {
        widths.push(0);
    }

    let prompt = input_prompt_prefix(view);
    let remaining = (old_cols as usize).saturating_sub(display_width(prompt));
    let (display_input, display_cursor) =
        display_control_chars_with_cursor(view.input_buffer, view.input_cursor);
    let (visible_input, _) = input_window(&display_input, display_cursor, remaining);
    widths.push(display_width(prompt) + display_width(&visible_input));

    let mut hints = Vec::new();
    if paint_hints(&mut hints, view, old_cols).is_ok() {
        widths.push(ansi_display_width(&hints));
    } else {
        widths.push(old_cols as usize);
    }

    widths
        .into_iter()
        .map(|width| width.max(1).div_ceil(new_cols))
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

/// Reserve the bottom `panel_height` rows. Sets the scrolling region above
/// the panel, preserves occupied rows that would otherwise be overwritten,
/// clears the panel rows, and restores the adjusted shell cursor within the
/// scrolling region.
pub fn reserve(
    out: &mut impl Write,
    panel_height: u16,
    total_rows: u16,
    total_cols: u16,
    shell_cursor: (u16, u16),
) -> io::Result<()> {
    if panel_height == 0 || panel_height >= total_rows {
        return reset_scroll_region(out, total_rows);
    }
    let scroll_bottom = total_rows - panel_height;
    let (shell_cursor_col, shell_cursor_row) = shell_cursor;

    // The panel is opened lazily, so the shell may already have printed into
    // the rows we are about to claim. This is especially common after the
    // first command: the next prompt and command line both sit at the bottom
    // of the terminal. Clearing those rows would silently erase terminal
    // history. Scroll just the occupied portion of the claimed area into
    // scrollback before installing the smaller scrolling region.
    let occupied_claimed_rows = shell_cursor_row
        .min(total_rows.saturating_sub(1))
        .saturating_add(1)
        .saturating_sub(scroll_bottom);
    if occupied_claimed_rows > 0 {
        out.queue(MoveTo(0, total_rows.saturating_sub(1)))?;
        for _ in 0..occupied_claimed_rows {
            out.write_all(b"\n")?;
        }
    }

    let adjusted_cursor_row = shell_cursor_row.saturating_sub(occupied_claimed_rows);

    // DECSTBM: confine scrolling to rows 1..=scroll_bottom (1-indexed).
    // NOTE: this also resets the cursor to home (1,1) on xterm-class
    // terminals; the explicit MoveTo below pins it back inside the region.
    write!(out, "\x1b[1;{}r", scroll_bottom)?;
    out.queue(MoveTo(0, scroll_bottom.saturating_sub(1)))?;
    // Clear each panel row so we start from a known blank slate.
    clear_panel_rows(out, panel_height, total_rows, total_cols)?;
    // clear_panel_rows leaves the cursor in the panel area; restore the
    // shell's logical cursor, adjusted for any rows scrolled above. Keeping
    // its original row avoids a large blank jump when the panel first opens
    // after `clear` or while output is still near the top of the screen.
    out.queue(MoveTo(
        shell_cursor_col.min(total_cols.saturating_sub(1)),
        adjusted_cursor_row.min(scroll_bottom.saturating_sub(1)),
    ))?;
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
    // Queue list. Reserve `panel_height - 3` rows for items (header(1) +
    // input(1) + hints(1) accounted for).
    let list_capacity = panel_height.saturating_sub(3) as usize;

    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    paint_header(out, view, total_cols, list_capacity)?;
    row += 1;
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

/// Rows of the queue list that don't fit, as `(first_shown, last_shown, total)`
/// (1-based) — `None` when every item is on screen.
fn queue_overflow(view: &PanelState<'_>, list_capacity: usize) -> Option<(usize, usize, usize)> {
    let total = view.queue.len();
    let shown = total
        .min(list_capacity)
        .min(view.max_queue_visible as usize);
    if shown >= total {
        return None;
    }
    let start = queue_window_start(view, shown);
    Some((start + 1, start + shown, total))
}

fn paint_header(
    out: &mut impl Write,
    view: &PanelState<'_>,
    total_cols: u16,
    list_capacity: usize,
) -> io::Result<()> {
    let mut header = String::from(" cmdq");
    // Without this the list silently truncates and the user has no idea
    // more commands are waiting below the fold.
    if let Some((first, last, total)) = queue_overflow(view, list_capacity) {
        header.push_str(&format!(" · {total} queued, showing {first}–{last}"));
    }
    if !view.status.is_empty() {
        header.push_str(&format!(" │ {}", display_control_chars(view.status)));
    }
    header.push(' ');
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
    let pause_label = if view.queue.paused { "resume" } else { "pause" };
    if total_cols < 88 {
        let pause_chip = format!("[^X {pause_label}]");
        let chips: Vec<&str> = if view.pending_quit {
            vec!["[^D again to quit]"]
        } else if view.pending_clear {
            vec!["[^K again to clear queue]"]
        } else if view.editing_index.is_some() {
            vec![
                "[Esc cancel]",
                "[Enter save]",
                "[^D delete]",
                "[Alt-Up/Down reorder]",
                "[Tab chain]",
            ]
        } else if view.child_input_prompt {
            vec![
                "[keys -> child]",
                "[Enter submit]",
                "[^C interrupt]",
                "[F1 help]",
            ]
        } else if view.passthrough_to_child {
            vec!["[keys -> child]", "[Esc Esc / ^\\ exit raw]"]
        } else {
            vec![
                "[Enter add]",
                "[Up edit]",
                "[Tab chain]",
                &pause_chip,
                "[^K clear]",
                if view.running {
                    "[^\\ SIGQUIT]"
                } else {
                    "[Esc Esc raw]"
                },
                "[? help]",
            ]
        };
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print(fit_chips(&chips, total_cols as usize)))?;
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
    if view.pending_clear {
        chip(out, "^K", "again to clear the queue")?;
        sep(out)?;
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print("any other key keeps it"))?;
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

    if view.child_input_prompt {
        chip(out, "keys", "to child")?;
        gap(out)?;
        chip(out, "⏎", "submit")?;
        gap(out)?;
        chip(out, "^C", "interrupt")?;
        sep(out)?;
        chip(out, "F1", "help")?;
        return Ok(());
    }

    if view.passthrough_to_child {
        chip(out, "keys", "to child")?;
        sep(out)?;
        chip(out, "Esc Esc", "exit raw")?;
        gap(out)?;
        chip(out, "^\\", "exit raw")?;
        return Ok(());
    }

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

/// Join as many whole chips as fit in `width`, so a narrow terminal drops
/// trailing hints instead of cutting one in half.
fn fit_chips(chips: &[&str], width: usize) -> String {
    let mut line = String::new();
    for chip in chips {
        let needed = display_width(&line) + usize::from(!line.is_empty()) + display_width(chip);
        if needed > width {
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(chip);
    }
    if line.is_empty() {
        return clip_to_width(chips.first().copied().unwrap_or(""), width);
    }
    line
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

    // The full layout fits in HELP_MAX_ROWS = 29 lines including the title.
    // A compact layout keeps every essential shortcut visible in a standard
    // 24-row terminal (21 content rows after leaving shell context + title).
    let full_lines: &[(&str, &str)] = &[
        (
            "",
            "panel appears 1.5s into a long command. ↑ recalls the QUEUE.",
        ),
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
        ("Ctrl-K", "clear the queue (press twice to confirm)"),
        ("", ""),
        ("modes", ""),
        ("Ctrl-Q", "force the panel open at the shell prompt"),
        ("Esc Esc", "raw input — keys go to the running app"),
        ("Ctrl-\\", "SIGQUIT running command · exits raw input"),
        ("", ""),
        ("misc", ""),
        ("Ctrl-C", "SIGINT the running command (pauses queue)"),
        ("Ctrl-Z", "suspend the running command"),
        ("Ctrl-D", "quit cmdq (twice if queue is non-empty)"),
        ("Ctrl/Alt edit keys", "line movement, backspace, word-jump"),
        ("F1 / ?", "show this help · Esc / Enter dismisses"),
    ];

    let compact_lines: &[(&str, &str)] = &[
        ("add to queue", ""),
        ("Enter", "add the typed command to the queue"),
        ("Tab", "chain — only run if previous succeeded"),
        ("Esc", "clear the input buffer"),
        ("edit a queued item", ""),
        ("↑ / ↓", "open previous / next queued item for edit"),
        ("Enter", "save the edit"),
        ("Esc", "cancel the edit (item unchanged)"),
        ("Ctrl-D", "delete the item being edited"),
        ("Alt-↑ / Alt-↓", "reorder the item being edited"),
        ("queue control", ""),
        ("Ctrl-X", "pause / resume auto-dispatch"),
        ("Ctrl-K", "clear the queue (press twice to confirm)"),
        ("modes", ""),
        ("Ctrl-Q", "force the panel open at the shell prompt"),
        ("Esc Esc", "raw input — keys go to the running app"),
        ("Ctrl-\\", "SIGQUIT running command · exits raw input"),
        ("misc", ""),
        ("Ctrl-C", "SIGINT the running command (pauses queue)"),
        ("Ctrl-D", "quit cmdq (twice if queue is non-empty)"),
        ("F1 / ?", "show this help · Esc / Enter dismisses"),
    ];

    let small_lines: &[(&str, &str)] = &[
        ("add to queue", ""),
        ("Enter / Tab / Esc", "add / chain / clear input"),
        ("edit queued item", ""),
        ("↑ / ↓", "select previous / next queued item"),
        ("Enter / Esc", "save / cancel edit"),
        ("Ctrl-D", "delete edited item"),
        ("Alt-↑ / Alt-↓", "reorder edited item"),
        ("queue control", ""),
        ("Ctrl-X / Ctrl-K", "pause or resume / clear queue"),
        ("modes", ""),
        ("Ctrl-Q", "force panel open at shell prompt"),
        ("Esc Esc / Ctrl-\\", "raw input / SIGQUIT or exit raw"),
        ("misc", ""),
        (
            "Ctrl-C / Ctrl-D",
            "SIGINT + pause / quit (confirm if queued)",
        ),
        ("F1 / ?", "show help · Esc / Enter dismisses"),
    ];

    let avail = panel_height.saturating_sub(1) as usize;
    let lines = if avail >= full_lines.len() {
        full_lines
    } else if avail >= compact_lines.len() {
        compact_lines
    } else {
        small_lines
    };
    let visible = avail.min(lines.len());
    for i in 0..visible {
        // On very short terminals, always keep the dismissal hint visible
        // instead of trapping the user in a help view with no visible exit.
        let (k, d) = if lines.len() > avail && i + 1 == visible {
            lines[lines.len() - 1]
        } else {
            lines[i]
        };
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
        let description_gap = usize::from((total_cols as usize) > key_width);
        if description_gap > 0 {
            out.queue(Print(" "))?;
        }
        out.queue(Print(clip_to_width(
            d,
            (total_cols as usize).saturating_sub(key_width + description_gap),
        )))?;
        out.queue(ResetColor)?;
    }
    Ok(())
}

fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn ansi_display_width(bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(bytes);
    let mut printable = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            printable.push(ch);
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
    display_width(&printable)
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
            pending_clear: false,
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
            pending_clear: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_header(&mut out, &view, 18, 8).unwrap();

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
            pending_clear: false,
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
            pending_clear: false,
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
            pending_clear: false,
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
            pending_clear: false,
            show_help: false,
            max_queue_visible: 8,
        };

        assert_eq!(queue_window_start(&view, 8), 4);
    }

    fn view_with<'a>(queue: &'a Queue, running: bool) -> PanelState<'a> {
        PanelState {
            queue,
            running,
            force_queue: false,
            passthrough_to_child: false,
            child_input_prompt: false,
            input_buffer: "",
            input_cursor: 0,
            editing_index: None,
            status: "",
            pending_quit: false,
            pending_clear: false,
            show_help: false,
            max_queue_visible: 8,
        }
    }

    #[test]
    fn header_reports_hidden_queue_items() {
        let mut queue = Queue::new();
        for i in 0..12 {
            queue.push(format!("cmd {i}"), false);
        }
        let mut view = view_with(&queue, true);
        let mut out = Vec::new();
        paint_header(&mut out, &view, 100, 8).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            printable.contains("12 queued, showing 1–8"),
            "header={printable:?}"
        );

        view.editing_index = Some(11);
        view.status = "saved: cmd 11";
        let mut out = Vec::new();
        paint_header(&mut out, &view, 100, 8).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            printable.contains("12 queued, showing 5–12"),
            "header={printable:?}"
        );
        assert!(printable.contains("saved: cmd 11"), "header={printable:?}");
    }

    #[test]
    fn header_stays_plain_when_every_item_fits() {
        let mut queue = Queue::new();
        queue.push("cmd", false);
        let view = view_with(&queue, true);
        let mut out = Vec::new();
        paint_header(&mut out, &view, 100, 8).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(!printable.contains("queued"), "header={printable:?}");
    }

    #[test]
    fn narrow_hints_track_pause_state_and_offer_clear() {
        let mut queue = Queue::new();
        queue.push("cmd", false);
        queue.paused = true;
        let view = view_with(&queue, true);
        let mut out = Vec::new();
        paint_hints(&mut out, &view, 80).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(printable.contains("[^X resume]"), "hints={printable:?}");
        assert!(printable.contains("[^K clear]"), "hints={printable:?}");
        assert!(!printable.contains("pause"), "hints={printable:?}");
    }

    #[test]
    fn narrow_hints_drop_whole_chips_instead_of_cutting_one() {
        let queue = Queue::new();
        let view = view_with(&queue, true);
        for cols in [40u16, 52, 60, 70, 87] {
            let mut out = Vec::new();
            paint_hints(&mut out, &view, cols).unwrap();
            let printable = strip_ansi(&String::from_utf8_lossy(&out));
            assert!(
                display_width(&printable) <= cols as usize,
                "{cols}: {printable:?}"
            );
            assert!(printable.ends_with(']'), "{cols}: {printable:?}");
            assert!(
                printable.starts_with("[Enter add]"),
                "{cols}: {printable:?}"
            );
        }
    }

    #[test]
    fn pending_clear_hint_replaces_regular_hints() {
        let mut queue = Queue::new();
        queue.push("cmd", false);
        let mut view = view_with(&queue, true);
        view.pending_clear = true;
        for cols in [60u16, 120] {
            let mut out = Vec::new();
            paint_hints(&mut out, &view, cols).unwrap();
            let printable = strip_ansi(&String::from_utf8_lossy(&out));
            assert!(printable.contains("^K"), "{cols}: {printable:?}");
            assert!(printable.contains("again"), "{cols}: {printable:?}");
            assert!(!printable.contains("add"), "{cols}: {printable:?}");
        }
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
