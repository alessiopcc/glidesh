use crate::config::types::ParamValue;
use crate::modules::detect::OsInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Current protocol version. Plugins must report this version to be loaded.
pub const PROTOCOL_VERSION: u32 = 1;

// --- Requests from glidesh to the plugin ---

#[derive(Serialize)]
pub struct DescribeRequest {
    pub method: &'static str,
}

impl Default for DescribeRequest {
    fn default() -> Self {
        Self { method: "describe" }
    }
}

impl DescribeRequest {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Serialize)]
pub struct ModuleRequest<'a> {
    pub method: &'a str,
    pub resource_name: &'a str,
    pub args: &'a HashMap<String, ParamValue>,
    pub os_info: &'a OsInfo,
    pub vars: &'a HashMap<String, String>,
    pub dry_run: bool,
}

#[derive(Serialize)]
pub struct ShutdownRequest {
    pub method: &'static str,
}

impl Default for ShutdownRequest {
    fn default() -> Self {
        Self { method: "shutdown" }
    }
}

impl ShutdownRequest {
    pub fn new() -> Self {
        Self::default()
    }
}

// --- Responses from plugin to glidesh ---

#[derive(Deserialize)]
pub struct DescribeResponse {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
}

/// A line from the plugin's stdout. It is either:
/// - An SSH operation request (plugin wants glidesh to run something)
/// - A check/apply response (plugin is done with the current method)
/// - An error
#[derive(Deserialize)]
#[serde(untagged)]
pub enum PluginMessage {
    SshRequest(SshRequest),
    CheckResponse(CheckResponse),
    ApplyResponse(ApplyResponse),
    Error(ErrorResponse),
}

#[derive(Deserialize)]
#[serde(tag = "status")]
pub enum CheckResponse {
    #[serde(rename = "satisfied")]
    Satisfied,
    #[serde(rename = "pending")]
    Pending { plan: String },
    #[serde(rename = "unknown")]
    Unknown { reason: String },
}

#[derive(Deserialize)]
pub struct ApplyResponse {
    pub changed: bool,
    pub output: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

// --- SSH proxy: plugin requests an SSH op, glidesh executes and responds ---

#[derive(Deserialize)]
#[serde(tag = "ssh", rename_all = "snake_case")]
pub enum SshRequest {
    Exec {
        command: String,
    },
    Upload {
        path: String,
        content_base64: String,
    },
    Download {
        path: String,
    },
    Checksum {
        path: String,
    },
    SetAttrs {
        path: String,
        owner: Option<String>,
        group: Option<String>,
        mode: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(tag = "ssh_result", rename_all = "snake_case")]
pub enum SshResponse {
    Exec {
        exit_code: u32,
        stdout: String,
        stderr: String,
    },
    Upload {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Download {
        content_base64: String,
        exists: bool,
    },
    Checksum {
        hash: String,
        exists: bool,
    },
    SetAttrs {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}
