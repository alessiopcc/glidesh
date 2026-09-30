use crate::config::template::{TemplateData, defined_references, render_file};
use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::file_diff;
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

    /// An empty destination would name the host's `/` once a path is joined to it.
    fn get_dest(params: &ModuleParams) -> Result<&str, GlideshError> {
        match params.resource_name.as_str() {
            "" => Err(GlideshError::Module {
                module: "file".to_string(),
                message: "the destination path is empty".to_string(),
            }),
            dest => Ok(dest),
        }
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

    /// A path on this machine — an upload's `src`, a fetch's destination — resolved from
    /// the plan's directory when relative, so a run does the same from any directory.
    fn resolve_local(path: &str, plan_base_dir: &std::path::Path) -> std::path::PathBuf {
        let path = std::path::Path::new(path);
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
        let resolved = Self::resolve_local(src, plan_base_dir);
        let content = std::fs::read(&resolved).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to read local file '{}': {}", resolved.display(), e),
        })?;

        if template {
            let text = String::from_utf8(content).map_err(|e| GlideshError::Module {
                module: "file".to_string(),
                message: format!("Template file '{}' is not valid UTF-8: {}", src, e),
            })?;
            let rendered = render_file(&text, vars, template_data, src)?;
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

    /// A file of a recursive copy as the plan names it, `src/<path>`, as `validate` does.
    fn shown_template(src: &str, rel_path: &Path) -> String {
        format!(
            "{}/{}",
            src.trim_end_matches('/'),
            rel_path.to_string_lossy().replace('\\', "/")
        )
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

/// Every parameter the module reads; any other is rejected before connecting.
const PARAMS: &[&str] = &[
    "diff", "fetch", "group", "mode", "owner", "recurse", "src", "template",
];

#[async_trait]
impl Module for FileModule {
    fn name(&self) -> &str {
        "file"
    }

    fn params(&self) -> Option<&'static [&'static str]> {
        Some(PARAMS)
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let src = Self::get_src(params)?;
        let dest = Self::get_dest(params)?;

        if Self::is_fetch(params) {
            if Self::is_recurse(params) {
                return Err(GlideshError::Module {
                    module: "file".to_string(),
                    message: "fetch=true and recurse=true cannot be combined".to_string(),
                });
            }
            return Ok(ModuleStatus::pending(format!(
                "Fetch {} -> {}",
                src,
                Self::resolve_local(dest, ctx.plan_base_dir).display()
            )));
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

        match ctx.checksum_remote(dest).await? {
            Some(remote_hash) if remote_hash == local_hash => {
                let desired_owner = params.args.get("owner").and_then(|v| v.as_str());
                let desired_group = params.args.get("group").and_then(|v| v.as_str());
                let desired_mode = params.args.get("mode").and_then(|v| v.as_str());

                if desired_owner.is_some() || desired_group.is_some() || desired_mode.is_some() {
                    let remote_attrs = ctx.get_file_attrs(dest).await?;
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
                        return Ok(ModuleStatus::pending(format!(
                            "Fix attrs on {}: {}",
                            dest,
                            changes.join(", ")
                        )));
                    } else {
                        return Ok(ModuleStatus::pending(format!(
                            "Set attrs on {} (could not read current attrs)",
                            dest
                        )));
                    }
                }

                Ok(ModuleStatus::Satisfied)
            }
            remote_hash => {
                let plan = format!("Upload {} -> {}", src, dest);
                if !ctx.diff {
                    return Ok(ModuleStatus::pending(plan));
                }
                if Self::diff_opted_out(params)? {
                    return Ok(ModuleStatus::pending_with_diff(
                        plan,
                        file_diff::opted_out(dest),
                    ));
                }
                let diff =
                    Self::diff_against_remote(ctx, params, dest, remote_hash.is_some(), &content)
                        .await?;
                Ok(ModuleStatus::pending_with_diff(plan, diff))
            }
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let src = Self::get_src(params)?;
        let dest = Self::get_dest(params)?;

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
    /// A warning when an upload without `template #true` contains `${name}` references to
    /// variables this host defines — they would ship verbatim, which is almost never meant.
    fn literal_reference_warning(
        ctx: &ModuleContext<'_>,
        label: &str,
        content: &[u8],
    ) -> Option<String> {
        let names = defined_references(content, |n| {
            ctx.vars.contains_key(n) || ctx.template_data.extra_vars.contains_key(n)
        });
        if names.is_empty() {
            return None;
        }
        let refs: Vec<String> = names.iter().map(|n| format!("${{{n}}}")).collect();
        Some(format!(
            "warning: {label} contains {} but is uploaded as-is, because `template` is not set; \
             add `template #true` to substitute",
            refs.join(", ")
        ))
    }

    async fn apply_upload(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let template = Self::is_template(params);
        let mode_str = if template { "template" } else { "copy" };

        let content = Self::read_local_content(
            src,
            template,
            ctx.vars,
            ctx.template_data,
            ctx.plan_base_dir,
        )?;
        // Before the dry-run return: a preview is the best moment to catch it.
        let warning = if template {
            String::new()
        } else {
            Self::literal_reference_warning(ctx, src, &content).unwrap_or_default()
        };

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would {} {} -> {}", mode_str, src, dest),
                stderr: warning,
                exit_code: 0,
                output_cut: false,
            });
        }

        let local_hash = Self::sha256_hex(&content);

        let needs_upload = match ctx.checksum_remote(dest).await? {
            Some(remote_hash) => remote_hash != local_hash,
            None => true,
        };

        if needs_upload {
            if let Some(parent) = std::path::Path::new(dest).parent() {
                let parent_str = parent.to_string_lossy();
                if !parent_str.is_empty() {
                    ctx.create_dirs(&[&parent_str]).await?;
                }
            }

            ctx.upload_file(&content, dest).await?;
        }

        let owner = params.args.get("owner").and_then(|v| v.as_str());
        let group = params.args.get("group").and_then(|v| v.as_str());
        let mode = params.args.get("mode").and_then(|v| v.as_str());

        ctx.set_file_attrs(dest, owner, group, mode).await?;

        let output_msg = if needs_upload {
            format!("{} {} -> {} ({} bytes)", mode_str, src, dest, content.len())
        } else {
            format!("attrs {} (content unchanged)", dest)
        };

        Ok(ModuleResult {
            changed: true,
            output: output_msg,
            stderr: warning,
            exit_code: 0,
            output_cut: false,
        })
    }

    async fn apply_fetch(
        &self,
        ctx: &ModuleContext<'_>,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let dest = Self::resolve_local(dest, ctx.plan_base_dir);
        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would fetch {} -> {}", src, dest.display()),
                stderr: String::new(),
                exit_code: 0,
                output_cut: false,
            });
        }

        let data = ctx.download_file(src).await?;

        if let Some(parent) = dest.parent() {
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

        std::fs::write(&dest, &data).map_err(|e| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to write local file '{}': {}", dest.display(), e),
        })?;

        Ok(ModuleResult {
            changed: true,
            output: format!("fetch {} -> {} ({} bytes)", src, dest.display(), data.len()),
            stderr: String::new(),
            exit_code: 0,
            output_cut: false,
        })
    }

    async fn check_recurse(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleStatus, GlideshError> {
        let resolved_src = Self::resolve_local(src, ctx.plan_base_dir);
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
        let mut diffs = Vec::new();
        let opted_out = Self::diff_opted_out(params)?;
        let show_diffs = ctx.diff && !opted_out;

        for rel_path in &local_files {
            let local_path = resolved_src.join(rel_path);
            let content = if template {
                let text =
                    std::fs::read_to_string(&local_path).map_err(|e| GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("Failed to read '{}': {}", local_path.display(), e),
                    })?;
                let rendered = render_file(
                    &text,
                    ctx.vars,
                    ctx.template_data,
                    &Self::shown_template(src, rel_path),
                )?;
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

            match ctx.checksum_remote(&remote_path).await? {
                Some(remote_hash) if remote_hash == local_hash => {
                    if check_attrs {
                        if let Some((remote_owner, remote_group, remote_mode)) =
                            ctx.get_file_attrs(&remote_path).await?
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
                remote_hash => {
                    content_changed += 1;
                    if show_diffs {
                        diffs.push(
                            Self::diff_against_remote(
                                ctx,
                                params,
                                &remote_path,
                                remote_hash.is_some(),
                                &content,
                            )
                            .await?,
                        );
                    }
                }
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
            let plan = format!(
                "Upload dir {} -> {} (changed: {} of {} files)",
                src,
                dest,
                parts.join(", "),
                local_files.len()
            );
            if ctx.diff && opted_out && content_changed > 0 {
                Ok(ModuleStatus::pending_with_diff(
                    plan,
                    file_diff::opted_out(dest),
                ))
            } else if diffs.is_empty() {
                Ok(ModuleStatus::pending(plan))
            } else {
                let diff = file_diff::truncate_lines(&diffs.join("\n"), file_diff::MAX_DIFF_LINES);
                Ok(ModuleStatus::pending_with_diff(plan, diff))
            }
        }
    }

    /// `diff=#false` keeps a task out of `--diff`, for content glidesh cannot tell is
    /// sensitive — such as a world-readable file still holding a secret the plan dropped.
    fn diff_opted_out(params: &ModuleParams) -> Result<bool, GlideshError> {
        match params.args.get("diff") {
            None => Ok(false),
            Some(value) => value
                .as_bool()
                .map(|show| !show)
                .ok_or_else(|| GlideshError::Module {
                    module: "file".to_string(),
                    message: "'diff' must be #true or #false".to_string(),
                }),
        }
    }

    /// Only called once the hashes differ, so the download is spent on a real change.
    async fn diff_against_remote(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        dest: &str,
        exists: bool,
        content: &[u8],
    ) -> Result<String, GlideshError> {
        let private =
            file_diff::mode_may_be_private(params.args.get("mode").and_then(|v| v.as_str()));
        if let Some(note) = file_diff::local_note(dest, content, private) {
            return Ok(note);
        }
        let remote = if exists {
            file_diff::fetch_remote(ctx, dest).await?
        } else {
            file_diff::Remote::Missing
        };
        Ok(file_diff::content_diff(
            dest,
            &remote,
            content,
            private,
            ctx.secrets.as_deref(),
        ))
    }

    async fn apply_recurse(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let resolved_src = Self::resolve_local(src, ctx.plan_base_dir);
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
        let template = Self::is_template(params);

        let mut warnings = Vec::new();
        if !template {
            for rel_path in &local_files {
                let local_path = resolved_src.join(rel_path);
                let content = std::fs::read(&local_path).map_err(|e| GlideshError::Module {
                    module: "file".to_string(),
                    message: format!("Failed to read '{}': {}", local_path.display(), e),
                })?;
                let label = format!(
                    "{}/{}",
                    src.trim_end_matches('/'),
                    rel_path.to_string_lossy().replace('\\', "/")
                );
                warnings.extend(Self::literal_reference_warning(ctx, &label, &content));
            }
        }
        let warnings = warnings.join("\n");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!(
                    "[dry-run] Would copy dir {} -> {} ({} files)",
                    src,
                    dest,
                    local_files.len()
                ),
                stderr: warnings,
                exit_code: 0,
                output_cut: false,
            });
        }

        let dest_trimmed = dest.trim_end_matches('/');
        let owner = params.args.get("owner").and_then(|v| v.as_str());
        let group = params.args.get("group").and_then(|v| v.as_str());
        let mode = params.args.get("mode").and_then(|v| v.as_str());
        let attrs_changed = owner.is_some() || group.is_some() || mode.is_some();
        // Refused before anything is uploaded, rather than by the attribute change after;
        // asked of the host, since `/tmp/..` or a symlink can name `/` too.
        if attrs_changed && ctx.is_root_dir(dest_trimmed).await? {
            return Err(crate::ssh::connection::root_refusal(dest));
        }
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

        // A copy to `/` names its top level as "", which `mkdir -p` rejects; `/` exists.
        let dirs: Vec<&str> = remote_dirs
            .iter()
            .map(String::as_str)
            .filter(|d| !d.is_empty())
            .collect();
        ctx.create_dirs(&dirs).await?;

        for rel_path in &local_files {
            let local_path = resolved_src.join(rel_path);
            let content = if template {
                let text =
                    std::fs::read_to_string(&local_path).map_err(|e| GlideshError::Module {
                        module: "file".to_string(),
                        message: format!("Failed to read '{}': {}", local_path.display(), e),
                    })?;
                let rendered = render_file(
                    &text,
                    ctx.vars,
                    ctx.template_data,
                    &Self::shown_template(src, rel_path),
                )?;
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

            let needs_upload = match ctx.checksum_remote(&remote_path).await? {
                Some(remote_hash) => remote_hash != local_hash,
                None => true,
            };

            if needs_upload {
                ctx.upload_file(&content, &remote_path).await?;
                uploaded += 1;
            }
        }

        if attrs_changed {
            ctx.set_file_attrs_recursive(dest_trimmed, owner, group, mode)
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
            stderr: warnings,
            exit_code: 0,
            output_cut: false,
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
    fn a_relative_local_path_resolves_from_the_plans_directory() {
        let plan_dir = std::env::temp_dir().join("plans");
        assert_eq!(
            FileModule::resolve_local("backups/db.sql", &plan_dir),
            plan_dir.join("backups/db.sql")
        );
        assert_eq!(
            FileModule::resolve_local("../out/db.sql", &plan_dir),
            plan_dir.join("../out/db.sql")
        );
    }

    #[test]
    fn an_absolute_local_path_is_used_as_given() {
        let absolute = std::env::temp_dir().join("backups").join("db.sql");
        let absolute = absolute.to_str().unwrap();
        assert_eq!(
            FileModule::resolve_local(absolute, std::path::Path::new("/plans")),
            std::path::PathBuf::from(absolute)
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
    fn an_empty_destination_is_rejected() {
        let params = |dest: &str| ModuleParams {
            resource_name: dest.to_string(),
            args: std::collections::HashMap::new(),
        };
        assert!(FileModule::get_dest(&params("")).is_err());
        assert_eq!(FileModule::get_dest(&params("/")).unwrap(), "/");
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

    #[test]
    fn diff_false_opts_a_task_out_and_anything_else_is_rejected() {
        use crate::config::types::ParamValue;
        let with = |value: Option<ParamValue>| ModuleParams {
            resource_name: "/etc/app".to_string(),
            args: value.into_iter().map(|v| ("diff".to_string(), v)).collect(),
        };
        assert!(!FileModule::diff_opted_out(&with(None)).unwrap());
        assert!(!FileModule::diff_opted_out(&with(Some(ParamValue::Bool(true)))).unwrap());
        assert!(FileModule::diff_opted_out(&with(Some(ParamValue::Bool(false)))).unwrap());
        let err = FileModule::diff_opted_out(&with(Some(ParamValue::String("no".into()))))
            .unwrap_err()
            .to_string();
        assert!(err.contains("'diff' must be #true or #false"), "{err}");
    }
}
