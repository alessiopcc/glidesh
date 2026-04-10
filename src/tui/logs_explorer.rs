use crate::logging::storage::{self, NodeSummary, RunSummaryFile};
use crate::tui::widgets::wrap_line;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Row, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Table,
};
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const COLOR_ACCENT: Color = Color::Rgb(99, 190, 101);
const COLOR_BORDER: Color = Color::Rgb(80, 80, 80);

#[derive(Debug, Clone, PartialEq)]
enum LogsView {
    RunList,
    RunDetail,
    NodeLog,
}

struct RunEntry {
    path: PathBuf,
    name: String,
    summary: Option<RunSummaryFile>,
}

struct LogsExplorerState {
    view: LogsView,
    runs: Vec<RunEntry>,
    selected_run: usize,
    run_scroll: usize,
    selected_runs: HashSet<usize>,
    confirm_delete: bool,
    current_summary: Option<RunSummaryFile>,
    node_names: Vec<String>,
    selected_node: usize,
    node_scroll: usize,
    log_lines: Vec<String>,
    log_scroll: usize,
    flash: Option<(String, std::time::Instant)>,
}

impl LogsExplorerState {
    fn new(run_dirs: Vec<PathBuf>) -> Self {
        let runs: Vec<RunEntry> = run_dirs
            .into_iter()
            .map(|path| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let summary = storage::read_summary(&path).ok();
                RunEntry {
                    path,
                    name,
                    summary,
                }
            })
            .collect();

        LogsExplorerState {
            view: LogsView::RunList,
            runs,
            selected_run: 0,
            run_scroll: 0,
            selected_runs: HashSet::new(),
            confirm_delete: false,
            current_summary: None,
            node_names: Vec::new(),
            selected_node: 0,
            node_scroll: 0,
            log_lines: Vec::new(),
            log_scroll: 0,
            flash: None,
        }
    }

    fn enter_run_detail(&mut self) {
        if self.selected_run >= self.runs.len() {
            return;
        }
        let run = &self.runs[self.selected_run];
        self.current_summary = run.summary.clone();
        self.node_names = if let Some(ref summary) = self.current_summary {
            let mut names: Vec<String> = summary.nodes.keys().cloned().collect();
            names.sort();
            names
        } else {
            Vec::new()
        };
        self.selected_node = 0;
        self.view = LogsView::RunDetail;
    }

    fn enter_node_log(&mut self) {
        if self.selected_node >= self.node_names.len() {
            return;
        }
        let node = &self.node_names[self.selected_node];
        let run = &self.runs[self.selected_run];
        self.log_lines = match storage::read_node_log(&run.path, node) {
            Ok(content) => content.lines().map(|l| l.to_string()).collect(),
            Err(_) => vec!["(no log file found)".to_string()],
        };
        self.log_scroll = 0;
        self.view = LogsView::NodeLog;
    }

    fn go_back(&mut self) {
        match self.view {
            LogsView::RunList => {}
            LogsView::RunDetail => {
                self.current_summary = None;
                self.node_names.clear();
                self.view = LogsView::RunList;
            }
            LogsView::NodeLog => {
                self.log_lines.clear();
                self.log_scroll = 0;
                self.view = LogsView::RunDetail;
            }
        }
    }

    fn move_up(&mut self) {
        match self.view {
            LogsView::RunList => {
                if self.selected_run > 0 {
                    self.selected_run -= 1;
                }
            }
            LogsView::RunDetail => {
                if self.selected_node > 0 {
                    self.selected_node -= 1;
                }
            }
            LogsView::NodeLog => {
                if self.log_scroll > 0 {
                    self.log_scroll -= 1;
                }
            }
        }
    }

    fn move_down(&mut self) {
        match self.view {
            LogsView::RunList => {
                if !self.runs.is_empty() && self.selected_run < self.runs.len() - 1 {
                    self.selected_run += 1;
                }
            }
            LogsView::RunDetail => {
                if !self.node_names.is_empty() && self.selected_node < self.node_names.len() - 1 {
                    self.selected_node += 1;
                }
            }
            LogsView::NodeLog => {
                self.log_scroll += 1;
            }
        }
    }

    fn page_up(&mut self, page_size: usize) {
        if self.view == LogsView::NodeLog {
            self.log_scroll = self.log_scroll.saturating_sub(page_size);
        }
    }

    fn page_down(&mut self, page_size: usize) {
        if self.view == LogsView::NodeLog {
            self.log_scroll += page_size;
        }
    }

    fn toggle_selection(&mut self) {
        if self.view != LogsView::RunList || self.runs.is_empty() {
            return;
        }
        if self.selected_runs.contains(&self.selected_run) {
            self.selected_runs.remove(&self.selected_run);
        } else {
            self.selected_runs.insert(self.selected_run);
        }
        if self.selected_run < self.runs.len() - 1 {
            self.selected_run += 1;
        }
    }

    fn runs_to_delete(&self) -> Vec<usize> {
        if self.selected_runs.is_empty() {
            if self.selected_run < self.runs.len() {
                vec![self.selected_run]
            } else {
                vec![]
            }
        } else {
            let mut indices: Vec<usize> = self.selected_runs.iter().copied().collect();
            indices.sort();
            indices
        }
    }

    fn delete_selected(&mut self) {
        let mut indices = self.runs_to_delete();
        indices.sort();
        indices.reverse();
        for idx in indices {
            if idx < self.runs.len() {
                let _ = storage::delete_run(&self.runs[idx].path);
                self.runs.remove(idx);
            }
        }
        self.selected_runs.clear();
        self.confirm_delete = false;
        if !self.runs.is_empty() {
            self.selected_run = self.selected_run.min(self.runs.len() - 1);
        } else {
            self.selected_run = 0;
        }
    }

    fn current_log_path(&self) -> Option<PathBuf> {
        if self.view != LogsView::NodeLog {
            return None;
        }
        let node = self.node_names.get(self.selected_node)?;
        let run = self.runs.get(self.selected_run)?;
        Some(run.path.join(format!("{}.log", node)))
    }

    fn set_flash(&mut self, msg: &str) {
        self.flash = Some((msg.to_string(), std::time::Instant::now()));
    }

    fn copy_to_clipboard(&mut self) {
        let content = self.log_lines.join("\n");
        let result = copy_text_to_clipboard(&content);
        match result {
            Ok(()) => self.set_flash("Copied to clipboard"),
            Err(e) => self.set_flash(&format!("Copy failed: {}", e)),
        }
    }
}

fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("clip.exe");
        c.stdin(std::process::Stdio::piped());
        c
    };

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("pbcopy");
        c.stdin(std::process::Stdio::piped());
        c
    };

    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = Command::new("xclip");
        c.arg("-selection").arg("clipboard");
        c.stdin(std::process::Stdio::piped());
        c
    };

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let Some(ref mut stdin) = child.stdin {
        use std::io::Write;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
    }
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    Command::new(&editor)
        .arg(path)
        .status()
        .map_err(|e| format!("{}: {}", editor, e))?;
    Ok(())
}

fn render(frame: &mut Frame, state: &mut LogsExplorerState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());

    match state.view {
        LogsView::RunList => render_run_list(frame, chunks[0], state),
        LogsView::RunDetail => render_run_detail(frame, chunks[0], state),
        LogsView::NodeLog => render_node_log(frame, chunks[0], state),
    }

    render_footer(frame, chunks[1], state);

    if state.confirm_delete {
        render_confirm_dialog(frame, state);
    }
}

fn render_confirm_dialog(frame: &mut Frame, state: &LogsExplorerState) {
    let count = state.runs_to_delete().len();
    let msg = format!(
        " Delete {} run{}? This cannot be undone. (y/n) ",
        count,
        if count == 1 { "" } else { "s" }
    );
    let width = (msg.len() as u16 + 4).min(frame.area().width);
    let height = 3;
    let area = centered_rect(width, height, frame.area());

    frame.render_widget(Clear, area);
    let dialog = Paragraph::new(Line::from(vec![Span::styled(
        msg,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Red))
            .title(" Confirm Delete "),
    );
    frame.render_widget(dialog, area);
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn ensure_visible(scroll: &mut usize, selected: usize, visible: usize) {
    if visible == 0 {
        return;
    }
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= *scroll + visible {
        *scroll = selected - visible + 1;
    }
}

fn render_run_list(frame: &mut Frame, area: Rect, state: &mut LogsExplorerState) {
    let selection_count = state.selected_runs.len();
    let visible_height = area.height.saturating_sub(2) as usize;
    ensure_visible(&mut state.run_scroll, state.selected_run, visible_height);
    let offset = state.run_scroll;
    let end = (offset + visible_height).min(state.runs.len());

    let items: Vec<ListItem> = state.runs[offset..end]
        .iter()
        .enumerate()
        .map(|(vi, run)| {
            let i = offset + vi;
            let snippet = if let Some(ref summary) = run.summary {
                let node_count = summary.nodes.len();
                let ok = summary.nodes.values().filter(|n| n.status == "ok").count();
                let failed = summary
                    .nodes
                    .values()
                    .filter(|n| n.status == "failed")
                    .count();
                format!("  {} nodes: {} ok, {} failed", node_count, ok, failed)
            } else {
                "  (no summary)".to_string()
            };

            let is_selected = state.selected_runs.contains(&i);
            let marker = if is_selected { "[x] " } else { "[ ] " };

            let style = if i == state.selected_run {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };

            let marker_style = if is_selected {
                style.fg(COLOR_ACCENT)
            } else {
                style.fg(Color::DarkGray)
            };

            ListItem::new(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(&run.name, style),
                Span::styled(snippet, style.fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let title = if selection_count > 0 {
        format!(
            " Runs (newest first) \u{2014} {} selected ",
            selection_count
        )
    } else {
        " Runs (newest first) ".to_string()
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_BORDER))
        .title(title);

    let list = List::new(items).block(block);
    frame.render_widget(list, area);

    if state.runs.len() > visible_height {
        let scrollbar_area = Rect {
            x: area.x + area.width - 1,
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };
        let mut scrollbar_state =
            ScrollbarState::new(state.runs.len().saturating_sub(visible_height)).position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .end_symbol(Some("\u{25bc}"))
            .track_symbol(Some("\u{2591}"))
            .thumb_symbol("\u{2588}");
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_run_detail(frame: &mut Frame, area: Rect, state: &mut LogsExplorerState) {
    let run = &state.runs[state.selected_run];

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .split(area);

    let header_text = if let Some(ref summary) = state.current_summary {
        let finished = summary
            .finished_at
            .map(|f| f.to_string())
            .unwrap_or_else(|| "(running)".to_string());
        format!(
            "Plan: {}\nRun ID: {}\nStarted: {}\nFinished: {}",
            summary.plan, summary.run_id, summary.started_at, finished
        )
    } else {
        format!("Run: {}\n(no summary available)", run.name)
    };

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(format!(" {} ", run.name)),
    );
    frame.render_widget(header, chunks[0]);

    let table_header = Row::new(vec!["", "NODE", "STATUS", "CHANGED", "ERROR"])
        .style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .bottom_margin(0);

    let visible_nodes = chunks[1].height.saturating_sub(3) as usize;
    ensure_visible(&mut state.node_scroll, state.selected_node, visible_nodes);
    let node_offset = state.node_scroll;
    let node_end = (node_offset + visible_nodes).min(state.node_names.len());

    let rows: Vec<Row> = state.node_names[node_offset..node_end]
        .iter()
        .enumerate()
        .map(|(vi, name)| {
            let i = node_offset + vi;
            let node_summary: Option<&NodeSummary> = state
                .current_summary
                .as_ref()
                .and_then(|s| s.nodes.get(name));

            let (status, changed, error) = if let Some(ns) = node_summary {
                (
                    ns.status.clone(),
                    ns.changed.to_string(),
                    ns.error.clone().unwrap_or_default(),
                )
            } else {
                ("?".to_string(), "?".to_string(), String::new())
            };

            let icon = match status.as_str() {
                "ok" => "\u{2713}",
                "failed" => "\u{2717}",
                _ => "\u{00b7}",
            };
            let icon_color = match status.as_str() {
                "ok" => Color::Green,
                "failed" => Color::Red,
                _ => Color::DarkGray,
            };

            let is_selected = i == state.selected_node;

            if is_selected {
                Row::new(vec![icon.to_string(), name.clone(), status, changed, error])
                    .style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                let status_color = match status.as_str() {
                    "ok" => Color::Green,
                    "failed" => Color::Red,
                    _ => Color::default(),
                };
                Row::new(vec![
                    Line::from(Span::styled(
                        icon.to_string(),
                        Style::default().fg(icon_color),
                    )),
                    Line::from(Span::styled(
                        name.clone(),
                        Style::default().fg(Color::White),
                    )),
                    Line::from(Span::styled(
                        status.clone(),
                        Style::default().fg(status_color),
                    )),
                    Line::from(Span::styled(changed, Style::default().fg(Color::DarkGray))),
                    Line::from(Span::styled(error, Style::default().fg(Color::Red))),
                ])
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Percentage(25),
            Constraint::Percentage(15),
            Constraint::Percentage(10),
            Constraint::Percentage(45),
        ],
    )
    .header(table_header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(COLOR_BORDER))
            .title(" Nodes "),
    );

    frame.render_widget(table, chunks[1]);

    if state.node_names.len() > visible_nodes {
        let scrollbar_area = Rect {
            x: chunks[1].x + chunks[1].width - 1,
            y: chunks[1].y + 1,
            width: 1,
            height: chunks[1].height.saturating_sub(2),
        };
        let mut scrollbar_state =
            ScrollbarState::new(state.node_names.len().saturating_sub(visible_nodes))
                .position(node_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .end_symbol(Some("\u{25bc}"))
            .track_symbol(Some("\u{2591}"))
            .thumb_symbol("\u{2588}");
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn log_line_style(line: &str) -> Style {
    let trimmed = line.trim();
    if trimmed.contains("FAILED") {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else if trimmed.contains("[STEP]") || (trimmed.starts_with("──") && trimmed.ends_with("──"))
    {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.contains("changed") {
        Style::default().fg(Color::Yellow)
    } else if trimmed.contains("[RESULT]")
        || trimmed.contains("[COMPLETE]")
        || trimmed.contains(": ok")
    {
        Style::default().fg(Color::Green)
    } else if trimmed.starts_with("CHECK ") || trimmed.starts_with("> CHECK") {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    }
}

fn render_node_log(frame: &mut Frame, area: Rect, state: &mut LogsExplorerState) {
    let node_name = &state.node_names[state.selected_node];
    let inner_width = area.width.saturating_sub(3) as usize;
    let visible_height = area.height.saturating_sub(2) as usize;

    let mut wrapped: Vec<(&str, Style)> = Vec::new();
    for line in &state.log_lines {
        let style = log_line_style(line);
        for sub in wrap_line(line, inner_width) {
            wrapped.push((sub, style));
        }
    }

    let total_lines = wrapped.len();

    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = state.log_scroll.min(max_scroll);

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

    let title = format!(" Log: {} ", node_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR_ACCENT))
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
            .begin_symbol(Some("\u{25b2}"))
            .end_symbol(Some("\u{25bc}"))
            .track_symbol(Some("\u{2591}"))
            .thumb_symbol("\u{2588}");
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_footer(frame: &mut Frame, area: Rect, state: &LogsExplorerState) {
    if let Some((ref msg, at)) = state.flash {
        if at.elapsed() < std::time::Duration::from_secs(2) {
            let paragraph =
                Paragraph::new(format!(" {} ", msg)).style(Style::default().fg(COLOR_ACCENT));
            frame.render_widget(paragraph, area);
            return;
        }
    }

    let text = match state.view {
        LogsView::RunList => {
            " Enter view  Space select  d delete  q quit  \u{2191}\u{2193} navigate"
        }
        LogsView::RunDetail => " Enter view log  Esc back  q quit  \u{2191}\u{2193} navigate",
        LogsView::NodeLog => {
            " Esc back  c copy  e editor  \u{2191}\u{2193}/j/k scroll  PgUp/PgDn page  q quit"
        }
    };

    let paragraph = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(paragraph, area);
}

pub fn run_logs_tui(run_dirs: Vec<PathBuf>) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut state = LogsExplorerState::new(run_dirs);

    loop {
        let visible_height = terminal.size()?.height.saturating_sub(3) as usize;

        terminal.draw(|f| render(f, &mut state))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if state.confirm_delete {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            state.delete_selected();
                        }
                        _ => {
                            state.confirm_delete = false;
                        }
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Enter => match state.view {
                        LogsView::RunList => state.enter_run_detail(),
                        LogsView::RunDetail => state.enter_node_log(),
                        LogsView::NodeLog => {}
                    },
                    KeyCode::Char(' ') => state.toggle_selection(),
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if state.view == LogsView::RunList && !state.runs.is_empty() {
                            state.confirm_delete = true;
                        }
                    }
                    KeyCode::Char('c') if state.view == LogsView::NodeLog => {
                        state.copy_to_clipboard();
                    }
                    KeyCode::Char('e') if state.view == LogsView::NodeLog => {
                        if let Some(path) = state.current_log_path() {
                            terminal::disable_raw_mode()?;
                            io::stdout().execute(LeaveAlternateScreen)?;
                            if let Err(e) = open_in_editor(&path) {
                                state.set_flash(&format!("Editor failed: {}", e));
                            }
                            terminal::enable_raw_mode()?;
                            io::stdout().execute(EnterAlternateScreen)?;
                            terminal.clear()?;
                        }
                    }
                    KeyCode::Esc | KeyCode::Backspace => state.go_back(),
                    KeyCode::Up | KeyCode::Char('k') => state.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                    KeyCode::PageUp => state.page_up(visible_height),
                    KeyCode::PageDown => state.page_down(visible_height),
                    KeyCode::Home | KeyCode::Char('g') => {
                        if state.view == LogsView::NodeLog {
                            state.log_scroll = 0;
                        }
                    }
                    KeyCode::End | KeyCode::Char('G') => {
                        if state.view == LogsView::NodeLog {
                            state.log_scroll = usize::MAX;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logs_explorer_state_empty() {
        let state = LogsExplorerState::new(Vec::new());
        assert_eq!(state.view, LogsView::RunList);
        assert!(state.runs.is_empty());
    }

    #[test]
    fn test_logs_explorer_navigation() {
        let state = LogsExplorerState::new(Vec::new());
        // Can't drill into empty list
        assert_eq!(state.view, LogsView::RunList);
    }
}
