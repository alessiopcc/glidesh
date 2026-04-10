use crate::config::template::{TemplateData, render};
use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

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

    fn resolve_src(src: &str, plan_base_dir: &std::path::Path) -> std::path::PathBuf {
        let path = std::path::Path::new(src);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            plan_base_dir.join(path)
        }
    }

    fn read_local_content(
        src: &str,
        template: bool,
        vars: &std::collections::HashMap<String, String>,
        template_data: &TemplateData,
        plan_base_dir: &std::path::Path,
    ) -> Result<Vec<u8>, GlideshError> {
        let resolved = Self::resolve_src(src, plan_base_dir);
        let content = std::fs::read(&resolved).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to read local file '{}': {}", resolved.display(), e),
        })?;

        if template {
            let text = String::from_utf8(content).map_err(|e| GlideshError::Module {
                module: "file".to_string(),
                message: format!("Template file '{}' is not valid UTF-8: {}", src, e),
            })?;
            let rendered = render(&text, vars, template_data)?;
            Ok(rendered.into_bytes())
        } else {
            Ok(content)
        }
    }

    fn is_recurse(params: &ModuleParams) -> bool {
        params
            .args
            .get("recurse")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hex_encode(hasher.finalize().as_slice())
    }

    /// Recursively walk a local directory, returning relative paths of all files (sorted).
    fn walk_dir(base: &Path) -> Result<Vec<PathBuf>, GlideshError> {
        let mut files = Vec::new();
        Self::walk_dir_inner(base, base, &mut files)?;
        files.sort();
        Ok(files)
    }

    fn walk_dir_inner(
        root: &Path,
        current: &Path,
        files: &mut Vec<PathBuf>,
    ) -> Result<(), GlideshError> {
        let entries = std::fs::read_dir(current).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to read directory '{}': {}", current.display(), e),
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "Failed to read directory entry in '{}': {}",
                    current.display(),
                    e
                ),
            })?;
            let path = entry.path();
            if path.is_dir() {
                Self::walk_dir_inner(root, &path, files)?;
            } else {
                let relative = path.strip_prefix(root).map_err(|e| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("Failed to compute relative path: {}", e),
                })?;
                files.push(relative.to_path_buf());
            }
        }
        Ok(())
    }
}

