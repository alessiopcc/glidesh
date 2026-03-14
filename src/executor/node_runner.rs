use crate::executor::result::{ExecutorEvent, NodeResult};
use glidesh::config::template::interpolate_args;
use glidesh::config::types::{LoopSource, Plan, ResolvedHost, Step};
use glidesh::error::GlideshError;
use glidesh::modules::context::ModuleContext;
use glidesh::modules::detect::{OsInfo, detect_os};
use glidesh::modules::{ModuleParams, ModuleRegistry, ModuleStatus};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
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

        let session = SshSession::connect(
            &self.host.address,
            self.host.port,
            &self.host.user,
            &self.key,
            self.host_key_policy,
        )
        .await
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

        let steps = self.plan.steps();
        let total_steps = steps.len();
        let mut total_changed = 0;

        for (step_idx, step) in steps.iter().enumerate() {
            let _ = self.event_tx.send(ExecutorEvent::StepStarted {
                host: self.host.name.clone(),
                step: step.name.clone(),
                step_index: step_idx,
                total_steps,
            });

            match &step.loop_source {
                None => {
                    if self
                        .run_step_tasks(step, &mut vars, &session, &os_info, &mut total_changed)
                        .await
                        .is_err()
                    {
                        let _ = session.close().await;
                        return Ok(NodeResult {
                            success: false,
                            total_changed,
                        });
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

                    for item in &items {
                        vars.insert("item".to_string(), item.clone());
                        if self
                            .run_step_tasks(step, &mut vars, &session, &os_info, &mut total_changed)
                            .await
                            .is_err()
                        {
                            vars.remove("item");
                            let _ = session.close().await;
                            return Ok(NodeResult {
                                success: false,
                                total_changed,
                            });
                        }
                    }
                    vars.remove("item");
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

    async fn run_step_tasks(
        &self,
        step: &Step,
        vars: &mut HashMap<String, String>,
        session: &SshSession,
        os_info: &OsInfo,
        total_changed: &mut usize,
    ) -> Result<(), (String, String)> {
        for task in &step.tasks {
            let module = self.registry.get(&task.module).ok_or_else(|| {
                (
                    step.name.clone(),
                    format!("Unknown module: {}", task.module),
                )
            })?;

            let interpolated_args = interpolate_args(&task.args, vars)
                .map_err(|e| (step.name.clone(), e.to_string()))?;

            let params = ModuleParams {
                resource_name: glidesh::config::template::interpolate(&task.resource, vars)
                    .map_err(|e| (step.name.clone(), e.to_string()))?,
                args: interpolated_args,
            };

            let ctx = ModuleContext {
                ssh: session,
                os_info,
                vars,
                dry_run: self.dry_run,
            };

            let _ = self.event_tx.send(ExecutorEvent::ModuleCheck {
                host: self.host.name.clone(),
                module: task.module.clone(),
                resource: params.resource_name.clone(),
            });

            let status = module
                .check(&ctx, &params)
                .await
                .map_err(|e| (step.name.clone(), e.to_string()))?;

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
                ModuleStatus::Pending { .. } => match module.apply(&ctx, &params).await {
                    Ok(result) => {
                        if result.changed {
                            *total_changed += 1;
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
                },
                ModuleStatus::Unknown { reason } => {
                    let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                        host: self.host.name.clone(),
                        module: task.module.clone(),
                        resource: params.resource_name.clone(),
                        error: format!("Check returned unknown: {}", reason),
                    });
                }
            }
        }
        Ok(())
    }
}
