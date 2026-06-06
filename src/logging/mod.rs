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
}

impl RunLogger {
    pub fn new(plan_name: &str) -> Result<Self, GlideshError> {
        let run_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let now = Utc::now();
        let dir_name = format!("{}_{}", now.format("%Y-%m-%dT%H-%M-%S"), plan_name);
        let run_dir = storage::runs_dir().join(&dir_name);
        fs::create_dir_all(&run_dir)?;

        Ok(RunLogger {
            run_dir,
            run_id,
            plan_name: plan_name.to_string(),
            started_at: now,
            node_files: HashMap::new(),
            node_summaries: HashMap::new(),
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
                stdout,
                stderr,
                exit_code,
            } => {
                let status = if *changed { "changed" } else { "ok" };
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
            } => {
                let status = if *success { "ok" } else { "failed" };
                self.log_line(
                    host,
                    &format!("[COMPLETE] status={} changed={}", status, changed),
                );
                if let Some(summary) = self.node_summaries.get_mut(host) {
                    summary.status = status.to_string();
                }
            }
            ExecutorEvent::RunComplete { .. } => {}
        }
    }

    pub fn write_summary(&self) -> Result<(), GlideshError> {
        let summary = RunSummaryFile {
            run_id: self.run_id.clone(),
            plan: self.plan_name.clone(),
            started_at: self.started_at,
            finished_at: Some(Utc::now()),
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
}
