use crate::config::template::TemplateData;
use crate::config::types::ResolvedRunAs;
use crate::error::GlideshError;
use crate::modules::detect::OsInfo;
use crate::ssh::SshSession;
use crate::ssh::connection::CommandOutput;
use std::collections::HashMap;
use std::path::Path;

pub struct ModuleContext<'a> {
    pub ssh: &'a SshSession,
    pub os_info: &'a OsInfo,
    pub vars: &'a HashMap<String, String>,
    pub template_data: &'a TemplateData,
    pub dry_run: bool,
    pub plan_base_dir: &'a Path,
    /// Effective privilege escalation for this task, or `None` to run as the login
    /// user. Resolved per task by the executor from module/step/host/group/global/CLI.
    pub run_as: Option<ResolvedRunAs>,
}

/// Escalation-aware wrappers. Modules call these instead of `ctx.ssh.*` so that the
/// configured `run-as` is applied uniformly to every remote operation (both `check`
/// and `apply`). Each forwards the task's `run_as` to the SSH layer.
impl ModuleContext<'_> {
    pub async fn exec(&self, command: &str) -> Result<CommandOutput, GlideshError> {
        self.ssh.exec_as(command, self.run_as.as_ref()).await
    }

    pub async fn upload_file(&self, content: &[u8], remote_path: &str) -> Result<(), GlideshError> {
        self.ssh
            .upload_file_as(content, remote_path, self.run_as.as_ref())
            .await
    }

    pub async fn download_file(&self, remote_path: &str) -> Result<Vec<u8>, GlideshError> {
        self.ssh
            .download_file_as(remote_path, self.run_as.as_ref())
            .await
    }

    pub async fn checksum_remote(&self, remote_path: &str) -> Result<Option<String>, GlideshError> {
        self.ssh
            .checksum_remote(remote_path, self.run_as.as_ref())
            .await
    }

    pub async fn get_file_attrs(
        &self,
        path: &str,
    ) -> Result<Option<(String, String, String)>, GlideshError> {
        self.ssh.get_file_attrs(path, self.run_as.as_ref()).await
    }

    pub async fn set_file_attrs(
        &self,
        path: &str,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> Result<(), GlideshError> {
        self.ssh
            .set_file_attrs(path, owner, group, mode, self.run_as.as_ref())
            .await
    }

    pub async fn set_file_attrs_recursive(
        &self,
        path: &str,
        owner: Option<&str>,
        group: Option<&str>,
        mode: Option<&str>,
    ) -> Result<(), GlideshError> {
        self.ssh
            .set_file_attrs_recursive(path, owner, group, mode, self.run_as.as_ref())
            .await
    }
}
