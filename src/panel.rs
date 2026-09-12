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
    pub conditional: bool,
    pub shell_input: bool,
    pub activity: Option<&'a str>,
    pub can_recover: bool,
    pub status: &'a str,
    pub pending_quit: bool,
    pub pending_clear: bool,
    pub show_help: bool,
    pub max_queue_visible: u16,
}

/// The queue itself confirms routine actions. Only actionable feedback needs a row.
fn panel_notice<'a>(view: &'a PanelState<'_>) -> Option<&'a str> {
    // These states are already explained by the header or confirmation hint.
    if view.queue.paused
        && !view.shell_input
        && !view.passthrough_to_child
        && !view.child_input_prompt
        && view.editing_index.is_none()
        && matches!(
            view.status,
            "queue paused" | "queue paused by another session"
        )
    {
        return view.activity;
    }
    if view.pending_clear || view.pending_quit {
        return None;
    }
    let routine = [
        "added:",
        "saved:",
        "dispatched:",
        "queue resumed",
        "Building queue",
        "Typing into",
        "alt-screen",
        "mouse tracking",
        "edit cancelled",
    ]
    .iter()
    .any(|prefix| view.status.starts_with(prefix));
    let outcome_in_activity = view.activity.is_some()
        && (view.status.starts_with("removed:")
            || matches!(view.status, "queue cleared" | "restored to paused queue"));
    if !view.status.is_empty() && !routine && !outcome_in_activity {
        Some(view.status)
    } else {
        view.activity
    }
}

fn notice_rows(view: &PanelState<'_>, height: u16) -> u16 {
    u16::from(height >= 4 && panel_notice(view).is_some())
}

/// Compute how many rows the panel needs given the current state.
///
/// Layout:  header(1) + hints(1) + queue rows + input(1).
///
/// When help is open, the panel expands to fill most of the screen.
pub fn panel_height(view: &PanelState<'_>, total_rows: u16) -> u16 {
    if view.show_help {
        // Leave at least 2 rows of shell visible above so the user keeps
        // some context (e.g. their prompt) while reading the help.
        return total_rows.saturating_sub(2).min(HELP_MAX_ROWS);
    }
    let n = (view.queue.len() as u16).min(view.max_queue_visible);
    1 + n + 1 + 1 + u16::from(panel_notice(view).is_some())
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

    let mut hints = Vec::new();
    if paint_hints(&mut hints, view, old_cols).is_ok() {
        widths.push(ansi_display_width(&hints));
    } else {
        widths.push(old_cols as usize);
    }

    let list_capacity = panel_height.saturating_sub(3 + notice_rows(view, panel_height)) as usize;
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
        let text = queue_row_text(view, i, item);
        widths.push(display_width(&clip_to_width(&text, old_cols as usize)));
    }
    while widths.len() < 2 + list_capacity {
        widths.push(0);
    }

    if let Some(activity) = panel_notice(view).filter(|_| notice_rows(view, panel_height) > 0) {
        widths.push(display_width(&clip_to_width(
            &display_control_chars(activity),
            old_cols as usize,
        )));
    }
    widths.push(display_width(&input_line(view, old_cols as usize).0));

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
        let input_row = top + panel_height.saturating_sub(1);
        let (_, cursor_col) = input_line(view, total_cols as usize);
        let col = (cursor_col as u16).min(total_cols.saturating_sub(1));
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

