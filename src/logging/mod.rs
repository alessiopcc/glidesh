pub mod storage;

use crate::executor::result::{ExecutorEvent, waiting_text};
use chrono::Utc;
use glidesh::error::GlideshError;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use storage::{NodeSummary, RunSummaryFile};

/// Per-stream cap for command output written to a node log (bytes).
const MAX_STREAM_LOG_BYTES: usize = 8 * 1024;

/// Truncate `content` to at most [`MAX_STREAM_LOG_BYTES`] on a UTF-8 char
/// boundary. Returns the (possibly shortened) slice and whether it was cut.
fn truncate_stream(content: &str) -> (&str, bool) {
    if content.len() <= MAX_STREAM_LOG_BYTES {
        return (content, false);
    }
    let mut end = MAX_STREAM_LOG_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (&content[..end], true)
}

/// Format a captured stream (`stdout`/`stderr`) into indented, labelled log lines,
/// capped at [`MAX_STREAM_LOG_BYTES`] with a `... [truncated]` marker. Shared by the
/// run log, the TUI, and the CLI so all three honor the same cap and a chatty command
/// can't bloat any of them. Returns an empty vec for an empty stream.
pub(crate) fn stream_log_lines(label: &str, content: &str) -> Vec<String> {
    let trimmed = content.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return Vec::new();
    }
    let (body, truncated) = truncate_stream(trimmed);
    let mut lines: Vec<String> = body
        .lines()
        .map(|line| format!("    {} | {}", label, line))
        .collect();
    if truncated {
        lines.push(format!("    {} | ... [truncated]", label));
    }
    lines
}

pub struct RunLogger {
    run_dir: PathBuf,
    run_id: String,
    plan_name: String,
    started_at: chrono::DateTime<Utc>,
    node_files: HashMap<String, fs::File>,
    node_summaries: HashMap<String, NodeSummary>,
    /// Per host. A skipped step is started too, so this counts every step a host reached.
    steps_started: HashMap<String, usize>,
    /// Learned from the closing `RunComplete`, so a saved summary records whether its
    /// `changed` counts were applied or merely previewed.
    dry_run: bool,
}

impl RunLogger {
    pub fn new(plan_name: &str) -> Result<Self, GlideshError> {
        Self::new_in(&storage::runs_dir(), plan_name)
    }

    /// `new`, with the parent directory given rather than taken from the home directory.
    fn new_in(runs_dir: &std::path::Path, plan_name: &str) -> Result<Self, GlideshError> {
        let run_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let now = Utc::now();
        let dir_name = format!("{}_{}", now.format("%Y-%m-%dT%H-%M-%S"), plan_name);
        let run_dir = runs_dir.join(&dir_name);
        fs::create_dir_all(&run_dir)?;

        Ok(RunLogger {
            run_dir,
            run_id,
            plan_name: plan_name.to_string(),
            started_at: now,
            node_files: HashMap::new(),
            node_summaries: HashMap::new(),
            steps_started: HashMap::new(),
            dry_run: false,
        })
    }

    pub fn run_dir(&self) -> &PathBuf {
        &self.run_dir
    }

    fn get_node_file(&mut self, host: &str) -> Result<&mut fs::File, GlideshError> {
        if !self.node_files.contains_key(host) {
            let path = self.run_dir.join(format!("{}.log", host));
            let file = fs::File::create(&path)?;
            self.node_files.insert(host.to_string(), file);
        }
        Ok(self.node_files.get_mut(host).unwrap())
    }

    fn log_line(&mut self, host: &str, line: &str) {
        let timestamp = Utc::now().format("%H:%M:%S");
        if let Ok(file) = self.get_node_file(host) {
            let _ = writeln!(file, "[{}] {}", timestamp, line);
        }
    }

