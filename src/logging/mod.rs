pub mod storage;

use crate::executor::result::ExecutorEvent;
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
            ExecutorEvent::NodeConnecting { host } => {
                self.log_line(host, "[CONNECTING]");
                self.node_summaries.insert(
                    host.clone(),
                    NodeSummary {
                        status: "connecting".to_string(),
                        changed: 0,
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
            ExecutorEvent::NodeComplete {
                host,
                success,
                changed,
                dry_run,
            } => {
                let status = if *success { "ok" } else { "failed" };
                let counted = if *dry_run { "would_change" } else { "changed" };
                self.log_line(
                    host,
                    &format!("[COMPLETE] status={} {}={}", status, counted, changed),
                );
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.status = status.to_string();
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
                dry_run,
            });

            let log = storage::read_node_log(logger.run_dir(), "web-1").unwrap();
            assert!(log.contains(expected), "expected {expected} in: {log}");
            assert!(!log.contains(absent), "unexpected {absent} in: {log}");
        }
    }
}
