use crate::config::template::interpolate;
use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

pub struct FileModule;

impl FileModule {
    fn is_fetch(params: &ModuleParams) -> bool {
        params
            .args
            .get("fetch")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn is_template(params: &ModuleParams) -> bool {
        params
            .args
            .get("template")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn get_src(params: &ModuleParams) -> Result<&str, GlideshError> {
        params
            .args
            .get("src")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "file".to_string(),
                message: "Missing required parameter: src".to_string(),
            })
    }

    fn read_local_content(
        src: &str,
        template: bool,
        vars: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<u8>, GlideshError> {
        let content = std::fs::read(src).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to read local file '{}': {}", src, e),
        })?;

        if template {
            let text = String::from_utf8(content).map_err(|e| GlideshError::Module {
                module: "file".to_string(),
                message: format!("Template file '{}' is not valid UTF-8: {}", src, e),
            })?;
            let rendered = interpolate(&text, vars)?;
            Ok(rendered.into_bytes())
        } else {
            Ok(content)
        }
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        format!("{:x}", hasher.finalize())
    }
}

#[async_trait]
impl Module for FileModule {
    fn name(&self) -> &str {
        "file"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let src = Self::get_src(params)?;
        let dest = &params.resource_name;

        if Self::is_fetch(params) {
            return Ok(ModuleStatus::Pending {
                plan: format!("Fetch {} -> {}", src, dest),
            });
        }

        let content = Self::read_local_content(src, Self::is_template(params), ctx.vars)?;
        let local_hash = Self::sha256_hex(&content);

        match ctx.ssh.checksum_remote(dest).await? {
            Some(remote_hash) if remote_hash == local_hash => Ok(ModuleStatus::Satisfied),
            _ => Ok(ModuleStatus::Pending {
                plan: format!("Upload {} -> {}", src, dest),
            }),
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let src = Self::get_src(params)?;
        let dest = &params.resource_name;

        if Self::is_fetch(params) {
            return self.apply_fetch(ctx, src, dest).await;
        }

        self.apply_upload(ctx, params, src, dest).await
    }
}

impl FileModule {
    async fn apply_upload(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let mode_str = if Self::is_template(params) {
            "template"
        } else {
            "copy"
        };

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} {} -> {}", mode_str, src, dest),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let content = Self::read_local_content(src, Self::is_template(params), ctx.vars)?;

        if let Some(parent) = std::path::Path::new(dest).parent() {
            let parent_str = parent.to_string_lossy();
            if !parent_str.is_empty() {
                ctx.ssh
                    .exec(&format!("mkdir -p '{}'", parent_str.replace('\'', "'\\''")))
                    .await?;
            }
        }

        ctx.ssh.upload_file(&content, dest).await?;

        let owner = params.args.get("owner").and_then(|v| v.as_str());
        let group = params.args.get("group").and_then(|v| v.as_str());
        let mode = params.args.get("mode").and_then(|v| v.as_str());

        ctx.ssh.set_file_attrs(dest, owner, group, mode).await?;

        Ok(ModuleResult {
            changed: true,
            output: format!("{} {} -> {} ({} bytes)", mode_str, src, dest, content.len()),
            stderr: String::new(),
            exit_code: 0,
        })
    }

    async fn apply_fetch(
        &self,
        ctx: &ModuleContext<'_>,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would fetch {} -> {}", src, dest),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let data = ctx.ssh.download_file(src).await?;

        if let Some(parent) = std::path::Path::new(dest).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!(
                        "Failed to create local directory '{}': {}",
                        parent.display(),
                        e
                    ),
                })?;
            }
        }

        std::fs::write(dest, &data).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to write local file '{}': {}", dest, e),
        })?;

        Ok(ModuleResult {
            changed: true,
            output: format!("fetch {} -> {} ({} bytes)", src, dest, data.len()),
            stderr: String::new(),
            exit_code: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex() {
        let hash = FileModule::sha256_hex(b"hello world\n");
        assert_eq!(
            hash,
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
    }

    #[test]
    fn test_sha256_empty() {
        let hash = FileModule::sha256_hex(b"");
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_is_fetch_default_false() {
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args: std::collections::HashMap::new(),
        };
        assert!(!FileModule::is_fetch(&params));
    }

    #[test]
    fn test_is_template_default_false() {
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args: std::collections::HashMap::new(),
        };
        assert!(!FileModule::is_template(&params));
    }

    #[test]
    fn test_get_src_missing() {
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args: std::collections::HashMap::new(),
        };
        assert!(FileModule::get_src(&params).is_err());
    }

    #[test]
    fn test_get_src_present() {
        use crate::config::types::ParamValue;
        let mut args = std::collections::HashMap::new();
        args.insert(
            "src".to_string(),
            ParamValue::String("files/test.conf".to_string()),
        );
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args,
        };
        assert_eq!(FileModule::get_src(&params).unwrap(), "files/test.conf");
    }
}
