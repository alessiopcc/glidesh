use crate::tui::state::{FocusPanel, NodeStatus, TuiState};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, LineGauge, List, ListItem, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table,
};
use std::time::Duration;

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const COLOR_BLUE: Color = Color::Rgb(100, 149, 237);
const COLOR_GREEN: Color = Color::Rgb(99, 190, 101);
const COLOR_RED: Color = Color::Rgb(220, 80, 80);
const COLOR_GAUGE_UNFILLED: Color = Color::Rgb(60, 60, 60);
const COLOR_BORDER_INACTIVE: Color = Color::Rgb(80, 80, 80);

/// Returns the frame accent color based on run state:
/// blue while running, green on success, red on failure.
fn frame_color(state: &TuiState) -> Color {
    if !state.run_complete {
        COLOR_BLUE
    } else if state.nodes.iter().any(|n| n.status == NodeStatus::Failed) {
        COLOR_RED
    } else {
        COLOR_GREEN
    }
}

pub fn render(frame: &mut Frame, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),      // header
            Constraint::Percentage(50), // node table
            Constraint::Percentage(50), // log panel
            Constraint::Length(1),      // footer
        ])
        .split(frame.area());

    render_header(frame, chunks[0], state);
    render_node_table(frame, chunks[1], state);
    render_log_panel(frame, chunks[2], state);
    render_footer(frame, chunks[3], state);

    if state.confirm_quit {
        render_confirm_quit(frame);
    }
}

fn render_header(frame: &mut Frame, area: Rect, state: &TuiState) {
    let progress = if state.total > 0 {
        state.completed as f64 / state.total as f64
    } else {
        0.0
    };

    let elapsed = elapsed_str(state.elapsed());
    let failed_count = state
        .nodes
        .iter()
        .filter(|n| n.status == NodeStatus::Failed)
        .count();

    let label = if state.run_complete {
        if failed_count > 0 {
            format!(
                " glidesh \u{2500}\u{2500} {}/{} hosts \u{2500}\u{2500} {} failed \u{2500}\u{2500} {} changed \u{2500}\u{2500} {} ",
                state.completed, state.total, failed_count, state.total_changed, elapsed
            )
        } else {
            format!(
                " glidesh \u{2500}\u{2500} {}/{} hosts \u{2500}\u{2500} {} changed \u{2500}\u{2500} {} \u{2500}\u{2500} all ok ",
                state.completed, state.total, state.total_changed, elapsed
            )
        }
    } else {
        format!(
            " glidesh \u{2500}\u{2500} {}/{} hosts \u{2500}\u{2500} {} changed \u{2500}\u{2500} {} ",
            state.completed, state.total, state.total_changed, elapsed
        )
    };

    let accent = frame_color(state);

    let gauge = LineGauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(accent)),
        )
        .filled_style(Style::default().fg(accent))
        .unfilled_style(Style::default().fg(COLOR_GAUGE_UNFILLED))
        .ratio(progress)
        .label(label);

    frame.render_widget(gauge, area);
}

fn render_node_table(frame: &mut Frame, area: Rect, state: &TuiState) {
    let is_focused = state.focus == FocusPanel::Nodes;
    let border_color = if is_focused {
        frame_color(state)
    } else {
        COLOR_BORDER_INACTIVE
    };

    let header = Row::new(vec![
        "", "HOST", "GROUP", "PLAN", "STATUS", "STEP", "CHG", "TIME",
    ])
    .style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .bottom_margin(0);

    let styled_rows: Vec<Row> = state
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let icon = status_icon(&node.status, state.spinner_tick);
            let icon_color = match node.status {
                NodeStatus::Connecting | NodeStatus::Running => Color::Cyan,
                NodeStatus::Done => Color::Green,
                NodeStatus::Failed => Color::Red,
            };

            let step_display =
                if node.status == NodeStatus::Done || node.status == NodeStatus::Failed {
                    "--".to_string()
                } else if node.total_steps > 0 {
                    format!(
                        "[{}/{}] {}",
                        node.step_index + 1,
                        node.total_steps,
                        node.current_step
                    )
                } else {
                    node.current_step.clone()
                };

            let elapsed = elapsed_str(match node.finished_at {
                Some(t) => t.duration_since(node.started_at),
                None => node.started_at.elapsed(),
            });
            let chg_str = format!("{}", node.changed);

            let is_selected = i == state.selected_node;

            if is_selected {
                let sel_style = if is_focused {
                    Style::default()
                        .add_modifier(Modifier::REVERSED)
                        .fg(Color::White)
                } else {
                    Style::default().add_modifier(Modifier::REVERSED)
                };
                Row::new(vec![
                    icon.to_string(),
                    node.host.clone(),
                    node.group_name.clone(),
                    node.plan_name.clone(),
                    node.status.to_string(),
                    step_display,
                    chg_str,
                    elapsed,
                ])
                .style(sel_style)
            } else {
                Row::new(vec![
                    Line::from(Span::styled(
                        icon.to_string(),
                        Style::default().fg(icon_color),
                    )),
                    Line::from(Span::styled(
                        node.host.clone(),
                        Style::default().fg(Color::White),
                    )),
                    Line::from(Span::styled(
                        node.group_name.clone(),
                        Style::default().fg(Color::Cyan),
                    )),
                    Line::from(Span::styled(
                        node.plan_name.clone(),
                        Style::default().fg(Color::Magenta),
                    )),
                    Line::from(Span::styled(
                        node.status.to_string(),
                        Style::default().fg(status_color(&node.status)),
                    )),
                    Line::from(Span::styled(
                        step_display,
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(Span::styled(
                        chg_str,
                        if node.changed > 0 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    )),
                    Line::from(Span::styled(elapsed, Style::default().fg(Color::DarkGray))),
                ])
            }
        })
        .collect();

    let table = Table::new(
        styled_rows,
        [
            Constraint::Length(2),      // icon
            Constraint::Percentage(16), // host
            Constraint::Percentage(10), // group
            Constraint::Percentage(12), // plan
            Constraint::Length(12),     // status
            Constraint::Percentage(24), // step
            Constraint::Length(5),      // chg
            Constraint::Length(8),      // time
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(" Nodes "),
    );

    frame.render_widget(table, area);
}

/// Split a line into chunks of at most `width` characters on char boundaries.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<&str> {
    if width == 0 {
        return vec![line];
    }
    let mut result = Vec::new();
    let mut remaining = line;
    while !remaining.is_empty() {
        if remaining.len() <= width {
            result.push(remaining);
            break;
        }
        let mut end = width;
        while !remaining.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        if end == 0 {
            end = remaining
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());
        }
        result.push(&remaining[..end]);
        remaining = &remaining[end..];
    }
    if result.is_empty() {
        result.push("");
    }
    result
}

