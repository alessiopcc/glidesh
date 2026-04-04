pub mod logs_explorer;
pub mod shell_tui;
pub mod state;
pub mod widgets;

pub use logs_explorer::run_logs_tui;
pub use shell_tui::run_shell_tui;

use crate::executor::result::ExecutorEvent;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use russh_keys::key::PrivateKeyWithHashAlg;
use state::{FocusPanel, HostConnectionInfo, TuiState};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Check if we're in a TTY (interactive terminal).
pub fn is_tty() -> bool {
    crossterm::tty::IsTty::is_tty(&io::stdout())
}

/// Run the TUI event loop. Consumes executor events and renders the UI.
/// Returns when the run is complete and the user presses 'q'.
/// `hosts` is a slice of `(hostname, group_name, plan_name)` tuples.
pub async fn run_tui(
    mut event_rx: mpsc::UnboundedReceiver<ExecutorEvent>,
    plan_name: &str,
    hosts: &[(String, String, String)],
    engine_handle: tokio::task::AbortHandle,
    connection_info: Vec<HostConnectionInfo>,
    ssh_key: PrivateKeyWithHashAlg,
    host_key_policy: HostKeyPolicy,
) -> io::Result<bool> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(TuiState::new(plan_name, hosts, connection_info)));

    let state_clone = state.clone();

    let event_consumer = tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            let mut s = state_clone.lock().unwrap();
            s.handle_event(&event);
        }
    });

    let mut aborted = false;

    loop {
        {
            let mut s = state.lock().unwrap();
            s.tick_spinner();
            terminal.draw(|f| widgets::render(f, &s))?;
        }

        // Poll for keyboard events (16ms = ~60fps)
        let mut shell_request: Option<HostConnectionInfo> = None;

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let mut s = state.lock().unwrap();

                // Handle confirm-quit dialog
                if s.confirm_quit {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            aborted = true;
                            break;
                        }
                        _ => {
                            s.confirm_quit = false;
                            continue;
                        }
                    }
                }

                match key.code {
                    KeyCode::Char('q') => {
                        if s.run_complete {
                            break;
                        } else {
                            s.confirm_quit = true;
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Enter => {
                        if s.focus == FocusPanel::Nodes && s.viewing_node.is_none() {
                            s.enter_node_view();
                        }
                    }
                    KeyCode::Esc => {
                        if s.viewing_node.is_some() {
                            s.exit_node_view();
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        if s.viewing_node.is_some() {
                            s.exit_node_view();
                        } else {
                            s.toggle_focus();
                        }
                    }
                    KeyCode::Up => match s.focus {
                        FocusPanel::Nodes => s.prev_node(),
                        FocusPanel::Logs => s.scroll_log_up(1),
                    },
                    KeyCode::Down => match s.focus {
                        FocusPanel::Nodes => s.next_node(),
                        FocusPanel::Logs => s.scroll_log_down(1),
                    },
                    KeyCode::Char('j') => match s.focus {
                        FocusPanel::Nodes => s.next_node(),
                        FocusPanel::Logs => s.scroll_log_down(1),
                    },
                    KeyCode::Char('k') => match s.focus {
                        FocusPanel::Nodes => s.prev_node(),
                        FocusPanel::Logs => s.scroll_log_up(1),
                    },
                    KeyCode::PageUp => s.scroll_log_up(10),
                    KeyCode::PageDown => s.scroll_log_down(10),
                    KeyCode::Home | KeyCode::Char('g') => s.scroll_log_to_top(),
                    KeyCode::End | KeyCode::Char('G') => s.scroll_log_to_bottom(),
                    KeyCode::Char('s') if s.run_complete && s.focus == FocusPanel::Nodes => {
                        shell_request = Some(s.connection_info[s.selected_node].clone());
                    }
                    _ => {}
                }
            }
        }

        // Handle shell request outside the mutex scope
        if let Some(info) = shell_request {
            terminal::disable_raw_mode()?;
            io::stdout().execute(LeaveAlternateScreen)?;

            let shell_result = open_shell_for_host(&info, &ssh_key, host_key_policy).await;

            terminal::enable_raw_mode()?;
            io::stdout().execute(EnterAlternateScreen)?;
            terminal.clear()?;

            if let Err(e) = shell_result {
                let mut s = state.lock().unwrap();
                s.combined_log.push(format!("Shell error: {}", e));
            }
        }
    }

    if aborted {
        engine_handle.abort();
        event_consumer.abort();
    }

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    {
        let s = state.lock().unwrap();
        if aborted {
            println!("Aborted by user.");
        } else if let Some(ref summary) = s.summary_line {
            println!("{}", summary);
        }
    }

    if !aborted {
        let _ = event_consumer.await;
    }

    Ok(aborted)
}

async fn open_shell_for_host(
    info: &HostConnectionInfo,
    key: &PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
) -> Result<(), glidesh::error::GlideshError> {
    let session = match &info.jump {
        Some(jump) => {
            SshSession::connect_via_jump(&info.address, info.port, &info.user, key, policy, jump)
                .await?
        }
        None => SshSession::connect(&info.address, info.port, &info.user, key, policy).await?,
    };

    println!(
        "Connected to {}@{}. Type 'exit' to return to TUI.\r",
        info.user, info.address
    );
    let exit_code = session.interactive_shell().await?;
    session.close().await?;
    println!("\r\nShell exited (code {}).\r", exit_code);

    // Brief pause so user can see the exit message
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(())
}
