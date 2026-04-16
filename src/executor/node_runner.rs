use crate::executor::result::{ExecutorEvent, NodeResult};
use glidesh::config::template::{TemplateData, interpolate_args};
use glidesh::config::types::{LoopSource, ParamValue, Plan, ResolvedHost, Step};
use glidesh::error::GlideshError;
use glidesh::modules::context::ModuleContext;
use glidesh::modules::detect::{OsInfo, detect_os};
use glidesh::modules::{ModuleParams, ModuleRegistry, ModuleStatus};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct NodeRunner {
    pub host: ResolvedHost,
    pub plan: Arc<Plan>,
    pub registry: Arc<ModuleRegistry>,
    pub key: PrivateKeyWithHashAlg,
    pub dry_run: bool,
    pub host_key_policy: HostKeyPolicy,
    pub event_tx: mpsc::UnboundedSender<ExecutorEvent>,
    pub inventory_template_data: Arc<TemplateData>,
    pub plan_base_dir: Arc<PathBuf>,
}

impl NodeRunner {
    pub async fn run(self) -> NodeResult {
        match self.run_inner().await {
            Ok(result) => result,
            Err(_) => {
                let _ = self.event_tx.send(ExecutorEvent::NodeComplete {
                    host: self.host.name.clone(),
                    success: false,
                    changed: 0,
                });
                NodeResult {
                    success: false,
                    total_changed: 0,
                }
            }
        }
    }

