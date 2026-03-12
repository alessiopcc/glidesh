use crate::error::GlideshError;
use crate::modules::context::ModuleContext;
use crate::modules::{Module, ModuleParams, ModuleResult, ModuleStatus};
use async_trait::async_trait;

pub struct DiskModule;

#[async_trait]
impl Module for DiskModule {
    fn name(&self) -> &str {
        "disk"
    }

    async fn check(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleStatus, GlideshError> {
        let device = &params.resource_name;
        let desired_fs = params
            .args
            .get("fs")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "disk".to_string(),
                message: "missing required parameter 'fs'".to_string(),
            })?;
        let mount_point = params
            .args
            .get("mount")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "disk".to_string(),
                message: "missing required parameter 'mount'".to_string(),
            })?;
        let state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("mounted");

        let mut needs_change = Vec::new();

        match state {
            "mounted" => {
                let force = params
                    .args
                    .get("force")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                let blkid = ctx
                    .ssh
                    .exec(&format!("blkid -o value -s TYPE {} 2>/dev/null", device))
                    .await?;
                let current_fs = blkid.stdout.trim();
                if current_fs != desired_fs {
                    if !current_fs.is_empty() && !force {
                        return Err(GlideshError::Module {
                            module: "disk".to_string(),
                            message: format!(
                                "{} already has filesystem '{}' (expected '{}'). Set force #true to reformat.",
                                device, current_fs, desired_fs
                            ),
                        });
                    }
                    needs_change.push("format");
                }

                let fstab = ctx
                    .ssh
                    .exec(&format!(
                        "grep -c '\\s{}\\s' /etc/fstab 2>/dev/null || echo 0",
                        mount_point
                    ))
                    .await?;
                if fstab.stdout.trim() == "0" {
                    needs_change.push("add fstab entry");
                }

                let findmnt = ctx
                    .ssh
                    .exec(&format!("findmnt -n {} 2>/dev/null", mount_point))
                    .await?;
                if findmnt.stdout.trim().is_empty() {
                    needs_change.push("mount");
                }
            }
            "unmounted" | "absent" => {
                let findmnt = ctx
                    .ssh
                    .exec(&format!("findmnt -n {} 2>/dev/null", mount_point))
                    .await?;
                if !findmnt.stdout.trim().is_empty() {
                    needs_change.push("unmount");
                }

                let fstab = ctx
                    .ssh
                    .exec(&format!(
                        "grep -c '\\s{}\\s' /etc/fstab 2>/dev/null || echo 0",
                        mount_point
                    ))
                    .await?;
                if fstab.stdout.trim() != "0" {
                    needs_change.push("remove fstab entry");
                }
            }
            _ => {
                return Err(GlideshError::Module {
                    module: "disk".to_string(),
                    message: format!(
                        "invalid state '{}': expected mounted, unmounted, or absent",
                        state
                    ),
                });
            }
        }

        if needs_change.is_empty() {
            Ok(ModuleStatus::Satisfied)
        } else {
            Ok(ModuleStatus::Pending {
                plan: format!("disk {}: {}", device, needs_change.join(", ")),
            })
        }
    }

    async fn apply(
        &self,
        ctx: &ModuleContext<'_>,
        params: &ModuleParams,
    ) -> Result<ModuleResult, GlideshError> {
        let device = &params.resource_name;
        let desired_fs = params
            .args
            .get("fs")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "disk".to_string(),
                message: "missing required parameter 'fs'".to_string(),
            })?;
        let mount_point = params
            .args
            .get("mount")
            .and_then(|v| v.as_str())
            .ok_or_else(|| GlideshError::Module {
                module: "disk".to_string(),
                message: "missing required parameter 'mount'".to_string(),
            })?;
        let opts = params
            .args
            .get("opts")
            .and_then(|v| v.as_str())
            .unwrap_or("defaults");
        let state = params
            .args
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("mounted");

        if ctx.dry_run {
            return Ok(ModuleResult {
                changed: false,
                output: format!("[dry-run] Would manage disk {} -> {}", device, mount_point),
                stderr: String::new(),
                exit_code: 0,
            });
        }

        let force = params
            .args
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match state {
            "mounted" => {
                self.apply_mounted(ctx, device, desired_fs, mount_point, opts, force)
                    .await
            }
            "unmounted" | "absent" => self.apply_unmounted(ctx, mount_point).await,
            _ => Err(GlideshError::Module {
                module: "disk".to_string(),
                message: format!(
                    "invalid state '{}': expected mounted, unmounted, or absent",
                    state
                ),
            }),
        }
    }
}

