pub mod logs_explorer;
pub mod state;
pub mod widgets;

pub use logs_explorer::run_logs_tui;

use crate::executor::result::ExecutorEvent;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use state::{FocusPanel, TuiState};
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
) -> io::Result<bool> {
    terminal::enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(Mutex::new(TuiState::new(plan_name, hosts)));

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
                    _ => {}
                }
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

/// Non-TUI fallback: just prints events line by line (for CI / piped output).
#[allow(dead_code)]
pub async fn run_line_output(
    mut event_rx: mpsc::UnboundedReceiver<ExecutorEvent>,
    mut logger: Option<&mut crate::logging::RunLogger>,
) {
    while let Some(event) = event_rx.recv().await {
        if let Some(ref mut logger) = logger.as_deref_mut() {
            logger.handle_event(&event);
        }

        match &event {
            ExecutorEvent::NodeConnecting { host } => {
                println!("[{}] Connecting...", host);
            }
            ExecutorEvent::NodeConnected { host, os } => {
                println!("[{}] Connected ({})", host, os.id);
            }
            ExecutorEvent::NodeAuthFailed { host, error } => {
                eprintln!("[{}] Auth failed: {}", host, error);
            }
            ExecutorEvent::StepStarted {
                host,
                step,
                step_index,
                total_steps,
            } => {
                println!(
                    "[{}] Step {}/{}: {}",
                    host,
                    step_index + 1,
                    total_steps,
                    step
                );
            }
            ExecutorEvent::ModuleCheck {
                host,
                module,
                resource,
            } => {
                println!("[{}]   Checking {} '{}'", host, module, resource);
            }
            ExecutorEvent::ModuleResult {
                host,
                module,
                resource,
                changed,
            } => {
                let status = if *changed { "changed" } else { "ok" };
                println!("[{}]   {} '{}': {}", host, module, resource, status);
            }
            ExecutorEvent::ModuleFailed {
                host,
                module,
                resource,
                error,
            } => {
                eprintln!("[{}]   FAILED {} '{}': {}", host, module, resource, error);
            }
            ExecutorEvent::OutputLine { host, line } => {
                println!("[{}]   > {}", host, line);
            }
            ExecutorEvent::NodeComplete {
                host,
                success,
                changed,
            } => {
                let status = if *success { "OK" } else { "FAILED" };
                println!("[{}] {} ({} changed)", host, status, changed);
            }
            ExecutorEvent::RunComplete { summary } => {
                println!("\n--- Run Complete ---");
                println!(
                    "Hosts: {} total, {} ok, {} failed, {} changed",
                    summary.total_hosts, summary.succeeded, summary.failed, summary.total_changed
                );
                if let Some(ref mut logger) = logger.as_deref_mut() {
                    let _ = logger.write_summary();
                }
            }
        }
    }
}