/// Strips leading zeros so "0644" and "644" compare equal.
/// Preserves "0" for zero-valued modes instead of returning an empty string.
fn normalize_mode(mode: &str) -> &str {
    let trimmed = mode.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
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
            if Self::is_recurse(params) {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: "fetch=true and recurse=true cannot be combined".to_string(),
                });
            }
            return Ok(ModuleStatus::Pending {
                plan: format!("Fetch {} -> {}", src, dest),
            });
        }

        if Self::is_recurse(params) {
            return self.check_recurse(ctx, params, src, dest).await;
        }

        let content = Self::read_local_content(
            src,
            Self::is_template(params),
            ctx.vars,
            ctx.template_data,
            ctx.plan_base_dir,
        )?;
        let local_hash = Self::sha256_hex(&content);

        match ctx.ssh.checksum_remote(dest).await? {
            Some(remote_hash) if remote_hash == local_hash => {
                let desired_owner = params.args.get("owner").and_then(|v| v.as_str());
                let desired_group = params.args.get("group").and_then(|v| v.as_str());
                let desired_mode = params.args.get("mode").and_then(|v| v.as_str());

                if desired_owner.is_some() || desired_group.is_some() || desired_mode.is_some() {
                    let remote_attrs = ctx.ssh.get_file_attrs(dest).await?;
                    if let Some((remote_owner, remote_group, remote_mode)) = remote_attrs {
                        let owner_ok = desired_owner.is_none_or(|o| o == remote_owner);
                        let group_ok = desired_group.is_none_or(|g| g == remote_group);
                        let mode_ok = desired_mode
                            .is_none_or(|m| normalize_mode(m) == normalize_mode(&remote_mode));

                        if owner_ok && group_ok && mode_ok {
                            return Ok(ModuleStatus::Satisfied);
                        }

                        let mut changes = Vec::new();
                        if !owner_ok {
                            changes.push(format!(
                                "owner: {} -> {}",
                                remote_owner,
                                desired_owner.unwrap()
                            ));
                        }
                        if !group_ok {
                            changes.push(format!(
                                "group: {} -> {}",
                                remote_group,
                                desired_group.unwrap()
                            ));
                        }
                        if !mode_ok {
                            changes.push(format!(
                                "mode: {} -> {}",
                                remote_mode,
                                desired_mode.unwrap()
                            ));
                        }
                        return Ok(ModuleStatus::Pending {
                            plan: format!("Fix attrs on {}: {}", dest, changes.join(", ")),
                        });
                    } else {
                        return Ok(ModuleStatus::Pending {
                            plan: format!("Set attrs on {} (could not read current attrs)", dest),
                        });
                    }
                }

                Ok(ModuleStatus::Satisfied)
            }
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

        if Self::is_recurse(params) {
            return self.apply_recurse(ctx, params, src, dest).await;
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

        let content = Self::read_local_content(
            src,
            Self::is_template(params),
            ctx.vars,
            ctx.template_data,
            ctx.plan_base_dir,
        )?;
        let local_hash = Self::sha256_hex(&content);

        let needs_upload = match ctx.ssh.checksum_remote(dest).await? {
            Some(remote_hash) => remote_hash != local_hash,
            None => true,
        };

        if needs_upload {
            if let Some(parent) = std::path::Path::new(dest).parent() {
                let parent_str = parent.to_string_lossy();
                if !parent_str.is_empty() {
                    ctx.ssh
                        .exec(&format!("mkdir -p '{}'", parent_str.replace('\'', "'\\''")))
                        .await?;
                }
            }

            ctx.ssh.upload_file(&content, dest).await?;
        }

        let owner = params.args.get("owner").and_then(|v| v.as_str());
        let group = params.args.get("group").and_then(|v| v.as_str());
        let mode = params.args.get("mode").and_then(|v| v.as_str());

        ctx.ssh.set_file_attrs(dest, owner, group, mode).await?;

        let output_msg = if needs_upload {
            format!("{} {} -> {} ({} bytes)", mode_str, src, dest, content.len())
        } else {
            format!("attrs {} (content unchanged)", dest)
        };

        Ok(ModuleResult {
            changed: true,
            output: output_msg,
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

    async fn check_recurse(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleStatus, GlideshError> {
        let resolved_src = Self::resolve_src(src, ctx.plan_base_dir);
        if !resolved_src.is_dir() {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "recurse=true but '{}' is not a directory",
                    resolved_src.display()
                ),
            });
        }

        let local_files = Self::walk_dir(&resolved_src)?;
        if local_files.is_empty() {
            return Ok(ModuleStatus::Satisfied);
        }

        let template = Self::is_template(params);
        let desired_owner = params.args.get("owner").and_then(|v| v.as_str());
        let desired_group = params.args.get("group").and_then(|v| v.as_str());
        let desired_mode = params.args.get("mode").and_then(|v| v.as_str());
        let check_attrs =
            desired_owner.is_some() || desired_group.is_some() || desired_mode.is_some();
        let mut content_changed = 0usize;
        let mut attrs_changed = 0usize;

        for rel_path in &local_files {
            let local_path = resolved_src.join(rel_path);
            let content = if template {
                let text =
                    std::fs::read_to_string(&local_path).map_err(|e| GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("Failed to read '{}': {}", local_path.display(), e),
                    })?;
                let rendered = render(&text, ctx.vars, ctx.template_data)?;
                rendered.into_bytes()
            } else {
                std::fs::read(&local_path).map_err(|e| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("Failed to read '{}': {}", local_path.display(), e),
                })?
            };

            let local_hash = Self::sha256_hex(&content);
            let remote_path = format!(
                "{}/{}",
                dest.trim_end_matches('/'),
                rel_path.to_string_lossy().replace('\\', "/")
            );

            match ctx.ssh.checksum_remote(&remote_path).await? {
                Some(remote_hash) if remote_hash == local_hash => {
                    if check_attrs {
                        if let Some((remote_owner, remote_group, remote_mode)) =
                            ctx.ssh.get_file_attrs(&remote_path).await?
                        {
                            let owner_ok = desired_owner.is_none_or(|o| o == remote_owner);
                            let group_ok = desired_group.is_none_or(|g| g == remote_group);
                            let mode_ok = desired_mode
                                .is_none_or(|m| normalize_mode(m) == normalize_mode(&remote_mode));
                            if !owner_ok || !group_ok || !mode_ok {
                                attrs_changed += 1;
                            }
                        } else {
                            attrs_changed += 1;
                        }
                    }
                }
                _ => content_changed += 1,
            }
        }

        if content_changed == 0 && attrs_changed == 0 {
            Ok(ModuleStatus::Satisfied)
        } else {
            let mut parts = Vec::new();
            if content_changed > 0 {
                parts.push(format!("{} content", content_changed));
            }
            if attrs_changed > 0 {
                parts.push(format!("{} attrs", attrs_changed));
            }
            Ok(ModuleStatus::Pending {
                plan: format!(
                    "Upload dir {} -> {} (changed: {} of {} files)",
                    src,
                    dest,
                    parts.join(", "),
                    local_files.len()
                ),
            })
        }
    }

    async fn apply_recurse(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let resolved_src = Self::resolve_src(src, ctx.plan_base_dir);
        if !resolved_src.is_dir() {
            return Err(GlideshError::Module {
                module: "file".to_string(),
                message: format!(
                    "recurse=true but '{}' is not a directory",
                    resolved_src.display()
                ),
            });
        }

        let local_files = Self::walk_dir(&resolved_src)?;

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!(
                    "[dry-run] Would copy dir {} -> {} ({} files)",
                    src,
                    dest,
                    local_files.len()
                ),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let template = Self::is_template(params);
        let dest_trimmed = dest.trim_end_matches('/');
        let mut uploaded = 0usize;

        let mut remote_dirs: Vec<String> = local_files
            .iter()
            .filter_map(|rel| {
                rel.parent().map(|p| {
                    let p_str = p.to_string_lossy().replace('\\', "/");
                    if p_str.is_empty() {
                        dest_trimmed.to_string()
                    } else {
                        format!("{}/{}", dest_trimmed, p_str)
                    }
                })
            })
            .collect();
        remote_dirs.sort();
        remote_dirs.dedup();

        if !remote_dirs.is_empty() {
            let dirs_arg = remote_dirs
                .iter()
                .map(|d| format!("'{}'", d.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ");
            ctx.ssh.exec(&format!("mkdir -p {}", dirs_arg)).await?;
        }

        for rel_path in &local_files {
            let local_path = resolved_src.join(rel_path);
            let content = if template {
                let text =
                    std::fs::read_to_string(&local_path).map_err(|e| GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("Failed to read '{}': {}", local_path.display(), e),
                    })?;
                let rendered = render(&text, ctx.vars, ctx.template_data)?;
                rendered.into_bytes()
            } else {
                std::fs::read(&local_path).map_err(|e| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("Failed to read '{}': {}", local_path.display(), e),
                })?
            };

            let local_hash = Self::sha256_hex(&content);
            let remote_path = format!(
                "{}/{}",
                dest_trimmed,
                rel_path.to_string_lossy().replace('\\', "/")
            );

            let needs_upload = match ctx.ssh.checksum_remote(&remote_path).await? {
                Some(remote_hash) => remote_hash != local_hash,
                None => true,
            };

            if needs_upload {
                ctx.ssh.upload_file(&content, &remote_path).await?;
                uploaded += 1;
            }
        }

        let owner = params.args.get("owner").and_then(|v| v.as_str());
        let group = params.args.get("group").and_then(|v| v.as_str());
        let mode = params.args.get("mode").and_then(|v| v.as_str());

        let attrs_changed = owner.is_some() || group.is_some() || mode.is_some();
        if attrs_changed {
            ctx.ssh
                .set_file_attrs_recursive(dest_trimmed, owner, group, mode)
                .await?;
        }

        Ok(ModuleResult {
            changed: uploaded > 0 || attrs_changed,
            output: format!(
                "copy dir {} -> {} ({} uploaded, {} total)",
                src,
                dest,
                uploaded,
                local_files.len()
            ),
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

    #[test]
    fn test_walk_dir_basic() {
        let dir = std::env::temp_dir().join(format!("glidesh_walk_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "world").unwrap();

        let files = FileModule::walk_dir(&dir).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], PathBuf::from("a.txt"));
        assert_eq!(
            files[1],
            PathBuf::from(if cfg!(windows) {
                "sub\\b.txt"
            } else {
                "sub/b.txt"
            })
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_walk_dir_empty() {
        let dir = std::env::temp_dir().join(format!("glidesh_walk_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let files = FileModule::walk_dir(&dir).unwrap();
        assert!(files.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_walk_dir_nonexistent() {
        let dir = std::env::temp_dir().join("glidesh_walk_nonexistent_dir");
        assert!(FileModule::walk_dir(&dir).is_err());
    }

    #[test]
    fn test_is_recurse_default_false() {
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args: std::collections::HashMap::new(),
        };
        assert!(!FileModule::is_recurse(&params));
    }

    #[test]
    fn test_is_recurse_true() {
        use crate::config::types::ParamValue;
        let mut args = std::collections::HashMap::new();
        args.insert("recurse".to_string(), ParamValue::Bool(true));
        let params = ModuleParams {
            resource_name: "/tmp/test".to_string(),
            args,
        };
        assert!(FileModule::is_recurse(&params));
    }
}