/// Standard panel layout: header / hints / queue rows / input.
fn paint_normal(
    out: &mut impl Write,
    view: &PanelState<'_>,
    top: u16,
    panel_height: u16,
    total_cols: u16,
) -> io::Result<()> {
    let mut row = top;

    // State lives in the header; actionable feedback has a single notice row.
    // Queue list. Reserve `panel_height - 3` rows for items (header(1) +
    // input(1) + hints(1) accounted for).
    let list_capacity = panel_height.saturating_sub(3 + notice_rows(view, panel_height)) as usize;

    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    paint_header(out, view, total_cols, list_capacity)?;
    row += 1;
    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    paint_hints(out, view, total_cols)?;
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
        let selected = view.editing_index == Some(i);
        out.queue(SetForegroundColor(if selected {
            Color::Cyan
        } else if i == 0 {
            Color::White
        } else {
            Color::Grey
        }))?;
        if selected || i == 0 {
            out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
        }
        let text = queue_row_text(view, i, item);
        out.queue(Print(clip_to_width(&text, total_cols as usize)))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
        out.queue(ResetColor)?;
        row += 1;
    }

    // Pad blank queue rows so the input lands in a stable place
    // regardless of queue length.
    while row < top + panel_height - 1 - notice_rows(view, panel_height) {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
        row += 1;
    }

    if let Some(activity) = panel_notice(view).filter(|_| notice_rows(view, panel_height) > 0) {
        out.queue(MoveTo(0, row))?;
        out.queue(Clear(ClearType::CurrentLine))?;
        out.queue(SetForegroundColor(Color::Yellow))?;
        out.queue(Print(clip_to_width(
            &display_control_chars(activity),
            total_cols as usize,
        )))?;
        out.queue(ResetColor)?;
        row += 1;
    }

    out.queue(MoveTo(0, row))?;
    out.queue(Clear(ClearType::CurrentLine))?;
    let (line, _) = input_line(view, total_cols as usize);
    let typing_above = view.passthrough_to_child || view.shell_input || view.child_input_prompt;
    if typing_above {
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print(line))?;
    } else {
        let prefix = input_prefix(view, total_cols as usize);
        out.queue(SetForegroundColor(Color::Cyan))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
        out.queue(Print(&prefix))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
        if view.input_buffer.is_empty() {
            out.queue(SetForegroundColor(Color::DarkGrey))?;
        } else {
            out.queue(ResetColor)?;
        }
        out.queue(Print(&line[prefix.len()..]))?;
    }
    out.queue(ResetColor)?;

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
    let state = if view.passthrough_to_child || view.child_input_prompt || view.shell_input {
        "↑"
    } else if view.editing_index.is_some() {
        "✎"
    } else if view.queue.paused {
        "Paused"
    } else if view.force_queue && !view.running {
        "○"
    } else if view.running {
        "▶"
    } else {
        "✓"
    };
    let mut header = format!(" {state} ");
    if let Some((first, last, total)) = queue_overflow(view, list_capacity) {
        if first > 1 {
            header.push_str("↑ ");
        }
        if last < total {
            header.push_str("↓ ");
        }
    }
    let header_width = display_width(&header);
    let cols = total_cols as usize;
    out.queue(SetForegroundColor(if view.queue.paused {
        Color::Yellow
    } else if view.running || view.editing_index.is_some() {
        Color::Cyan
    } else {
        Color::Grey
    }))?;
    if header_width >= cols {
        out.queue(Print(clip_to_width(&header, cols)))?;
    } else {
        out.queue(Print(header))?;
        let pad = cols - header_width;
        out.queue(SetForegroundColor(Color::DarkGrey))?;
        out.queue(Print("─".repeat(pad)))?;
    }
    out.queue(ResetColor)?;
    Ok(())
}

