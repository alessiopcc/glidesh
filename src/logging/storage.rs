use chrono::{DateTime, Utc};
use glidesh::error::GlideshError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummaryFile {
    pub run_id: String,
    pub plan: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Absent from older run logs, which were all real runs.
    #[serde(default)]
    pub dry_run: bool,
    pub nodes: HashMap<String, NodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub status: String,
    pub changed: usize,
    /// Absent from run logs written before `when=` existed, and omitted when zero so a plan
    /// without `when=` writes the same summary it always has.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped: usize,
    pub steps_completed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunSummaryFile {
    /// `"N nodes: X ok, Y failed"`, plus the hosts a stopped rollout never reached. Shared by
    /// `glidesh logs` and the logs explorer's run list.
    pub fn node_counts(&self) -> String {
        let count = |status: &str| self.nodes.values().filter(|n| n.status == status).count();
        let aborted = count("aborted");
        format!(
            "{} nodes: {} ok, {} failed{}",
            self.nodes.len(),
            count("ok"),
            count("failed"),
            if aborted > 0 {
                format!(", {aborted} aborted")
            } else {
                String::new()
            }
        )
    }
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub fn glidesh_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".glidesh")
}

pub fn runs_dir() -> PathBuf {
    glidesh_dir().join("runs")
}

pub fn list_runs() -> Result<Vec<PathBuf>, GlideshError> {
    let dir = runs_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries.reverse(); // newest first
    Ok(entries)
}

pub fn read_summary(run_dir: &Path) -> Result<RunSummaryFile, GlideshError> {
    let path = run_dir.join("summary.json");
    let content = fs::read_to_string(&path)?;
    let summary: RunSummaryFile = serde_json::from_str(&content)
        .map_err(|e| GlideshError::Other(format!("Failed to parse summary.json: {}", e)))?;
    Ok(summary)
}

pub fn read_node_log(run_dir: &Path, node: &str) -> Result<String, GlideshError> {
    let path = run_dir.join(format!("{}.log", node));
    let content = fs::read_to_string(&path)?;
    Ok(content)
}

pub fn delete_run(run_dir: &Path) -> Result<(), GlideshError> {
    fs::remove_dir_all(run_dir)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::RunSummaryFile;

    /// Summaries written before this field existed must still load.
    #[test]
    fn summary_without_dry_run_key_still_loads() {
        let json = r#"{
            "run_id": "abc123",
            "plan": "deploy",
            "started_at": "2026-09-11T10:00:00Z",
            "finished_at": null,
            "nodes": {}
        }"#;
        let summary: RunSummaryFile = serde_json::from_str(json).unwrap();
        assert_eq!(summary.run_id, "abc123");
        assert!(!summary.dry_run);
    }

    fn with_statuses(statuses: &[&str]) -> RunSummaryFile {
        let nodes = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| {
                (
                    format!("h{i}"),
                    serde_json::from_str(&format!(
                        r#"{{"status":"{s}","changed":0,"steps_completed":0}}"#
                    ))
                    .unwrap(),
                )
            })
            .collect();
        RunSummaryFile {
            run_id: "r".into(),
            plan: "p".into(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            dry_run: false,
            nodes,
        }
    }

    #[test]
    fn node_counts_mention_aborted_hosts_only_when_there_are_some() {
        assert_eq!(
            with_statuses(&["ok", "ok", "failed"]).node_counts(),
            "3 nodes: 2 ok, 1 failed"
        );
        assert_eq!(
            with_statuses(&["ok", "failed", "aborted", "aborted"]).node_counts(),
            "4 nodes: 1 ok, 1 failed, 2 aborted"
        );
    }
}