fn line_style(line: &str) -> Style {
    let trimmed = line.trim();
    if trimmed.starts_with("──") && trimmed.ends_with("──") {
        // Step header
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.contains("FAILED") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("CHECK ") {
        Style::default().fg(Color::DarkGray)
    } else if trimmed.contains(": changed") {
        Style::default().fg(Color::Yellow)
    } else if trimmed.contains(": ok") || trimmed.starts_with("Connected") {
        Style::default().fg(Color::Green)
    } else if trimmed.starts_with("Connecting") {
        Style::default().fg(Color::DarkGray)
    } else if trimmed.starts_with("> ") || trimmed.starts_with("  > ") {
        Style::default().fg(Color::Rgb(120, 120, 120))
    } else {
        Style::default().fg(Color::White)
    }
}

fn render_log_panel(frame: &mut Frame, area: Rect, state: &TuiState) {
    let logs = state.active_logs();
    let scroll_pos = state.active_scroll();

    let is_focused = state.focus == FocusPanel::Logs;
    let border_color = if is_focused {
        frame_color(state)
    } else {
        COLOR_BORDER_INACTIVE
    };

    let inner_width = area.width.saturating_sub(3) as usize;
    let visible_height = area.height.saturating_sub(2) as usize;

    let mut wrapped: Vec<(&str, Style)> = Vec::new();
    for line in logs {
        let style = line_style(line);
        for sub in wrap_line(line, inner_width) {
            wrapped.push((sub, style));
        }
    }

    let total_lines = wrapped.len();

    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = scroll_pos.min(max_scroll);

    let end = (scroll + visible_height).min(total_lines);
    let items: Vec<ListItem> = wrapped[scroll..end]
        .iter()
        .map(|(text, style)| ListItem::new(Line::from(Span::styled((*text).to_owned(), *style))))
        .collect();

    let scroll_info = if total_lines > visible_height {
        format!(" {}/{} ", scroll + 1, total_lines)
    } else {
        String::new()
    };

    let title = state.log_title();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title)
        .title_bottom(Line::from(scroll_info).right_aligned());

    let list = List::new(items).block(block);

    frame.render_widget(list, area);

    if total_lines > visible_height {
        let scrollbar_area = Rect {
            x: area.x + area.width - 1,
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };
        let mut scrollbar_state =
            ScrollbarState::new(total_lines.saturating_sub(visible_height)).position(scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("▲"))
            .end_symbol(Some("▼"))
            .track_symbol(Some("░"))
            .thumb_symbol("█");
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_footer(frame: &mut Frame, area: Rect, state: &TuiState) {
    let text = if state.viewing_node.is_some() {
        match state.focus {
            FocusPanel::Nodes => {
                " \u{2191}\u{2193} select node  Esc/Tab back  j/k scroll log  q quit "
            }
            FocusPanel::Logs => {
                " \u{2191}\u{2193}/j/k scroll  PgUp/PgDn page  g/G top/bottom  Esc/Tab back  q quit "
            }
        }
    } else {
        match state.focus {
            FocusPanel::Nodes => {
                " \u{2191}\u{2193} select node  Enter view node  Tab switch panel  j/k scroll log  q quit "
            }
            FocusPanel::Logs => {
                " \u{2191}\u{2193}/j/k scroll  PgUp/PgDn page  g/G top/bottom  Tab switch panel  q quit "
            }
        }
    };

    let paragraph = Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )]));
    frame.render_widget(paragraph, area);
}

fn status_icon(status: &NodeStatus, tick: usize) -> char {
    match status {
        NodeStatus::Connecting | NodeStatus::Running => {
            // Animate every 4 ticks (~64ms per frame at 16ms poll)
            SPINNER_FRAMES[(tick / 4) % SPINNER_FRAMES.len()]
        }
        NodeStatus::Done => '\u{2713}',   // ✓
        NodeStatus::Failed => '\u{2717}', // ✗
    }
}

fn status_color(status: &NodeStatus) -> Color {
    match status {
        NodeStatus::Connecting => Color::Yellow,
        NodeStatus::Running => Color::Cyan,
        NodeStatus::Done => Color::Green,
        NodeStatus::Failed => Color::Red,
    }
}

fn render_confirm_quit(frame: &mut Frame) {
    let area = frame.area();
    let width = 40u16.min(area.width.saturating_sub(4));
    let height = 5u16;
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "A plan is still running. Quit? (y/n)",
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Confirm Quit ");

    let paragraph = Paragraph::new(text).block(block).centered();
    frame.render_widget(paragraph, popup);
}

fn elapsed_str(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m {}s", secs / 60, secs % 60)
    }
}