    /// Write a captured command stream (stdout/stderr) under a `[RESULT]` line,
    /// each source line indented and labelled. Empty streams are skipped; very
    /// large streams are truncated so a chatty command can't bloat the node log.
    fn log_stream(&mut self, host: &str, label: &str, content: &str) {
        let trimmed = content.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            return;
        }
        let lines = stream_log_lines(label, trimmed);
        if let Ok(file) = self.get_node_file(host) {
            for line in &lines {
                let _ = writeln!(file, "{}", line);
            }
        }
    }

    pub fn handle_event(&mut self, event: &ExecutorEvent) {
        match event {
            ExecutorEvent::BatchStarted {
                index,
                total,
                hosts,
            } => {
                for host in hosts {
                    self.log_line(host, &format!("[BATCH] {}/{}", index + 1, total));
                }
            }
            ExecutorEvent::HostsAborted { hosts, reason } => {
                for host in hosts {
                    self.log_line(host, &format!("[ABORTED] not started: {}", reason));
                    self.node_summaries.insert(
                        host.clone(),
                        NodeSummary {
                            status: "aborted".to_string(),
                            changed: 0,
                            skipped: 0,
                            steps_completed: 0,
                            failed_step: None,
                            error: Some(reason.clone()),
                        },
                    );
                }
            }
            ExecutorEvent::NodeConnecting { host } => {
                self.log_line(host, "[CONNECTING]");
                self.node_summaries.insert(
                    host.clone(),
                    NodeSummary {
                        status: "connecting".to_string(),
                        changed: 0,
                        skipped: 0,
                        steps_completed: 0,
                        failed_step: None,
                        error: None,
                    },
                );
            }
            ExecutorEvent::NodeConnected { host, os } => {
                self.log_line(host, &format!("[CONNECTED] OS: {}", os.id));
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.status = "running".to_string();
                }
            }
            ExecutorEvent::NodeAuthFailed { host, error } => {
                self.log_line(host, &format!("[AUTH FAILED] {}", error));
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.status = "failed".to_string();
                    summary.error = Some(error.clone());
                }
            }
            ExecutorEvent::StepStarted {
                host,
                step,
                step_index,
                total_steps,
            } => {
                self.log_line(
                    host,
                    &format!("[step: {}] ({}/{})", step, step_index + 1, total_steps),
                );
                *self.steps_started.entry(host.clone()).or_default() += 1;
            }
            ExecutorEvent::ModuleCheck {
                host,
                module,
                resource,
            } => {
                self.log_line(
                    host,
                    &format!("[CHECK] [module: {}] [resource: {}]", module, resource),
                );
            }
            ExecutorEvent::ModuleResult {
                host,
                module,
                resource,
                changed,
                dry_run,
                stdout,
                stderr,
                exit_code,
            } => {
                let status = crate::executor::changed_label(*changed, *dry_run);
                self.log_line(
                    host,
                    &format!(
                        "[RESULT] [module: {}] [resource: {}] {} (exit {})",
                        module, resource, status, exit_code
                    ),
                );
                self.log_stream(host, "stdout", stdout);
                self.log_stream(host, "stderr", stderr);
                if *changed {
                    if let Some(summary) = self.node_summaries.get_mut(host) {
                        summary.changed += 1;
                    }
                }
            }
            ExecutorEvent::ModuleFailed {
                host,
                module,
                resource,
                error,
            } => {
                self.log_line(
                    host,
                    &format!(
                        "[FAILED] [module: {}] [resource: {}] {}",
                        module, resource, error
                    ),
                );
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.error = Some(error.clone());
                }
            }
            ExecutorEvent::StepFailed { host, step, error } => {
                self.log_line(host, &format!("[FAILED] [step: {}] {}", step, error));
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.failed_step = Some(step.clone());
                    summary.error = Some(error.clone());
                }
            }
            ExecutorEvent::StepSkipped {
                host,
                step,
                tasks,
                reason,
            } => {
                self.log_line(host, &format!("[SKIPPED] [step: {}] {}", step, reason));
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.skipped += tasks;
                }
            }
            ExecutorEvent::StepWaiting {
                host,
                step,
                command,
                elapsed_secs,
                timeout_secs,
                preview,
            } => {
                self.log_line(
                    host,
                    &format!(
                        "[WAITING] [step: {}] {}",
                        step,
                        waiting_text(command, *elapsed_secs, *timeout_secs, *preview)
                    ),
                );
            }
            ExecutorEvent::TaskSkipped {
                host,
                module,
                resource,
                reason,
            } => {
                self.log_line(
                    host,
                    &format!(
                        "[SKIPPED] [module: {}] [resource: {}] {}",
                        module, resource, reason
                    ),
                );
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.skipped += 1;
                }
            }
            ExecutorEvent::NodeComplete {
                host,
                success,
                changed,
                skipped,
                dry_run,
            } => {
                let status = if *success { "ok" } else { "failed" };
                let counted = if *dry_run { "would_change" } else { "changed" };
                // Only when non-zero, so a plan without `when=` logs exactly as before.
                let skipped = if *skipped > 0 {
                    format!(" skipped={skipped}")
                } else {
                    String::new()
                };
                self.log_line(
                    host,
                    &format!(
                        "[COMPLETE] status={} {}={}{}",
                        status, counted, changed, skipped
                    ),
                );
                // A host stops at the first failure, so a failed host's last started step
                // is the one that failed.
                let started = self.steps_started.get(host).copied().unwrap_or(0);
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.status = status.to_string();
                    summary.steps_completed = if *success {
                        started
                    } else {
                        started.saturating_sub(1)
                    };
                }
            }
            ExecutorEvent::RunComplete { summary } => self.dry_run = summary.dry_run,
        }
    }

    pub fn write_summary(&self) -> Result<(), GlideshError> {
        let summary = RunSummaryFile {
            run_id: self.run_id.clone(),
            plan: self.plan_name.clone(),
            started_at: self.started_at,
            finished_at: Some(Utc::now()),
            dry_run: self.dry_run,
            nodes: self.node_summaries.clone(),
        };

        let path = self.run_dir.join("summary.json");
        let content = serde_json::to_string_pretty(&summary)
            .map_err(|e| GlideshError::Other(format!("Failed to serialize summary: {}", e)))?;
        fs::write(&path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_stream_is_not_truncated() {
        let (body, truncated) = truncate_stream("hello world");
        assert_eq!(body, "hello world");
        assert!(!truncated);
    }

    #[test]
    fn oversized_stream_is_truncated_on_char_boundary() {
        let input = "é".repeat(MAX_STREAM_LOG_BYTES); // 2 bytes each
        let (body, truncated) = truncate_stream(&input);
        assert!(truncated);
        assert!(body.len() <= MAX_STREAM_LOG_BYTES);
        // Truncation must not split a multi-byte char.
        assert!(std::str::from_utf8(body.as_bytes()).is_ok());
    }

    fn summary(dry_run: bool) -> ExecutorEvent {
        ExecutorEvent::RunComplete {
            summary: crate::executor::result::RunSummary {
                total_hosts: 1,
                succeeded: 1,
                failed: 0,
                total_changed: 1,
                total_skipped: 0,
                aborted: 0,
                dry_run,
            },
        }
    }

    fn module_result(dry_run: bool) -> ExecutorEvent {
        ExecutorEvent::ModuleResult {
            host: "web-1".to_string(),
            module: "file".to_string(),
            resource: "/etc/app.conf".to_string(),
            changed: true,
            dry_run,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    fn logger(runs_dir: &std::path::Path) -> RunLogger {
        let mut logger = RunLogger::new_in(runs_dir, "deploy").unwrap();
        logger.handle_event(&ExecutorEvent::NodeConnecting {
            host: "web-1".to_string(),
        });
        logger
    }

    /// A saved summary must not be mistakable for an applied run.
    #[test]
    fn a_previewed_run_is_recorded_as_a_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&module_result(true));
        logger.handle_event(&summary(true));
        logger.write_summary().unwrap();

        let saved = storage::read_summary(logger.run_dir()).unwrap();
        assert!(saved.dry_run);
        assert_eq!(saved.nodes["web-1"].changed, 1);
    }

    #[test]
    fn an_applied_run_is_recorded_as_a_real_run() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&module_result(false));
        logger.handle_event(&summary(false));
        logger.write_summary().unwrap();

        assert!(!storage::read_summary(logger.run_dir()).unwrap().dry_run);
    }

    #[test]
    fn the_node_log_labels_a_previewed_task_as_would_change() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&module_result(true));

        let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
        assert!(log.contains("would change"), "got: {log}");
    }

    /// The per-host tally is keyed by what it counted, so a preview's log cannot be read
    /// as a record of applied changes.
    #[test]
    fn the_node_log_keys_the_completion_tally_by_run_mode() {
        for (dry_run, expected, absent) in [
            (true, "would_change=1", "changed=1"),
            (false, "changed=1", "would_change=1"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let mut logger = logger(tmp.path());
            logger.handle_event(&module_result(dry_run));
            logger.handle_event(&ExecutorEvent::NodeComplete {
                host: "web-1".to_string(),
                success: true,
                changed: 1,
                skipped: 0,
                dry_run,
            });

            let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
            assert!(log.contains(expected), "expected {expected} in: {log}");
            assert!(!log.contains(absent), "unexpected {absent} in: {log}");
        }
    }

    fn step_started(index: usize) -> ExecutorEvent {
        ExecutorEvent::StepStarted {
            host: "web-1".to_string(),
            step: format!("step {index}"),
            step_index: index,
            total_steps: 3,
        }
    }

    fn steps_completed_after(started: usize, success: bool) -> usize {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        for index in 0..started {
            logger.handle_event(&step_started(index));
        }
        logger.handle_event(&ExecutorEvent::NodeComplete {
            host: "web-1".to_string(),
            success,
            changed: 0,
            skipped: 0,
            dry_run: false,
        });
        logger.write_summary().unwrap();
        storage::read_summary(logger.run_dir()).unwrap().nodes["web-1"].steps_completed
    }

    #[test]
    fn a_successful_host_completed_every_step_it_started() {
        assert_eq!(steps_completed_after(3, true), 3);
        assert_eq!(steps_completed_after(0, true), 0);
    }

    #[test]
    fn a_failed_host_did_not_complete_the_step_it_failed_in() {
        assert_eq!(steps_completed_after(2, false), 1);
        assert_eq!(
            steps_completed_after(0, false),
            0,
            "failed before its first step"
        );
    }

    #[test]
    fn skips_are_logged_and_counted_per_task() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&ExecutorEvent::StepSkipped {
            host: "web-1".to_string(),
            step: "Install".to_string(),
            tasks: 2,
            reason: "when: ${x}".to_string(),
        });
        logger.handle_event(&ExecutorEvent::TaskSkipped {
            host: "web-1".to_string(),
            module: "shell".to_string(),
            resource: "uptime".to_string(),
            reason: "when: ${y}".to_string(),
        });
        logger.handle_event(&ExecutorEvent::NodeComplete {
            host: "web-1".to_string(),
            success: true,
            changed: 0,
            skipped: 3,
            dry_run: false,
        });
        logger.handle_event(&summary(false));
        logger.write_summary().unwrap();

        let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
        assert!(
            log.contains("[SKIPPED] [step: Install] when: ${x}"),
            "{log}"
        );
        assert!(
            log.contains("[SKIPPED] [module: shell] [resource: uptime] when: ${y}"),
            "{log}"
        );
        assert!(log.contains("changed=0 skipped=3"), "{log}");
        let saved = storage::read_summary(logger.run_dir()).unwrap();
        assert_eq!(saved.nodes["web-1"].skipped, 3);
    }

    #[test]
    fn a_wait_is_logged_without_counting_as_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&ExecutorEvent::StepWaiting {
            host: "web-1".to_string(),
            step: "Wait".to_string(),
            command: "test -e /ready".to_string(),
            elapsed_secs: 30,
            timeout_secs: 300,
            preview: false,
        });
        let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
        assert!(
            log.contains(
                "[WAITING] [step: Wait] still waiting (30s of 300s) until: test -e /ready"
            ),
            "{log}"
        );
    }

    /// Log parsers see no new key unless the plan actually skipped something.
    #[test]
    fn a_run_without_skips_logs_no_skipped_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&ExecutorEvent::NodeComplete {
            host: "web-1".to_string(),
            success: true,
            changed: 1,
            skipped: 0,
            dry_run: false,
        });
        let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
        assert!(!log.contains("skipped"), "{log}");
    }

    #[test]
    fn a_summary_without_skips_has_no_skipped_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = logger(tmp.path());
        logger.handle_event(&module_result(false));
        logger.handle_event(&summary(false));
        logger.write_summary().unwrap();

        let raw = std::fs::read_to_string(logger.run_dir().join("summary.json")).unwrap();
        assert!(!raw.contains("skipped"), "{raw}");
    }

    #[test]
    fn each_host_log_records_its_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = RunLogger::new_in(tmp.path(), "deploy").unwrap();
        logger.handle_event(&ExecutorEvent::BatchStarted {
            index: 1,
            total: 3,
            hosts: vec!["web-2".to_string(), "web-3".to_string()],
        });
        for host in ["web-2", "web-3"] {
            let log = storage::read_node_log(logger.run_dir(), host).unwrap();
            assert!(log.contains("[BATCH] 2/3"), "{host}: {log}");
        }
    }

    /// An aborted host never connected, so nothing else would give it a summary entry.
    #[test]
    fn an_aborted_host_is_recorded_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let mut logger = RunLogger::new_in(tmp.path(), "deploy").unwrap();
        logger.handle_event(&ExecutorEvent::HostsAborted {
            hosts: vec!["web-9".to_string()],
            reason: "3 of 9 hosts failed, more than max-fail 2".to_string(),
        });
        logger.write_summary().unwrap();

        let node = &storage::read_summary(logger.run_dir()).unwrap().nodes["web-9"];
        assert_eq!(node.status, "aborted");
        assert_eq!(
            node.error.as_deref(),
            Some("3 of 9 hosts failed, more than max-fail 2")
        );
        let log = storage::read_node_log(logger.run_dir(), "web-9").unwrap();
        assert!(log.contains("[ABORTED] not started"), "{log}");
    }

    #[test]
    fn a_summary_written_before_when_existed_still_loads() {
        let old = r#"{"status":"ok","changed":2,"steps_completed":0}"#;
        let node: storage::NodeSummary = serde_json::from_str(old).unwrap();
        assert_eq!(node.skipped, 0);
    }
}
