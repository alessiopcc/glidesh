use crate::executor::result::ExecutorEvent;
use glidesh::config::types::ResolvedJumpHost;
use std::collections::HashMap;
use std::time::Instant;

/// Connection details for a host, used to open a shell after plan completion.
#[derive(Debug, Clone)]
pub struct HostConnectionInfo {
    pub address: String,
    pub user: String,
    pub port: u16,
    pub jump: Option<ResolvedJumpHost>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FocusPanel {
    Nodes,
    Logs,
}

#[derive(Debug, Clone)]
pub struct NodeState {
    pub host: String,
    pub group_name: String,
    pub plan_name: String,
    pub status: NodeStatus,
    pub current_step: String,
    pub step_index: usize,
    pub total_steps: usize,
    pub changed: usize,
    pub os_id: String,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub log_lines: Vec<String>,
    pub error: Option<String>,
}

impl NodeState {
    /// Returns the display identifier: "group:host" if in a group, "host" otherwise.
    pub fn display_id(&self) -> String {
        if self.group_name.is_empty() {
            self.host.clone()
        } else {
            format!("{}:{}", self.group_name, self.host)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatus {
    Connecting,
    Running,
    Done,
    Failed,
}

impl std::fmt::Display for NodeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeStatus::Connecting => write!(f, "CONNECTING"),
            NodeStatus::Running => write!(f, "RUNNING"),
            NodeStatus::Done => write!(f, "OK"),
            NodeStatus::Failed => write!(f, "FAILED"),
        }
    }
}

pub struct TuiState {
    pub nodes: Vec<NodeState>,
    pub node_index: HashMap<String, usize>,
    pub selected_node: usize,
    pub log_scroll: usize,
    pub completed: usize,
    pub total: usize,
    pub run_complete: bool,
    pub summary_line: Option<String>,
    pub focus: FocusPanel,
    pub auto_scroll: bool,
    pub spinner_tick: usize,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub total_changed: usize,
    pub combined_log: Vec<String>,
    pub combined_scroll: usize,
    pub combined_auto_scroll: bool,
    /// None = combined view (main), Some(idx) = per-node view
    pub viewing_node: Option<usize>,
    /// Whether a quit confirmation dialog is showing
    pub confirm_quit: bool,
    /// Connection info per node for post-run shell access
    pub connection_info: Vec<HostConnectionInfo>,
}

impl TuiState {
    /// Create a new TUI state. `hosts` is a slice of `(hostname, group_name, plan_name)` tuples.
    pub fn new(
        hosts: &[(String, String, String)],
        connection_info: Vec<HostConnectionInfo>,
    ) -> Self {
        let now = Instant::now();
        let mut nodes = Vec::new();
        let mut node_index = HashMap::new();
        for (i, (host, group, host_plan)) in hosts.iter().enumerate() {
            node_index.insert(host.clone(), i);
            nodes.push(NodeState {
                host: host.clone(),
                group_name: group.clone(),
                plan_name: host_plan.clone(),
                status: NodeStatus::Connecting,
                current_step: "--".to_string(),
                step_index: 0,
                total_steps: 0,
                changed: 0,
                os_id: String::new(),
                started_at: now,
                finished_at: None,
                log_lines: Vec::new(),
                error: None,
            });
        }

        TuiState {
            nodes,
            node_index,
            selected_node: 0,
            log_scroll: 0,
            completed: 0,
            total: hosts.len(),
            run_complete: false,
            summary_line: None,
            focus: FocusPanel::Nodes,
            auto_scroll: true,
            spinner_tick: 0,
            started_at: now,
            finished_at: None,
            total_changed: 0,
            combined_log: Vec::new(),
            combined_scroll: usize::MAX,
            combined_auto_scroll: true,
            viewing_node: None,
            confirm_quit: false,
            connection_info,
        }
    }

    fn push_node_log(&mut self, host: &str, line: String) {
        if let Some(&idx) = self.node_index.get(host) {
            self.nodes[idx].log_lines.push(line.clone());
            let id = self.nodes[idx].display_id();
            self.combined_log.push(format!("[{}] {}", id, line));
        }
    }

