#![allow(unused_assignments)]

use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
#[allow(dead_code, unused_assignments)]
pub enum GlideshError {
    #[error("SSH connection failed: {message}")]
    SshConnection { message: String },

    #[error("SSH authentication failed for {user}@{host}: {message}")]
    SshAuth {
        host: String,
        user: String,
        message: String,
    },

    #[error("SSH command failed (exit code {exit_code}): {stderr}")]
    SshCommand {
        exit_code: u32,
        stdout: String,
        stderr: String,
    },

    #[error("SSH channel error: {message}")]
    SshChannel { message: String },

    #[error("Key loading failed: {message}")]
    KeyLoad { message: String },

    #[error("Config parse error: {message}")]
    #[diagnostic(help("Check your KDL configuration syntax"))]
    ConfigParse { message: String },

    #[error("Template interpolation error: {message}")]
    TemplateError { message: String },

    #[error("Module error in {module}: {message}")]
    Module { module: String, message: String },

    #[error("OS detection failed: {message}")]
    OsDetection { message: String },

    #[error("Executor error: {message}")]
    Executor { message: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("No targets resolved for the given filter")]
    NoTargets,

    #[error("{0}")]
    Other(String),
}

impl From<russh::Error> for GlideshError {
    fn from(e: russh::Error) -> Self {
        GlideshError::SshConnection {
            message: e.to_string(),
        }
    }
}

impl From<russh_keys::Error> for GlideshError {
    fn from(e: russh_keys::Error) -> Self {
        GlideshError::KeyLoad {
            message: e.to_string(),
        }
    }
}