    async fn run_inner(&self) -> Result<NodeResult, GlideshError> {
        let _ = self.event_tx.send(ExecutorEvent::NodeConnecting {
            host: self.host.name.clone(),
        });

        let session = match &self.host.jump {
            Some(jump) => {
                SshSession::connect_via_jump(
                    &self.host.address,
                    self.host.port,
                    &self.host.user,
                    &self.key,
                    self.host_key_policy,
                    jump,
                )
                .await
            }
            None => {
                SshSession::connect(
                    &self.host.address,
                    self.host.port,
                    &self.host.user,
                    &self.key,
                    self.host_key_policy,
                )
                .await
            }
        }
        .inspect_err(|e| {
            let _ = self.event_tx.send(ExecutorEvent::NodeAuthFailed {
                host: self.host.name.clone(),
                error: e.to_string(),
            });
        })?;

        let os_info = detect_os(&session).await?;

        let _ = self.event_tx.send(ExecutorEvent::NodeConnected {
            host: self.host.name.clone(),
            os: os_info.clone(),
        });

        // Merge vars: inventory host vars + plan vars (plan wins)
        let mut vars = self.host.vars.clone();
        vars.extend(self.plan.vars.iter().map(|(k, v)| (k.clone(), v.clone())));

        // Inject built-in host vars (cannot be overridden by user vars)
        vars.insert("host.name".to_string(), self.host.name.clone());
        vars.insert("host.address".to_string(), self.host.address.clone());
        vars.insert("host.user".to_string(), self.host.user.clone());
        vars.insert("host.port".to_string(), self.host.port.to_string());

        // Build template data: inventory @-refs + plan structured vars.
        // Preserve inventory-provided collections so plan structured vars
        // cannot overwrite reserved @group.* or @inventory.* namespaces.
        let mut template_data = (*self.inventory_template_data).clone();
        for (key, value) in &self.plan.structured_vars {
            if !template_data.collections.contains_key(key) {
                template_data.collections.insert(key.clone(), value.clone());
            }
        }

        let steps = self.plan.steps();
        let total_steps = steps.len();
        let mut total_changed = 0;
        let mut step_changed: HashMap<String, bool> = HashMap::new();

        for (step_idx, step) in steps.iter().enumerate() {
            let _ = self.event_tx.send(ExecutorEvent::StepStarted {
                host: self.host.name.clone(),
                step: step.name.clone(),
                step_index: step_idx,
                total_steps,
            });

            let force_apply = step
                .subscribe
                .iter()
                .any(|s| step_changed.get(s).copied().unwrap_or(false));

            match &step.loop_source {
                None => {
                    match self
                        .run_step_tasks(
                            step,
                            &mut vars,
                            &template_data,
                            &session,
                            &os_info,
                            &mut total_changed,
                            force_apply,
                        )
                        .await
                    {
                        Ok(changed) => {
                            step_changed.insert(step.name.clone(), changed);
                        }
                        Err(_) => {
                            let _ = self.event_tx.send(ExecutorEvent::NodeComplete {
                                host: self.host.name.clone(),
                                success: false,
                                changed: total_changed,
                            });
                            let _ = session.close().await;
                            return Ok(NodeResult {
                                success: false,
                                total_changed,
                            });
                        }
                    }
                }
                Some(loop_source) => {
                    let items: Vec<String> = match loop_source {
                        LoopSource::Variable(var_name) => {
                            let value =
                                vars.get(var_name)
                                    .ok_or_else(|| GlideshError::TemplateError {
                                        message: format!(
                                            "Loop variable '{}' is not defined",
                                            var_name
                                        ),
                                    })?;
                            value
                                .lines()
                                .map(|l| l.trim().to_string())
                                .filter(|l| !l.is_empty())
                                .collect()
                        }
                        LoopSource::Literal(items) => items.clone(),
                    };

                    let mut any_iteration_changed = false;
                    for item in &items {
                        vars.insert("item".to_string(), item.clone());
                        match self
                            .run_step_tasks(
                                step,
                                &mut vars,
                                &template_data,
                                &session,
                                &os_info,
                                &mut total_changed,
                                force_apply,
                            )
                            .await
                        {
                            Ok(changed) => {
                                any_iteration_changed |= changed;
                            }
                            Err(_) => {
                                vars.remove("item");
                                let _ = self.event_tx.send(ExecutorEvent::NodeComplete {
                                    host: self.host.name.clone(),
                                    success: false,
                                    changed: total_changed,
                                });
                                let _ = session.close().await;
                                return Ok(NodeResult {
                                    success: false,
                                    total_changed,
                                });
                            }
                        }
                    }
                    vars.remove("item");
                    step_changed.insert(step.name.clone(), any_iteration_changed);
                }
            }
        }

        let _ = session.close().await;

        let _ = self.event_tx.send(ExecutorEvent::NodeComplete {
            host: self.host.name.clone(),
            success: true,
            changed: total_changed,
        });

        Ok(NodeResult {
            success: true,
            total_changed,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_step_tasks(
        &self,
        step: &Step,
        vars: &mut HashMap<String, String>,
        template_data: &TemplateData,
        session: &SshSession,
        os_info: &OsInfo,
        total_changed: &mut usize,
        force_apply: bool,
    ) -> Result<bool, (String, String)> {
        let mut any_changed = false;

        for task in &step.tasks {
            let module = self.registry.get(&task.module).ok_or_else(|| {
                (
                    step.name.clone(),
                    format!("Unknown module: {}", task.module),
                )
            })?;

            let interpolated_args = interpolate_args(&task.args, vars)
                .map_err(|e| (step.name.clone(), e.to_string()))?;

            let mut resource_name = glidesh::config::template::interpolate(&task.resource, vars)
                .map_err(|e| (step.name.clone(), e.to_string()))?;

            if resource_name.is_empty() {
                match interpolated_args.get("cmd") {
                    Some(ParamValue::List(cmds)) => resource_name = cmds.join(" && "),
                    Some(ParamValue::String(s)) => resource_name = s.clone(),
                    _ => {}
                }
            }

            let params = ModuleParams {
                resource_name,
                args: interpolated_args,
            };

            let ctx = ModuleContext {
                ssh: session,
                os_info,
                vars,
                template_data,
                dry_run: self.dry_run,
                plan_base_dir: &self.plan_base_dir,
            };

            let _ = self.event_tx.send(ExecutorEvent::ModuleCheck {
                host: self.host.name.clone(),
                module: task.module.clone(),
                resource: params.resource_name.clone(),
            });

            let status = match module.check(&ctx, &params).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                        host: self.host.name.clone(),
                        module: task.module.clone(),
                        resource: params.resource_name.clone(),
                        error: e.to_string(),
                    });
                    return Err((step.name.clone(), e.to_string()));
                }
            };

            let should_apply = match &status {
                ModuleStatus::Satisfied => force_apply,
                ModuleStatus::Pending { .. } => true,
                ModuleStatus::Unknown { .. } => false,
            };

            if should_apply {
                match module.apply(&ctx, &params).await {
                    Ok(result) => {
                        if result.changed || force_apply {
                            *total_changed += 1;
                            any_changed = true;
                        }
                        if let Some(ref var_name) = task.register {
                            vars.insert(var_name.clone(), result.output.trim().to_string());
                        }
                        let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            changed: result.changed,
                        });
                    }
                    Err(e) => {
                        let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            error: e.to_string(),
                        });
                        return Err((step.name.clone(), e.to_string()));
                    }
                }
            } else {
                match status {
                    ModuleStatus::Satisfied => {
                        if let Some(ref var_name) = task.register {
                            vars.insert(var_name.clone(), String::new());
                        }
                        let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            changed: false,
                        });
                    }
                    ModuleStatus::Unknown { reason } => {
                        let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            error: format!("Check returned unknown: {}", reason),
                        });
                    }
                    ModuleStatus::Pending { .. } => unreachable!(),
                }
            }
        }
        Ok(any_changed)
    }
}