    pub fn handle_event(&mut self, event: &ExecutorEvent) {
        match event {
            ExecutorEvent::NodeConnecting { host } => {
                if let Some(&idx) = self.node_index.get(host) {
                    self.nodes[idx].status = NodeStatus::Connecting;
                }
                self.push_node_log(host, "Connecting...".to_string());
            }
            ExecutorEvent::NodeConnected { host, os } => {
                if let Some(&idx) = self.node_index.get(host) {
                    self.nodes[idx].status = NodeStatus::Running;
                    self.nodes[idx].os_id = os.id.clone();
                }
                self.push_node_log(host, format!("Connected ({})", os.id));
            }
            ExecutorEvent::NodeAuthFailed { host, error } => {
                if let Some(&idx) = self.node_index.get(host) {
                    self.nodes[idx].status = NodeStatus::Failed;
                    self.nodes[idx].finished_at = Some(Instant::now());
                    self.nodes[idx].error = Some(error.clone());
                    self.completed += 1;
                }
                self.push_node_log(host, format!("AUTH FAILED: {}", error));
            }
            ExecutorEvent::StepStarted {
                host,
                step,
                step_index,
                total_steps,
            } => {
                if let Some(&idx) = self.node_index.get(host) {
                    self.nodes[idx].current_step = step.clone();
                    self.nodes[idx].step_index = *step_index;
                    self.nodes[idx].total_steps = *total_steps;
                }
                self.push_node_log(
                    host,
                    format!("── Step {}/{}: {} ──", step_index + 1, total_steps, step),
                );
            }
            ExecutorEvent::ModuleCheck {
                host,
                module,
                resource,
            } => {
                self.push_node_log(host, format!("CHECK {} '{}'", module, resource));
            }
            ExecutorEvent::ModuleResult {
                host,
                module,
                resource,
                changed,
            } => {
                let status = if *changed { "changed" } else { "ok" };
                self.push_node_log(host, format!("  {} '{}': {}", module, resource, status));
                if *changed {
                    if let Some(&idx) = self.node_index.get(host) {
                        self.nodes[idx].changed += 1;
                        self.total_changed += 1;
                    }
                }
            }
            ExecutorEvent::ModuleFailed {
                host,
                module,
                resource,
                error,
            } => {
                self.push_node_log(
                    host,
                    format!("  FAILED {} '{}': {}", module, resource, error),
                );
            }
            ExecutorEvent::NodeComplete {
                host,
                success,
                changed: _,
            } => {
                if let Some(&idx) = self.node_index.get(host) {
                    let already_finished = self.nodes[idx].finished_at.is_some();
                    self.nodes[idx].status = if *success {
                        NodeStatus::Done
                    } else {
                        NodeStatus::Failed
                    };
                    self.nodes[idx].finished_at = Some(Instant::now());
                    self.nodes[idx].current_step = "--".to_string();
                    if !already_finished {
                        self.completed += 1;
                    }
                }
            }
            ExecutorEvent::RunComplete { summary } => {
                self.run_complete = true;
                self.finished_at = Some(Instant::now());
                self.summary_line = Some(format!(
                    "Complete: {} hosts, {} ok, {} failed, {} changed",
                    summary.total_hosts, summary.succeeded, summary.failed, summary.total_changed
                ));
            }
        }

        if self.auto_scroll {
            self.log_scroll = usize::MAX;
        }
        if self.combined_auto_scroll {
            self.combined_scroll = usize::MAX;
        }
    }

    /// Returns the currently active log lines (combined or per-node).
    pub fn active_logs(&self) -> &[String] {
        match self.viewing_node {
            None => &self.combined_log,
            Some(idx) => {
                if idx < self.nodes.len() {
                    &self.nodes[idx].log_lines
                } else {
                    &[]
                }
            }
        }
    }

    /// Returns the current scroll position for the active log view.
    pub fn active_scroll(&self) -> usize {
        match self.viewing_node {
            None => self.combined_scroll,
            Some(_) => self.log_scroll,
        }
    }

    pub fn set_active_scroll(&mut self, value: usize) {
        match self.viewing_node {
            None => self.combined_scroll = value,
            Some(_) => self.log_scroll = value,
        }
    }

    /// Returns the log panel title based on active view.
    pub fn log_title(&self) -> String {
        match self.viewing_node {
            None => " Log: all ".to_string(),
            Some(idx) => {
                if idx < self.nodes.len() {
                    format!(" Log: {} ", self.nodes[idx].host)
                } else {
                    " Log: ? ".to_string()
                }
            }
        }
    }

    pub fn enter_node_view(&mut self) {
        self.viewing_node = Some(self.selected_node);
        self.log_scroll = usize::MAX;
        self.auto_scroll = true;
    }

    pub fn exit_node_view(&mut self) {
        self.viewing_node = None;
    }

    pub fn next_node(&mut self) {
        if !self.nodes.is_empty() {
            self.selected_node = (self.selected_node + 1) % self.nodes.len();
            if self.viewing_node.is_some() {
                self.viewing_node = Some(self.selected_node);
                self.log_scroll = usize::MAX;
                self.auto_scroll = true;
            }
        }
    }

    pub fn prev_node(&mut self) {
        if !self.nodes.is_empty() {
            self.selected_node = if self.selected_node == 0 {
                self.nodes.len() - 1
            } else {
                self.selected_node - 1
            };
            if self.viewing_node.is_some() {
                self.viewing_node = Some(self.selected_node);
                self.log_scroll = usize::MAX;
                self.auto_scroll = true;
            }
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusPanel::Nodes => FocusPanel::Logs,
            FocusPanel::Logs => FocusPanel::Nodes,
        };
    }

    pub fn scroll_log_up(&mut self, amount: usize) {
        match self.viewing_node {
            None => {
                self.combined_auto_scroll = false;
                self.combined_scroll = self.combined_scroll.saturating_sub(amount);
            }
            Some(_) => {
                self.auto_scroll = false;
                self.log_scroll = self.log_scroll.saturating_sub(amount);
            }
        }
    }

    pub fn scroll_log_down(&mut self, amount: usize) {
        match self.viewing_node {
            None => {
                self.combined_scroll = self.combined_scroll.saturating_add(amount);
            }
            Some(_) => {
                self.log_scroll = self.log_scroll.saturating_add(amount);
            }
        }
    }

    pub fn scroll_log_to_top(&mut self) {
        match self.viewing_node {
            None => {
                self.combined_auto_scroll = false;
                self.combined_scroll = 0;
            }
            Some(_) => {
                self.auto_scroll = false;
                self.log_scroll = 0;
            }
        }
    }

    pub fn scroll_log_to_bottom(&mut self) {
        match self.viewing_node {
            None => {
                self.combined_auto_scroll = true;
                self.combined_scroll = usize::MAX;
            }
            Some(_) => {
                self.auto_scroll = true;
                self.log_scroll = usize::MAX;
            }
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        match self.finished_at {
            Some(t) => t.duration_since(self.started_at),
            None => self.started_at.elapsed(),
        }
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
    }
}
