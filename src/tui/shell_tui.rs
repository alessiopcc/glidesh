use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use glidesh::config::types::ResolvedHost;
use glidesh::error::GlideshError;
use glidesh::ssh::{HostKeyPolicy, SshSession};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

const COLOR_ACCENT: Color = Color::Rgb(100, 149, 237);
const COLOR_BORDER_INACTIVE: Color = Color::Rgb(80, 80, 80);

struct ShellTuiState {
    input: String,
    cursor_pos: usize,
    output_lines: Vec<(String, OutputKind)>,
    scroll: usize,
    auto_scroll: bool,
    running: bool,
    host_count: usize,
}

#[derive(Clone)]
enum OutputKind {
    Stdout,
    Stderr,
    System,
}

/// Messages sent from command execution tasks to the TUI.
enum ShellMsg {
    Line(String, OutputKind),
    Done,
}

impl ShellTuiState {
    fn new(host_count: usize) -> Self {
        Self {
            input: String::new(),
            cursor_pos: 0,
            output_lines: vec![(
                format!(
                    "Connected to {} host(s). Type a command and press Enter. Ctrl+D to exit.",
                    host_count
                ),
                OutputKind::System,
            )],
            scroll: usize::MAX,
            auto_scroll: true,
            running: false,
            host_count,
        }
    }
}

/// Run the interactive shell TUI for multiple hosts.
/// Shows an input bar at the bottom, streams command output above.
pub async fn run_shell_tui(
    hosts: &[ResolvedHost],
    key: &PrivateKeyWithHashAlg,
    host_key_policy: HostKeyPolicy,
    concurrency: usize,
) -> Result<(), GlideshError> {
    terminal::enable_raw_mode().map_err(|e| GlideshError::Other(e.to_string()))?;
    io::stdout()
        .execute(EnterAlternateScreen)
        .map_err(|e| GlideshError::Other(e.to_string()))?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).map_err(|e| GlideshError::Other(e.to_string()))?;

    let state = Arc::new(std::sync::Mutex::new(ShellTuiState::new(hosts.len())));

    let result = shell_tui_loop(
        &mut terminal,
        &state,
        hosts,
        key,
        host_key_policy,
        concurrency,
    )
    .await;

    let _ = terminal::disable_raw_mode();
    let _ = io::stdout().execute(LeaveAlternateScreen);

    result
}

