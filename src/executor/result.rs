use glidesh::modules::detect::OsInfo;
use serde::Serialize;
#[derive(Debug, Clone)]
pub struct NodeResult {
    pub success: bool,
    pub total_changed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub total_hosts: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub total_changed: usize,
}

/// Events emitted by the executor for TUI/logging consumption.
#[derive(Debug, Clone)]
pub enum ExecutorEvent {
    NodeConnecting {
        host: String,
    },
    NodeConnected {
        host: String,
        os: OsInfo,
    },
    NodeAuthFailed {
        host: String,
        error: String,
    },
    StepStarted {
        host: String,
        step: String,
        step_index: usize,
        total_steps: usize,
    },
    ModuleCheck {
        host: String,
        module: String,
        resource: String,
    },
    ModuleResult {
        host: String,
        module: String,
        resource: String,
        changed: bool,
    },
    ModuleFailed {
        host: String,
        module: String,
        resource: String,
        error: String,
    },
    NodeComplete {
        host: String,
        success: bool,
        changed: usize,
    },
    RunComplete {
        summary: RunSummary,
    },
}