fn paint_hints(out: &mut impl Write, view: &PanelState<'_>, total_cols: u16) -> io::Result<()> {
    // Order shortcuts by relevance. Wider terminals expose more controls;
    // narrow terminals retain the primary action and Help without wrapping.
    let actions = if view.pending_quit {
        vec![("Ctrl-D", "Quit (press again)")]
    } else if view.pending_clear {
        vec![("Ctrl-K", "Clear (press again)")]
    } else if view.passthrough_to_child || view.child_input_prompt {
        vec![("F2", "↓ Type here"), ("Ctrl-C", "Stop")]
    } else if view.shell_input {
        let mut actions = vec![("Ctrl-Q", "↓ Type here")];
        if view.can_recover {
            actions.push(("Alt-U", "Undo"));
        }
        actions.push(("F3", "Hide"));
        actions
    } else {
        let mut actions = Vec::new();
        if view.editing_index.is_some() {
            actions.extend([("↵", "Save"), ("Esc", "Cancel"), ("↑/↓", "Select")]);
            actions.extend([("Alt-↑/↓", "Move"), ("Ctrl-D", "Delete")]);
        } else {
            if view.queue.paused {
                actions.push(("Ctrl-X", "Resume"));
            } else if view.force_queue && !view.running && !view.queue.is_empty() {
                actions.push(("Ctrl-X", "Start"));
            } else if !view.queue.is_empty() {
                actions.push(("Ctrl-X", "Pause"));
            }
            if view.running {
                actions.push(("F2", "↑ Type above"));
            } else {
                if !view.queue.paused && !view.force_queue {
                    actions.push((
                        "↵",
                        if view.conditional {
                            "Run if success"
                        } else {
                            "Run"
                        },
                    ));
                }
                actions.push(("Ctrl-Q", "↑ Type above"));
            }
            if !view.queue.is_empty() {
                actions.extend([("↑/↓", "Edit"), ("Ctrl-K", "Clear")]);
            }
        }
        if view.can_recover {
            actions.push(("Alt-U", "Undo"));
        }
        if view.running {
            actions.push(("Ctrl-C", "Stop"));
        }
        actions.push((
            "Alt-S",
            if view.conditional {
                "Always"
            } else {
                "If success"
            },
        ));
        if !view.input_buffer.is_empty() && view.editing_index.is_none() {
            actions.push(("Esc", "Cancel"));
        }
        actions
    };
    let inset = usize::from(total_cols > 2);
    let width = (total_cols as usize).saturating_sub(inset * 2);
    let help = ("F1", "Help");
    let hint_width = |(key, label): (&str, &str)| display_width(key) + 1 + display_width(label);
    let gap = 3;
    let mut fitted = Vec::new();
    let mut used = 0;
    // Reserve Help before adding secondary actions. Never cut a shortcut in half.
    let help_budget = if view.pending_quit || view.pending_clear {
        0
    } else {
        hint_width(help) + gap
    };
    for action in actions {
        let needed = hint_width(action) + if fitted.is_empty() { 0 } else { gap };
        if used + needed + help_budget <= width {
            used += needed;
            fitted.push(action);
        }
    }
    if help_budget > 0
        && used
            + if fitted.is_empty() {
                hint_width(help)
            } else {
                help_budget
            }
            <= width
    {
        fitted.push(help);
    }
    if inset > 0 {
        out.queue(Print(" "))?;
    }
    // A very narrow terminal may only fit the confirmation key; keep it usable.
    if fitted.is_empty() && (view.pending_quit || view.pending_clear) {
        let key = if view.pending_quit {
            "Ctrl-D"
        } else {
            "Ctrl-K"
        };
        out.queue(Print(clip_to_width(&format!("{key} again"), width)))?;
    }
    let mut painted = 0;
    for (index, &(key, label)) in fitted.iter().enumerate() {
        let spacing = if (key, label) == help {
            width.saturating_sub(painted + hint_width(help))
        } else if index > 0 {
            gap
        } else {
            0
        };
        out.queue(Print(" ".repeat(spacing)))?;
        painted += spacing;
        out.queue(SetForegroundColor(if index == 0 && (key, label) != help {
            Color::Cyan
        } else {
            Color::Grey
        }))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Bold))?;
        out.queue(Print(key))?;
        out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
        out.queue(SetForegroundColor(Color::Grey))?;
        out.queue(Print(format!(" {label}")))?;
        painted += hint_width((key, label));
    }
    out.queue(ResetColor)?;
    Ok(())
}

