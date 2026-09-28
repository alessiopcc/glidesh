use glidesh::modules::detect::OsInfo;
use serde::Serialize;
#[derive(Debug, Clone)]
pub struct NodeResult {
    pub success: bool,
    pub total_changed: usize,
    pub total_skipped: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub total_hosts: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub total_changed: usize,
    /// Tasks not run because a `when=` was false — a skipped step counts each of its tasks.
    pub total_skipped: usize,
    /// Hosts a stopped rolling run never started. Included in `total_hosts`.
    pub aborted: usize,
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

/// The skipped count as a summary suffix — empty when nothing was skipped, so a plan
/// without `when=` reads exactly as it always has. Shared by the plain output and the TUI.
pub fn skipped_suffix(skipped: usize) -> String {
    if skipped == 0 {
        String::new()
    } else {
        format!(", {skipped} skipped")
    }
}

/// How a `StepWaiting` event reads, in the plain output, the TUI and the run log alike.
pub fn waiting_text(command: &str, elapsed_secs: u64, timeout_secs: u64, preview: bool) -> String {
    if preview {
        format!("until not met yet: {command} (a run would wait up to {timeout_secs}s)")
    } else if elapsed_secs == 0 {
        format!("waiting until: {command} (up to {timeout_secs}s)")
    } else {
        format!("still waiting ({elapsed_secs}s of {timeout_secs}s) until: {command}")
    }
}

/// The aborted count as a summary suffix — empty unless a rolling run was stopped.
pub fn aborted_suffix(aborted: usize) -> String {
    if aborted == 0 {
        String::new()
    } else {
        format!(", {aborted} aborted")
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
    /// A step's `when=` did not hold, or `--tags`/`--skip-tags` left it out, so none of its
    /// tasks ran.
    StepSkipped {
        host: String,
        step: String,
        /// Tasks in the step, so a renderer can count them without the plan.
        tasks: usize,
        reason: String,
    },
    /// A step's `until=` gate has not opened yet. Sent on the first failed attempt and then
    /// at most every `WAIT_REPORT_EVERY`, so a long wait shows progress without flooding.
    StepWaiting {
        host: String,
        step: String,
        /// As written in the plan, like `TaskSkipped::resource`.
        command: String,
        elapsed_secs: u64,
        timeout_secs: u64,
        /// A preview checks the gate once and never waits; this reports it closed.
        preview: bool,
    },
    /// A task's `when=` did not hold.
    TaskSkipped {
        host: String,
        module: String,
        /// As written in the plan: interpolating it could fail on the very variable whose
        /// absence caused the skip.
        resource: String,
        reason: String,
    },
    /// A rolling run is starting its next batch. Sent only when there is more than one.
    BatchStarted {
        /// Zero-based.
        index: usize,
        total: usize,
        hosts: Vec<String>,
    },
    /// A rolling run stopped — see [`crate::executor::rollout::stop_reason`] for when — and
    /// these hosts were never started.
    HostsAborted {
        hosts: Vec<String>,
        reason: String,
    },
    NodeComplete {
        host: String,
        success: bool,
        /// In a dry run this counts what *would* change — see `dry_run`.
        changed: usize,
        skipped: usize,
        dry_run: bool,
    },
    RunComplete {
        summary: RunSummary,
    },
}

#[cfg(test)]
mod tests {
    use super::{changed_label, skipped_suffix, waiting_text};

    #[test]
    fn waiting_says_how_long_and_a_preview_that_it_would_wait() {
        assert_eq!(
            waiting_text("test -e /r", 0, 300, false),
            "waiting until: test -e /r (up to 300s)"
        );
        assert_eq!(
            waiting_text("test -e /r", 60, 300, false),
            "still waiting (60s of 300s) until: test -e /r"
        );
        assert_eq!(
            waiting_text("test -e /r", 0, 300, true),
            "until not met yet: test -e /r (a run would wait up to 300s)"
        );
    }

    #[test]
    fn nothing_skipped_reads_as_before() {
        assert_eq!(skipped_suffix(0), "");
        assert_eq!(skipped_suffix(2), ", 2 skipped");
    }

    #[test]
    fn a_preview_never_claims_it_changed_something() {
        assert_eq!(changed_label(true, true), "would change");
        assert_eq!(changed_label(true, false), "changed");
        assert_eq!(changed_label(false, true), "ok");
        assert_eq!(changed_label(false, false), "ok");
    }
}
