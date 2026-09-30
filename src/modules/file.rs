use crate::config::template::{TemplateData, defined_references, render_file};
use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::file_diff;
use crate::modules::file_tree::{self, SourceTree, Strays};
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
}

/// Strips leading zeros so "0644" and "644" compare equal.
/// Preserves "0" for zero-valued modes instead of returning an empty string.
fn normalize_mode(mode: &str) -> &str {
    let trimmed = mode.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

/// Every parameter the module reads; any other is rejected before connecting.
const PARAMS: &[&str] = &[
    "diff",
    "dir-mode",
    "exclude",
    "fetch",
    "file-mode",
    "group",
    "mode",
    "owner",
    "prune",
    "recurse",
    "src",
    "template",
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
        // Rejects a recursive-only parameter on any other upload, before anything is read.
        Self::tree_options(params, dest)?;

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

    /// A recursive upload's options, or why they cannot be used.
    fn tree_options(params: &ModuleParams, dest: &str) -> Result<file_tree::Options, GlideshError> {
        file_tree::options(&params.args, Some(dest), Self::is_recurse(params)).map_err(|message| {
            GlideshError::Module {
                module: "file".to_string(),
                message,
            }
        })
    }

    /// The source directory of a recursive upload, and its tree less what `exclude` leaves
    /// out.
    fn source_tree(
        ctx: &ModuleContext<'_>,
        src: &str,
        options: &file_tree::Options,
    ) -> Result<(PathBuf, SourceTree), GlideshError> {
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
        let tree =
            file_tree::walk(&resolved_src, &options.exclude).map_err(|e| GlideshError::Module {
                module: "file".to_string(),
                message: format!("Failed to read directory {e}"),
            })?;
        Ok((resolved_src, tree))
    }

    /// One file of a recursive upload as it is written to the host: rendered when it is a
    /// template.
    fn tree_file_content(
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        resolved_src: &Path,
        rel_path: &str,
    ) -> Result<Vec<u8>, GlideshError> {
        let local_path = resolved_src.join(rel_path);
        let failed = |e: std::io::Error| GlideshError::Module {
            module: "file".to_string(),
            message: format!("Failed to read '{}': {}", local_path.display(), e),
        };
        if !Self::is_template(params) {
            return std::fs::read(&local_path).map_err(failed);
        }
        let text = std::fs::read_to_string(&local_path).map_err(failed)?;
        let rendered = render_file(
            &text,
            ctx.vars,
            ctx.template_data,
            &Self::shown_template(src, Path::new(rel_path)),
        )?;
        Ok(rendered.into_bytes())
    }

    /// The paths a recursive upload manages on the host: the destination and the source's
    /// directories under it, and its files. The destination is named `dest/`, so a symlink
    /// to a directory stands for that directory, as uploads through it do: its attributes
    /// are changed without following links.
    fn managed_paths(dest: &str, tree: &SourceTree) -> (Vec<String>, Vec<String>) {
        let root = format!("{}/", dest.trim_end_matches('/'));
        let dirs = std::iter::once(root)
            .chain(tree.dirs.iter().map(|d| file_tree::join(dest, d)))
            .collect();
        let files = tree
            .files
            .iter()
            .map(|f| file_tree::join(dest, f))
            .collect();
        (files, dirs)
    }

    /// Whether a host path's attributes are what `options` asks for.
    fn attrs_match(
        options: &file_tree::Options,
        is_dir: bool,
        (owner, group, mode): &(String, String, String),
    ) -> bool {
        let wanted_mode = if is_dir {
            options.dir_mode.as_deref()
        } else {
            options.file_mode.as_deref()
        };
        options.owner.as_deref().is_none_or(|o| o == owner)
            && options.group.as_deref().is_none_or(|g| g == group)
            && wanted_mode.is_none_or(|m| normalize_mode(m) == normalize_mode(mode))
    }

    /// `paths`, the first few named and the rest counted.
    fn some_paths(paths: &[String]) -> String {
        const SHOWN: usize = 5;
        let mut shown = paths
            .iter()
            .take(SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if paths.len() > SHOWN {
            shown.push_str(&format!(" and {} more", paths.len() - SHOWN));
        }
        shown
    }

    /// What `prune` would remove from `dest`; nothing without it.
    async fn strays(
        ctx: &ModuleContext<'_>,
        options: &file_tree::Options,
        tree: &SourceTree,
        src: &str,
        dest: &str,
    ) -> Result<Strays, GlideshError> {
        if !options.prune {
            return Ok(Strays::default());
        }
        file_tree::check_prune_source(tree, src, dest).map_err(|message| GlideshError::Module {
            module: "file".to_string(),
            message,
        })?;
        let listed = ctx.list_tree(dest).await?;
        Ok(file_tree::strays(&listed, tree, &options.exclude))
    }

    async fn check_recurse(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
        src: &str,
        dest: &str,
    ) -> Result<ModuleStatus, GlideshError> {
        let options = Self::tree_options(params, dest)?;
        let (resolved_src, tree) = Self::source_tree(ctx, src, &options)?;

        let mut content_changed = 0usize;
        let mut diffs = Vec::new();
        let opted_out = Self::diff_opted_out(params)?;
        let show_diffs = ctx.diff && !opted_out;

        for rel_path in &tree.files {
            let content = Self::tree_file_content(ctx, params, src, &resolved_src, rel_path)?;
            let local_hash = Self::sha256_hex(&content);
            let remote_path = file_tree::join(dest, rel_path);
            match ctx.checksum_remote(&remote_path).await? {
                Some(remote_hash) if remote_hash == local_hash => {}
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

        // Directories always, since an empty one has no file to upload into it; files only
        // for their attributes.
        let (files, dirs) = Self::managed_paths(dest, &tree);
        let checked: Vec<(String, bool)> = if options.changes_attrs() {
            dirs.iter()
                .map(|d| (d.clone(), true))
                .chain(files.iter().map(|f| (f.clone(), false)))
                .collect()
        } else {
            dirs.iter().map(|d| (d.clone(), true)).collect()
        };
        let paths: Vec<String> = checked.iter().map(|(p, _)| p.clone()).collect();
        let stats = ctx.stat_many(&paths).await?;
        let mut missing_dirs = 0usize;
        let mut attrs_changed = 0usize;
        for ((_, is_dir), stat) in checked.iter().zip(&stats) {
            match stat {
                None if *is_dir => missing_dirs += 1,
                // A missing file is new content, counted above.
                None => {}
                Some(found)
                    if options.changes_attrs() && !Self::attrs_match(&options, *is_dir, found) =>
                {
                    attrs_changed += 1
                }
                Some(_) => {}
            }
        }

        let strays = Self::strays(ctx, &options, &tree, src, dest).await?;

        if content_changed == 0 && attrs_changed == 0 && missing_dirs == 0 && strays.is_empty() {
            return Ok(ModuleStatus::Satisfied);
        }
        let mut parts = Vec::new();
        if content_changed > 0 {
            parts.push(format!("{} content", content_changed));
        }
        if attrs_changed > 0 {
            parts.push(format!("{} attrs", attrs_changed));
        }
        if missing_dirs > 0 {
            parts.push(format!("{} new dirs", missing_dirs));
        }
        let mut plan = format!("Upload dir {} -> {}", src, dest);
        if !parts.is_empty() {
            plan.push_str(&format!(
                " (changed: {} of {} files)",
                parts.join(", "),
                tree.files.len()
            ));
        }
        let removed = strays.paths(dest);
        if !removed.is_empty() {
            plan.push_str(&format!(
                "; remove {}: {}",
                removed.len(),
                Self::some_paths(&removed)
            ));
        }
        let removals: Vec<String> = removed.iter().map(|p| format!("remove {p}")).collect();

        if !ctx.diff {
            return Ok(ModuleStatus::pending(plan));
        }
        let mut shown = Vec::new();
        if opted_out && content_changed > 0 {
            shown.push(file_diff::opted_out(dest));
        }
        shown.extend(diffs);
        shown.extend(removals);
        if shown.is_empty() {
            Ok(ModuleStatus::pending(plan))
        } else {
            let diff = file_diff::truncate_lines(&shown.join("\n"), file_diff::MAX_DIFF_LINES);
            Ok(ModuleStatus::pending_with_diff(plan, diff))
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
        let options = Self::tree_options(params, dest)?;
        let (resolved_src, tree) = Self::source_tree(ctx, src, &options)?;
        let template = Self::is_template(params);

        let mut warnings = Vec::new();
        if !template {
            for rel_path in &tree.files {
                let content = Self::tree_file_content(ctx, params, src, &resolved_src, rel_path)?;
                let label = Self::shown_template(src, Path::new(rel_path));
                warnings.extend(Self::literal_reference_warning(ctx, &label, &content));
            }
        }
        let warnings = warnings.join("\n");

        if ctx.dry_run {
            let mut output = format!(
                "[dry-run] Would copy dir {} -> {} ({} files)",
                src,
                dest,
                tree.files.len()
            );
            let strays = Self::strays(ctx, &options, &tree, src, dest).await?;
            if !strays.is_empty() {
                output.push_str(&format!(" and remove {}", strays.len()));
            }
            return Ok(ModuleResult {
                changed: false,
                output,
                stderr: warnings,
                exit_code: 0,
                output_cut: false,
            });
        }

        // Refused before anything is uploaded, rather than by the attribute change after;
        // asked of the host, since `/tmp/..` or a symlink can name `/` too.
        if options.changes_attrs() && ctx.is_root_dir(dest).await? {
            return Err(crate::ssh::connection::root_refusal(dest));
        }
        // Listed before uploading, so a destination prune refuses is refused before any
        // change; what the upload adds is the source's, never a stray.
        let strays = Self::strays(ctx, &options, &tree, src, dest).await?;

        let (files, dirs) = Self::managed_paths(dest, &tree);
        let missing_dirs = ctx
            .stat_many(&dirs)
            .await?
            .iter()
            .filter(|stat| stat.is_none())
            .count();
        // `/` exists, and `mkdir -p` rejects the "" a copy to it would name.
        let to_create: Vec<&str> = dirs
            .iter()
            .map(|d| d.trim_end_matches('/'))
            .filter(|d| !d.is_empty())
            .collect();
        ctx.create_dirs(&to_create).await?;

        let mut uploaded = 0usize;
        for (rel_path, remote_path) in tree.files.iter().zip(&files) {
            let content = Self::tree_file_content(ctx, params, src, &resolved_src, rel_path)?;
            let needs_upload = match ctx.checksum_remote(remote_path).await? {
                Some(remote_hash) => remote_hash != Self::sha256_hex(&content),
                None => true,
            };
            if needs_upload {
                ctx.upload_file(&content, remote_path).await?;
                uploaded += 1;
            }
        }

        ctx.remove_strays(dest, &strays).await?;
        let removed = strays.len();

        ctx.set_tree_attrs(dest, &files, &dirs, &options).await?;

        let mut output = format!(
            "copy dir {} -> {} ({} uploaded, {} total",
            src,
            dest,
            uploaded,
            tree.files.len()
        );
        if missing_dirs > 0 {
            output.push_str(&format!(", {} dirs created", missing_dirs));
        }
        if options.prune {
            output.push_str(&format!(", {} removed", removed));
        }
        output.push(')');
        Ok(ModuleResult {
            changed: uploaded > 0 || missing_dirs > 0 || removed > 0 || options.changes_attrs(),
            output,
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
