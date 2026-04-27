//! ratatui rendering: top region paints the inner shell (via vt100), bottom
//! region paints the queue panel + input line.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap},
};

use crate::queue::Queue;

/// Visible state for the bottom queue panel.
pub struct QueueViewState<'a> {
    pub queue: &'a Queue,
    pub running: bool,
    pub passthrough_to_child: bool,
    pub force_queue: bool,
    pub input_buffer: &'a str,
    pub input_cursor: usize,
    /// Index of the queue item being edited in place (if any).
    pub editing_index: Option<usize>,
    /// Number of additional lines beyond input/header that may be used to
    /// list queue items. Caller chooses based on terminal height.
    pub max_queue_visible: usize,
    pub status: &'a str,
    /// True if Ctrl-D was pressed once and we're waiting for confirmation.
    pub pending_quit: bool,
}

pub fn render(
    f: &mut Frame<'_>,
    parser: &vt100::Parser,
    qview: &QueueViewState<'_>,
    show_queue_panel: bool,
) {
    let area = f.area();
    if area.width < 4 || area.height < 4 {
        return;
    }

    let panel_height = if show_queue_panel {
        compute_panel_height(qview).min(area.height.saturating_sub(2))
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(panel_height)])
        .split(area);

    let shell_area = chunks[0];
    f.render_widget(VtScreen { parser }, shell_area);

    if panel_height > 0 {
        let panel_area = chunks[1];
        render_panel(f, panel_area, qview);
    }

    // Place the OS cursor: when queue panel is visible & we're capturing
    // keystrokes, place the cursor in the input line. Otherwise track the
    // shell's own cursor.
    if show_queue_panel && !qview.passthrough_to_child {
        // Cursor goes on the input line, which is the second-to-last row of
        // the panel (last row is the hints line).
        let prompt = input_prompt_prefix(qview);
        let input_row = chunks[1].y + chunks[1].height.saturating_sub(2);
        let col = chunks[1].x + (prompt.len() as u16) + (qview.input_cursor as u16);
        let col = col.min(chunks[1].x + chunks[1].width.saturating_sub(1));
        f.set_cursor_position((col, input_row));
    } else {
        let (cur_row, cur_col) = parser.screen().cursor_position();
        let x = shell_area.x + cur_col.min(shell_area.width.saturating_sub(1));
        let y = shell_area.y + cur_row.min(shell_area.height.saturating_sub(1));
        f.set_cursor_position((x, y));
    }
}

fn compute_panel_height(qview: &QueueViewState<'_>) -> u16 {
    // 1 header + N queue rows + 1 input line + 1 hints line.
    let n = qview.queue.len().min(qview.max_queue_visible);
    1 + (n as u16) + 1 + 1
}

