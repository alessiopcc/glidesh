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
    /// Nothing was applied: `total_changed` counts what *would* change.
    pub dry_run: bool,
}

/// How a task's outcome reads. A dry run reports intent, not history, so it must never
/// claim a change it did not make. Shared by all three renderers (TUI, plain stdout, run
/// log) so they cannot drift apart.
pub fn changed_label(changed: bool, dry_run: bool) -> &'static str {
    match (changed, dry_run) {
        (true, true) => "would change",
        (true, false) => "changed",
        (false, _) => "ok",
    }
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
        /// In a dry run this means "would change" — see `dry_run`.
        changed: bool,
        dry_run: bool,
        stdout: String,
        stderr: String,
        exit_code: i32,
    },
    ModuleFailed {
        host: String,
        module: String,
        resource: String,
        error: String,
    },
    StepFailed {
        host: String,
        step: String,
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

#[cfg(test)]
mod tests {
    use super::changed_label;

    #[test]
    fn a_preview_never_claims_it_changed_something() {
        assert_eq!(changed_label(true, true), "would change");
        assert_eq!(changed_label(true, false), "changed");
        assert_eq!(changed_label(false, true), "ok");
        assert_eq!(changed_label(false, false), "ok");
    }
}
