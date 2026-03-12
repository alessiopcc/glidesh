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
    pub nodes: HashMap<String, NodeSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub status: String,
    pub changed: usize,
    pub steps_completed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