fn queue_row_text(view: &PanelState<'_>, index: usize, item: &crate::queue::QueueItem) -> String {
    let label = if view.editing_index == Some(index) {
        " ✎  "
    } else if index == 0 {
        " ›  "
    } else {
        " ·  "
    };
    let condition = if item.conditional {
        "[if success] "
    } else {
        ""
    };
    format!("{label}{condition}{}", display_control_chars(&item.command))
}

/// Keep the draft and queued command text aligned, with a distinct editing marker.
fn input_prefix(view: &PanelState<'_>, cols: usize) -> String {
    let mut prompt = if view.editing_index.is_some() {
        " ✎  ".to_string()
    } else {
        " ❯  ".to_string()
    };
    if view.conditional {
        prompt.push_str("if success ");
    }
    // On narrow terminals, keep space for the actual command and cursor.
    let prompt = if display_width(&prompt) + 8 > cols {
        if view.conditional {
            "if success ❯ ".to_string()
        } else {
            "❯ ".to_string()
        }
    } else {
        prompt
    };
    clip_to_width(&prompt, cols.saturating_sub(1))
}

/// Shared by painting, cursor placement and resize cleanup.
fn input_line(view: &PanelState<'_>, cols: usize) -> (String, usize) {
    if view.shell_input || view.passthrough_to_child || view.child_input_prompt {
        let text = if view.input_buffer.is_empty() {
            " ↑ Typing above"
        } else {
            " ↑ Typing above · draft saved"
        };
        return (clip_to_width(text, cols), 0);
    }
    let prompt = input_prefix(view, cols);
    let prefix_width = display_width(&prompt);
    let (display, cursor) = display_control_chars_with_cursor(view.input_buffer, view.input_cursor);
    let remaining = cols.saturating_sub(prefix_width);
    let (text, offset) = input_window(&display, cursor, remaining);
    let text = if view.input_buffer.is_empty() {
        clip_to_width("Next command…", remaining)
    } else {
        text
    };
    (format!("{prompt}{text}"), prefix_width + offset)
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
    out.queue(Print(clip_to_width(
        " cmdq · keyboard shortcuts ",
        total_cols as usize,
    )))?;
    out.queue(SetAttribute(crossterm::style::Attribute::Reset))?;
    out.queue(SetForegroundColor(Color::DarkGrey))?;
    let pad = (total_cols as usize).saturating_sub(" cmdq · keyboard shortcuts ".chars().count());
    out.queue(Print("─".repeat(pad)))?;
    out.queue(ResetColor)?;

    let lines: &[(&str, &str)] = &[
        ("▶ running · ○ not started · ✎ editing", ""),
        ("Enter", "queue command; run at prompt; save when editing"),
        ("Ctrl-X", "pause / resume / start"),
        ("Ctrl-Q", "open queue / return to shell"),
        ("Alt-U", "restore removed or skipped commands, paused"),
        ("F3", "dismiss notice"),
        ("Ctrl-K twice", "clear queue"),
        ("Edit", ""),
        ("↑ / ↓", "select queued command"),
        ("Esc", "cancel edit / clear draft"),
        ("Alt-S", "toggle: always / if previous command succeeds"),
        ("Ctrl-D (editing)", "remove command"),
        ("Alt-↑ / Alt-↓", "reorder command"),
        ("Terminal", ""),
        ("F2", "type above / return to queue; keeps draft"),
        ("Ctrl-C", "interrupt command; pause queue"),
        ("Ctrl-Z", "suspend command"),
        ("Ctrl-\\", "send SIGQUIT / return to queue"),
        ("Ctrl-D", "quit; twice if queued; delete if draft has text"),
        ("F1 / Esc / Enter", "close help"),
    ];
    let avail = panel_height.saturating_sub(1) as usize;
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
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
    fn hints_prioritize_program_input_over_sigquit() {
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
            status: "",
            pending_quit: false,
            pending_clear: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_hints(&mut out, &view, 100).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(printable.contains("F2 ↑ Type above"), "hints={printable:?}");
        assert!(!printable.contains("SIGQUIT"), "hints={printable:?}");
        assert!(!printable.contains("raw"), "hints={printable:?}");
    }

    #[test]
    fn hints_offer_return_to_shell_when_building_queue() {
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
            status: "",
            pending_quit: false,
            pending_clear: false,
            show_help: false,
            max_queue_visible: 8,
        };
        let mut out = Vec::new();

        paint_hints(&mut out, &view, 100).unwrap();

        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            printable.contains("Ctrl-Q ↑ Type above"),
            "hints={printable:?}"
        );
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
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
            conditional: false,
            shell_input: false,
            activity: None,
            can_recover: false,
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
            printable.starts_with(" ▶ ↓ ") && !printable.contains("12"),
            "header={printable:?}"
        );

        view.editing_index = Some(11);
        view.status = "saved: cmd 11";
        let mut out = Vec::new();
        paint_header(&mut out, &view, 100, 8).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            printable.starts_with(" ✎ ↑ ") && !printable.contains("12"),
            "header={printable:?}"
        );
        assert!(!printable.contains("saved: cmd 11"), "header={printable:?}");
    }

    #[test]
    fn header_stays_plain_when_every_item_fits() {
        let mut queue = Queue::new();
        queue.push("cmd", false);
        let view = view_with(&queue, true);
        let mut out = Vec::new();
        paint_header(&mut out, &view, 100, 8).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(
            !printable.contains("↓") && !printable.contains("↑"),
            "header={printable:?}"
        );
    }

    #[test]
    fn narrow_hints_prioritize_resume_and_help() {
        let mut queue = Queue::new();
        queue.push("cmd", false);
        queue.paused = true;
        let view = view_with(&queue, true);
        let mut out = Vec::new();
        paint_hints(&mut out, &view, 80).unwrap();
        let printable = strip_ansi(&String::from_utf8_lossy(&out));
        assert!(printable.contains("Ctrl-X Resume"), "hints={printable:?}");
        assert!(printable.contains("F1 Help"), "hints={printable:?}");
        assert!(!printable.contains("Ctrl-X Pause"), "hints={printable:?}");
    }

    #[test]
    fn narrow_hints_keep_whole_actions() {
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
            assert!(printable.ends_with("F1 Help"), "{cols}: {printable:?}");
            assert!(
                printable.trim_start().starts_with("F2 ↑ Type above")
                    && !printable.contains("Enter"),
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
            assert!(printable.contains("Ctrl-K"), "{cols}: {printable:?}");
            assert!(printable.contains("again"), "{cols}: {printable:?}");
            assert!(!printable.contains("add"), "{cols}: {printable:?}");
        }
    }

    #[test]
    fn pause_is_explained_once_without_an_extra_notice_row() {
        let mut queue = Queue::new();
        queue.push("ls", false);
        queue.paused = true;
        let mut view = view_with(&queue, true);
        view.status = "queue paused by another session";
        let height = panel_height(&view, 30);
        assert_eq!(height, 4);
        let mut out = Vec::new();
        paint(&mut out, &view, height, 30, 120, true, (0, 0)).unwrap();
        let rendered = strip_ansi(&String::from_utf8_lossy(&out));
        assert_eq!(rendered.matches("Paused").count(), 1);
        assert!(!rendered.contains("another session"));
        assert!(rendered.contains("Ctrl-X Resume"));
    }

    #[test]
    fn routine_feedback_is_quiet_but_errors_and_recovery_remain() {
        let queue = Queue::new();
        let mut view = view_with(&queue, true);
        for status in ["added: ls", "saved: ls", "dispatched: ls", "queue resumed"] {
            view.status = status;
            assert_eq!(panel_height(&view, 30), 3);
            assert!(panel_notice(&view).is_none());
        }
        view.status = "queue save failed: disk full";
        assert_eq!(panel_notice(&view), Some(view.status));
        let mut out = Vec::new();
        paint(&mut out, &view, 4, 30, 80, true, (0, 0)).unwrap();
        let rendered = strip_ansi(&String::from_utf8_lossy(&out));
        assert_eq!(rendered.matches(view.status).count(), 1);
        view.status = "queue cleared by another session";
        assert_eq!(panel_notice(&view), Some(view.status));
        view.status = "removed: ls";
        view.activity = Some("Removed ls — Alt-U restores paused");
        assert_eq!(panel_notice(&view), view.activity);
    }

    #[test]
    fn wider_terminals_show_more_shortcuts_with_clear_labels() {
        let mut queue = Queue::new();
        queue.push("ls", false);
        let view = view_with(&queue, true);
        let mut narrow = Vec::new();
        let mut wide = Vec::new();
        paint_hints(&mut narrow, &view, 40).unwrap();
        paint_hints(&mut wide, &view, 120).unwrap();
        let narrow = strip_ansi(&String::from_utf8_lossy(&narrow));
        let wide = strip_ansi(&String::from_utf8_lossy(&wide));
        assert!(narrow.contains("Ctrl-X Pause"));
        assert!(narrow.contains("F1 Help"));
        for hint in [
            "Ctrl-X Pause",
            "↑/↓ Edit",
            "Ctrl-K Clear",
            "Ctrl-C Stop",
            "Alt-S If success",
            "F1 Help",
        ] {
            assert!(wide.contains(hint), "missing {hint}: {wide}");
        }
        assert!(wide.contains("Ctrl-K Clear") && !narrow.contains("Ctrl-K Clear"));
        assert_eq!(display_width(&wide), 119);
        assert_eq!(display_width(&narrow), 39);
    }

    #[test]
    fn help_fits_a_standard_24_row_terminal() {
        let queue = Queue::new();
        let mut view = view_with(&queue, true);
        view.show_help = true;
        let mut out = Vec::new();
        paint(
            &mut out,
            &view,
            panel_height(&view, 24),
            24,
            80,
            false,
            (0, 0),
        )
        .unwrap();
        let rendered = strip_ansi(&String::from_utf8_lossy(&out));
        for control in ["Alt-S", "Alt-U", "Ctrl-D", "Ctrl-Z", "close help"] {
            assert!(rendered.contains(control), "missing {control}");
        }
    }

    #[test]
    fn hints_fit_even_on_tiny_terminals() {
        let mut queue = Queue::new();
        queue.push("ls", false);
        queue.paused = true;
        let mut view = view_with(&queue, true);
        for pending in [false, true] {
            view.pending_clear = pending;
            for cols in 1..=80 {
                let mut out = Vec::new();
                paint_hints(&mut out, &view, cols).unwrap();
                assert!(ansi_display_width(&out) <= cols as usize);
            }
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
    #[test]
    fn execution_condition_and_edit_position_stay_visible() {
        let mut q = Queue::new();
        q.push("deploy", true);
        let mut v = view_with(&q, true);
        v.conditional = true;
        v.editing_index = Some(0);
        v.input_buffer = "deploy";
        let (line, _) = input_line(&v, 100);
        assert!(line.starts_with(" ✎  "));
        assert!(line.contains("if success"));
        assert!(queue_row_text(&v, 0, &q.items()[0]).contains("if success"));
    }

    #[test]
    fn optional_notice_never_pushes_input_outside_tiny_panel() {
        let q = Queue::new();
        let mut v = view_with(&q, true);
        v.activity = Some("Removed command");
        for rows in [5, 6, 12, 24] {
            let height = panel_height(&v, rows).min(rows - 2);
            let mut out = Vec::new();
            paint(&mut out, &v, height, rows, 24, true, (0, 0)).unwrap();
            let rendered = String::from_utf8_lossy(&out);
            assert!(!rendered.contains(&format!("\x1b[{};", rows + 1)));
        }
    }
}