fn input_prompt_prefix(qview: &QueueViewState<'_>) -> &'static str {
    if qview.passthrough_to_child {
        "raw input> "
    } else if qview.editing_index.is_some() {
        "edit> "
    } else if qview.queue.paused && qview.force_queue {
        "force-queue (paused)> "
    } else if qview.queue.paused {
        "queue (paused)> "
    } else if qview.force_queue {
        "force-queue> "
    } else {
        "queue> "
    }
}

fn render_panel(f: &mut Frame<'_>, area: Rect, qview: &QueueViewState<'_>) {
    // Layout: header line (separator + status), queue list, input line, hints.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    let header = build_header(qview, chunks[0].width);
    f.render_widget(Paragraph::new(header), chunks[0]);

    let list_lines: Vec<Line> = qview
        .queue
        .items()
        .iter()
        .take(qview.max_queue_visible)
        .enumerate()
        .map(|(i, it)| {
            let prefix = if qview.editing_index == Some(i) {
                " ✎ "
            } else if i == 0 {
                " ▸ "
            } else {
                "   "
            };
            let cond = if it.conditional { "↪ " } else { "  " };
            let prefix_part = format!("{prefix}{cond}");
            let style = if i == 0 {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else if it.conditional {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(vec![
                Span::styled(prefix_part, style),
                Span::styled(it.command.as_str(), style),
            ])
        })
        .collect();

    let list = Paragraph::new(list_lines).wrap(Wrap { trim: false });
    f.render_widget(list, chunks[1]);

    let prompt = input_prompt_prefix(qview);
    let input_line = Line::from(vec![
        Span::styled(
            prompt,
            Style::default()
                .fg(if qview.passthrough_to_child {
                    Color::Red
                } else {
                    Color::Green
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(qview.input_buffer),
    ]);
    f.render_widget(Paragraph::new(input_line), chunks[2]);

    let hints = build_hint_line(qview);
    f.render_widget(Paragraph::new(hints), chunks[3]);
}

fn chip(key: &str, label: &str) -> Vec<Span<'static>> {
    let bracket = Style::default().fg(Color::DarkGray);
    let key_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::Gray);
    vec![
        Span::styled("[", bracket),
        Span::styled(key.to_string(), key_style),
        Span::styled(format!(" {label}"), label_style),
        Span::styled("]", bracket),
    ]
}

fn build_hint_line(qview: &QueueViewState<'_>) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let sep = || Span::styled("  ·  ", dim);
    let gap = || Span::raw(" ");

    let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];

    if qview.pending_quit {
        spans.extend(chip("^D", "again to quit"));
        spans.push(sep());
        spans.push(Span::styled(
            "any other key keeps working",
            Style::default().fg(Color::Gray),
        ));
        return Line::from(spans);
    }

    if qview.editing_index.is_some() {
        spans.extend(chip("Esc", "cancel"));
        spans.push(gap());
        spans.extend(chip("⏎", "save"));
        spans.push(gap());
        spans.extend(chip("^D", "delete"));
        spans.push(gap());
        spans.extend(chip("Alt-↑↓", "reorder"));
        spans.push(gap());
        spans.extend(chip("⇥", "chain"));
        return Line::from(spans);
    }

    let pause_label = if qview.queue.paused {
        "resume"
    } else {
        "pause"
    };

    spans.extend(chip("⏎", "add"));
    spans.push(gap());
    spans.extend(chip("↑", "edit"));
    spans.push(gap());
    spans.extend(chip("⇥", "chain"));
    spans.push(sep());
    spans.extend(chip("^X", pause_label));
    spans.push(gap());
    spans.extend(chip("^K", "clear"));
    spans.push(gap());
    spans.extend(chip("^\\", "raw"));
    spans.push(sep());
    spans.extend(chip("?", "help"));

    Line::from(spans)
}

fn build_header(qview: &QueueViewState<'_>, width: u16) -> Line<'static> {
    let dash_count = width as usize;
    let header = if qview.status.is_empty() {
        " cmdq ".to_string()
    } else {
        format!(" cmdq │ {} ", qview.status)
    };
    let header_len = header.chars().count();
    let line_text: String = if header_len < dash_count {
        let pad = dash_count - header_len;
        let mut s = header;
        s.extend(std::iter::repeat_n('─', pad));
        s
    } else {
        "─".repeat(dash_count)
    };
    Line::from(Span::styled(
        line_text,
        Style::default().fg(Color::DarkGray),
    ))
}

/// Render a centered help overlay listing all key bindings.
pub fn render_help(f: &mut Frame<'_>) {
    let area = f.area();
    let width = area.width.saturating_sub(8).clamp(40, 72);
    let height = area.height.saturating_sub(2).clamp(20, 32);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" cmdq · keyboard shortcuts ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::LightYellow))
        .title_style(
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        );

    let key_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Gray);

    let row = |k: &'static str, d: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {:<14}", k), key_style),
            Span::styled(d, dim),
        ])
    };
    let section = |title: &'static str| -> Line<'static> {
        Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let note_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::ITALIC);

    let lines: Vec<Line> = vec![
        Line::from(Span::styled(
            "  panel appears 1.5s into a long command. ↑ recalls",
            note_style,
        )),
        Line::from(Span::styled(
            "  the QUEUE — shell history isn't reachable from here.",
            note_style,
        )),
        Line::from(""),
        section("add to queue"),
        row("Enter", "add the typed command to the queue"),
        row("Tab", "chain — only run if previous succeeded"),
        row("Esc", "clear the input buffer"),
        Line::from(""),
        section("edit a queued item"),
        row("↑ / ↓", "open previous / next queued item for edit"),
        row("Enter", "save the edit"),
        row("Esc", "cancel the edit (item unchanged)"),
        row("Ctrl-D", "delete the item being edited"),
        row("Alt-↑ / Alt-↓", "reorder the item being edited"),
        Line::from(""),
        section("queue control"),
        row("Ctrl-X", "pause / resume auto-dispatch"),
        row("Ctrl-K", "clear the entire queue"),
        Line::from(""),
        section("modes"),
        row("Ctrl-Q", "force the panel open even at the shell prompt"),
        row("Ctrl-\\", "raw input — keys go straight to the running app"),
        row("Esc Esc", "raw input (SSH-safe alternative to Ctrl-\\)"),
        Line::from(""),
        section("misc"),
        row("Ctrl-C", "send SIGINT to running command (pauses queue)"),
        row("Ctrl-D", "quit cmdq (twice if queue is non-empty)"),
        row("F1 / ?", "show this help · Esc / Enter dismisses"),
    ];

    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
}

/// Widget that paints a `vt100::Parser` screen state into a ratatui Buffer.
struct VtScreen<'a> {
    parser: &'a vt100::Parser,
}

impl<'a> Widget for VtScreen<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let max_rows = rows.min(area.height);
        let max_cols = cols.min(area.width);
        for row in 0..max_rows {
            for col in 0..max_cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => continue,
                };
                let mut style = Style::default();
                if let Some(color) = vt_color_to_ratatui(cell.fgcolor()) {
                    style = style.fg(color);
                }
                if let Some(color) = vt_color_to_ratatui(cell.bgcolor()) {
                    style = style.bg(color);
                }
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let contents = cell.contents();
                let target_x = area.x + col;
                let target_y = area.y + row;
                if let Some(buf_cell) = buf.cell_mut((target_x, target_y)) {
                    if contents.is_empty() {
                        buf_cell.set_symbol(" ");
                    } else {
                        buf_cell.set_symbol(contents);
                    }
                    buf_cell.set_style(style);
                }
            }
        }
    }
}

fn vt_color_to_ratatui(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(idx) => Some(Color::Indexed(idx)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}