async fn shell_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &Arc<std::sync::Mutex<ShellTuiState>>,
    hosts: &[ResolvedHost],
    key: &PrivateKeyWithHashAlg,
    host_key_policy: HostKeyPolicy,
    concurrency: usize,
) -> Result<(), GlideshError> {
    // Channel for receiving output lines from command execution
    let (output_tx, mut output_rx) = mpsc::unbounded_channel::<ShellMsg>();

    loop {
        // Drain any pending output
        while let Ok(msg) = output_rx.try_recv() {
            let mut s = state.lock().unwrap();
            match msg {
                ShellMsg::Line(text, kind) => s.output_lines.push((text, kind)),
                ShellMsg::Done => {
                    s.running = false;
                    s.output_lines.push(("---".to_string(), OutputKind::System));
                }
            }
        }

        // Render
        {
            let s = state.lock().unwrap();
            terminal
                .draw(|f| render_shell_tui(f, &s))
                .map_err(|e| GlideshError::Other(e.to_string()))?;
        }

        // Poll for input
        if event::poll(std::time::Duration::from_millis(16))
            .map_err(|e| GlideshError::Other(e.to_string()))?
        {
            if let Event::Key(key_event) =
                event::read().map_err(|e| GlideshError::Other(e.to_string()))?
            {
                if key_event.kind != KeyEventKind::Press {
                    continue;
                }

                let mut s = state.lock().unwrap();

                match key_event.code {
                    // Ctrl+D: exit
                    KeyCode::Char('d') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                        break;
                    }
                    // Ctrl+C: cancel running or exit
                    KeyCode::Char('c') if key_event.modifiers.contains(KeyModifiers::CONTROL) => {
                        if s.running {
                            // Just note it; tasks will finish on their own
                            s.output_lines.push(("^C".to_string(), OutputKind::System));
                        } else {
                            break;
                        }
                    }
                    // Enter: submit command
                    KeyCode::Enter if !s.running => {
                        let cmd = s.input.trim().to_string();
                        if cmd.is_empty() {
                            continue;
                        }
                        let host_count = s.host_count;
                        s.input.clear();
                        s.cursor_pos = 0;
                        s.running = true;
                        s.output_lines.push((
                            format!("$ {} ({} hosts)", cmd, host_count),
                            OutputKind::System,
                        ));
                        s.auto_scroll = true;
                        s.scroll = usize::MAX;

                        // Spawn command execution
                        let tx = output_tx.clone();
                        let hosts_vec: Vec<ResolvedHost> = hosts.to_vec();
                        let key_clone = key.clone();
                        let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));

                        tokio::spawn(async move {
                            run_on_all_hosts(
                                &hosts_vec,
                                &cmd,
                                &key_clone,
                                host_key_policy,
                                sem,
                                &tx,
                            )
                            .await;
                            let _ = tx.send(ShellMsg::Done);
                        });

                        // The sentinel is handled in the drain loop below
                        drop(s);
                        continue;
                    }
                    // Text editing
                    KeyCode::Char(c) if !s.running => {
                        let pos = s.cursor_pos;
                        s.input.insert(pos, c);
                        s.cursor_pos += c.len_utf8();
                    }
                    KeyCode::Backspace if !s.running => {
                        if s.cursor_pos > 0 {
                            let prev = s.input[..s.cursor_pos]
                                .chars()
                                .last()
                                .map(|c| c.len_utf8())
                                .unwrap_or(0);
                            s.cursor_pos -= prev;
                            let pos = s.cursor_pos;
                            s.input.remove(pos);
                        }
                    }
                    KeyCode::Left if !s.running => {
                        if s.cursor_pos > 0 {
                            let prev = s.input[..s.cursor_pos]
                                .chars()
                                .last()
                                .map(|c| c.len_utf8())
                                .unwrap_or(0);
                            s.cursor_pos -= prev;
                        }
                    }
                    KeyCode::Right if !s.running => {
                        if s.cursor_pos < s.input.len() {
                            let next = s.input[s.cursor_pos..]
                                .chars()
                                .next()
                                .map(|c| c.len_utf8())
                                .unwrap_or(0);
                            s.cursor_pos += next;
                        }
                    }
                    KeyCode::Home => {
                        if s.running {
                            s.auto_scroll = false;
                            s.scroll = 0;
                        } else {
                            s.cursor_pos = 0;
                        }
                    }
                    KeyCode::End => {
                        if s.running {
                            s.auto_scroll = true;
                            s.scroll = usize::MAX;
                        } else {
                            s.cursor_pos = s.input.len();
                        }
                    }
                    // Scroll while running
                    KeyCode::Up | KeyCode::PageUp => {
                        let amount = if key_event.code == KeyCode::PageUp {
                            10
                        } else {
                            1
                        };
                        s.auto_scroll = false;
                        s.scroll = s.scroll.saturating_sub(amount);
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        let amount = if key_event.code == KeyCode::PageDown {
                            10
                        } else {
                            1
                        };
                        s.scroll = s.scroll.saturating_add(amount);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

async fn run_on_all_hosts(
    hosts: &[ResolvedHost],
    command: &str,
    key: &PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
    semaphore: Arc<tokio::sync::Semaphore>,
    tx: &mpsc::UnboundedSender<ShellMsg>,
) {
    let mut handles = Vec::new();

    for host in hosts {
        let tx = tx.clone();
        let key = key.clone();
        let host = host.clone();
        let command = command.to_string();
        let sem = semaphore.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let name = host.name.clone();

            let session = match connect_to_host(&host, &key, policy).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(ShellMsg::Line(
                        format!("[{}] Connection failed: {}", name, e),
                        OutputKind::Stderr,
                    ));
                    return;
                }
            };

            match session.exec(&command).await {
                Ok(output) => {
                    for line in output.stdout.lines() {
                        let _ = tx.send(ShellMsg::Line(
                            format!("[{}] {}", name, line),
                            OutputKind::Stdout,
                        ));
                    }
                    for line in output.stderr.lines() {
                        let _ = tx.send(ShellMsg::Line(
                            format!("[{}] {}", name, line),
                            OutputKind::Stderr,
                        ));
                    }
                    if output.exit_code != 0 {
                        let _ = tx.send(ShellMsg::Line(
                            format!("[{}] (exit code {})", name, output.exit_code),
                            OutputKind::Stderr,
                        ));
                    }
                }
                Err(e) => {
                    let _ = tx.send(ShellMsg::Line(
                        format!("[{}] Command failed: {}", name, e),
                        OutputKind::Stderr,
                    ));
                }
            }
            let _ = session.close().await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }
}

async fn connect_to_host(
    host: &ResolvedHost,
    key: &PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
) -> Result<SshSession, GlideshError> {
    match &host.jump {
        Some(jump) => {
            SshSession::connect_via_jump(&host.address, host.port, &host.user, key, policy, jump)
                .await
        }
        None => SshSession::connect(&host.address, host.port, &host.user, key, policy).await,
    }
}

fn render_shell_tui(frame: &mut ratatui::Frame, state: &ShellTuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // output
            Constraint::Length(3), // input
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    render_output(frame, chunks[0], state);
    render_input(frame, chunks[1], state);
    render_footer(frame, chunks[2], state);
}

fn render_output(frame: &mut ratatui::Frame, area: Rect, state: &ShellTuiState) {
    let inner_width = area.width.saturating_sub(3) as usize;
    let visible_height = area.height.saturating_sub(2) as usize;

    let styled_lines: Vec<(String, Style)> = state
        .output_lines
        .iter()
        .flat_map(|(line, kind)| {
            let style = match kind {
                OutputKind::Stdout => Style::default().fg(Color::White),
                OutputKind::Stderr => Style::default().fg(Color::Red),
                OutputKind::System => Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            };
            super::widgets::wrap_line(line, inner_width)
                .into_iter()
                .map(move |s| (s.to_string(), style))
        })
        .collect();

    let total = styled_lines.len();
    let max_scroll = total.saturating_sub(visible_height);
    let scroll = state.scroll.min(max_scroll);
    let end = (scroll + visible_height).min(total);

    let items: Vec<ListItem> = styled_lines[scroll..end]
        .iter()
        .map(|(text, style)| ListItem::new(Line::from(Span::styled(text.clone(), *style))))
        .collect();

    let border_color = if state.running {
        COLOR_ACCENT
    } else {
        COLOR_BORDER_INACTIVE
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(" Output ");

    let list = List::new(items).block(block);
    frame.render_widget(list, area);

    if total > visible_height {
        let scrollbar_area = Rect {
            x: area.x + area.width - 1,
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };
        let mut scrollbar_state =
            ScrollbarState::new(total.saturating_sub(visible_height)).position(scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("\u{25b2}"))
            .end_symbol(Some("\u{25bc}"))
            .track_symbol(Some("\u{2591}"))
            .thumb_symbol("\u{2588}");
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }
}

fn render_input(frame: &mut ratatui::Frame, area: Rect, state: &ShellTuiState) {
    let border_color = if state.running {
        COLOR_BORDER_INACTIVE
    } else {
        COLOR_ACCENT
    };

    let display_text = if state.running {
        "Running...".to_string()
    } else {
        state.input.clone()
    };

    let paragraph = Paragraph::new(Line::from(vec![
        Span::styled(
            "> ",
            Style::default()
                .fg(COLOR_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(display_text, Style::default().fg(Color::White)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(" Command "),
    );
    frame.render_widget(paragraph, area);

    // Place cursor
    if !state.running {
        let cursor_x = area.x + 3 + state.cursor_pos as u16;
        let cursor_y = area.y + 1;
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

fn render_footer(frame: &mut ratatui::Frame, area: Rect, state: &ShellTuiState) {
    let text = if state.running {
        " \u{2191}\u{2193} scroll  Ctrl+C cancel  "
    } else {
        " Enter run  \u{2191}\u{2193} scroll  Ctrl+D exit  "
    };

    let paragraph = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(paragraph, area);
}