impl DiskModule {
    async fn apply_mounted(
        &self,
        ctx: &ModuleContext<'_>,
        device: &str,
        fs: &str,
        mount_point: &str,
        opts: &str,
        force: bool,
    ) -> Result<ModuleResult, GlideshError> {
        let mut actions = Vec::new();

        let blkid = ctx
            .ssh
            .exec(&format!("blkid -o value -s TYPE {} 2>/dev/null", device))
            .await?;
        let current_fs = blkid.stdout.trim();
        if current_fs != fs {
            if !current_fs.is_empty() && !force {
                return Err(GlideshError::Module {
                    module: "disk".to_string(),
                    message: format!(
                        "{} already has filesystem '{}' (expected '{}'). Set force #true to reformat.",
                        device, current_fs, fs
                    ),
                });
            }
            let force_flag = if force { " -f" } else { "" };
            let mkfs = ctx
                .ssh
                .exec(&format!("mkfs.{}{} {}", fs, force_flag, device))
                .await?;
            if mkfs.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "disk".to_string(),
                    message: format!(
                        "mkfs.{} failed (exit {}): {}",
                        fs, mkfs.exit_code, mkfs.stderr
                    ),
                });
            }
            actions.push(format!("formatted {} as {}", device, fs));
        }

        let uuid_out = ctx
            .ssh
            .exec(&format!("blkid -o value -s UUID {}", device))
            .await?;
        let uuid = uuid_out.stdout.trim().to_string();
        if uuid.is_empty() {
            return Err(GlideshError::Module {
                module: "disk".to_string(),
                message: format!("could not determine UUID for {}", device),
            });
        }

        let mkdir = ctx.ssh.exec(&format!("mkdir -p {}", mount_point)).await?;
        if mkdir.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "disk".to_string(),
                message: format!("mkdir -p {} failed: {}", mount_point, mkdir.stderr),
            });
        }

        // Update fstab: remove existing entry for this mount point, then append
        ctx.ssh
            .exec(&format!("sed -i '\\|\\s{}\\s|d' /etc/fstab", mount_point))
            .await?;
        let fstab_line = format!("UUID={}  {}  {}  {}  0  2", uuid, mount_point, fs, opts);
        let append = ctx
            .ssh
            .exec(&format!("echo '{}' >> /etc/fstab", fstab_line))
            .await?;
        if append.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "disk".to_string(),
                message: format!("failed to update fstab: {}", append.stderr),
            });
        }
        actions.push("updated fstab".to_string());

        let findmnt = ctx
            .ssh
            .exec(&format!("findmnt -n {} 2>/dev/null", mount_point))
            .await?;
        if findmnt.stdout.trim().is_empty() {
            let mount = ctx.ssh.exec(&format!("mount {}", mount_point)).await?;
            if mount.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "disk".to_string(),
                    message: format!(
                        "mount {} failed (exit {}): {}",
                        mount_point, mount.exit_code, mount.stderr
                    ),
                });
            }
            actions.push(format!("mounted {}", mount_point));
        }

        Ok(ModuleResult {
            changed: true,
            output: actions.join("; "),
            stderr: String::new(),
            exit_code: 0,
        })
    }

    async fn apply_unmounted(
        &self,
        ctx: &ModuleContext<'_>,
        mount_point: &str,
    ) -> Result<ModuleResult, GlideshError> {
        let mut actions = Vec::new();

        let findmnt = ctx
            .ssh
            .exec(&format!("findmnt -n {} 2>/dev/null", mount_point))
            .await?;
        if !findmnt.stdout.trim().is_empty() {
            let umount = ctx.ssh.exec(&format!("umount {}", mount_point)).await?;
            if umount.exit_code != 0 {
                return Err(GlideshError::Module {
                    module: "disk".to_string(),
                    message: format!(
                        "umount {} failed (exit {}): {}",
                        mount_point, umount.exit_code, umount.stderr
                    ),
                });
            }
            actions.push(format!("unmounted {}", mount_point));
        }

        let sed = ctx
            .ssh
            .exec(&format!("sed -i '\\|\\s{}\\s|d' /etc/fstab", mount_point))
            .await?;
        if sed.exit_code != 0 {
            return Err(GlideshError::Module {
                module: "disk".to_string(),
                message: format!("failed to remove fstab entry: {}", sed.stderr),
            });
        }
        actions.push("removed fstab entry".to_string());

        Ok(ModuleResult {
            changed: true,
            output: actions.join("; "),
            stderr: String::new(),
            exit_code: 0,
        })
    }
}
