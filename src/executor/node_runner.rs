use crate::executor::host_coordinator::{HostCoordinator, TaskKey};
use crate::executor::result::{ExecutorEvent, NodeResult};
use glidesh::config::template::{TemplateData, interpolate_args};
use glidesh::config::types::{LoopSource, ParamValue, Plan, ResolvedHost, Step};
use glidesh::error::GlideshError;
use glidesh::modules::context::ModuleContext;
use glidesh::modules::detect::{OsInfo, detect_os};
use glidesh::modules::host as host_module;
use glidesh::modules::{ModuleParams, ModuleRegistry, ModuleStatus};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// One iteration of a step `loop`. A flat item binds `${item}`; a structured
/// item (a row from a `vars` collection) binds `${item.<field>}` for each field.
#[derive(Debug)]
enum LoopItem {
    Flat(String),
    Structured(HashMap<String, String>),
}

/// Resolve a step's `loop` source into the items to iterate over. A `${name}`
/// referencing a `vars` collection yields structured rows (`${item.field}`);
/// one referencing a flat var yields its newline-split values (`${item}`); a
/// literal yields its lines.
fn resolve_loop_items(
    loop_source: &LoopSource,
    vars: &HashMap<String, String>,
    template_data: &TemplateData,
) -> Result<Vec<LoopItem>, String> {
    match loop_source {
        LoopSource::Variable(name) => {
            if let Some(rows) = template_data.collections.get(name) {
                Ok(rows.iter().cloned().map(LoopItem::Structured).collect())
            } else if let Some(value) = vars.get(name) {
                Ok(value
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .map(LoopItem::Flat)
                    .collect())
            } else {
                Err(format!("Loop variable '{}' is not defined", name))
            }
        }
        LoopSource::Literal(items) => Ok(items.iter().cloned().map(LoopItem::Flat).collect()),
    }
}

/// Bind a loop item's variables into `vars`, returning the keys that were
/// inserted so the caller can remove them after the iteration.
fn inject_loop_item(vars: &mut HashMap<String, String>, item: &LoopItem) -> Vec<String> {
    match item {
        LoopItem::Flat(value) => {
            vars.insert("item".to_string(), value.clone());
            vec!["item".to_string()]
        }
        LoopItem::Structured(row) => {
            let mut keys = Vec::with_capacity(row.len());
            for (field, value) in row {
                let key = format!("item.{field}");
                vars.insert(key.clone(), value.clone());
                keys.push(key);
            }
            keys
        }
    }
}

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
    pub coordinator: Arc<HostCoordinator>,
    pub all_targets: Arc<Vec<ResolvedHost>>,
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
                            step_idx,
                            0,
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
                    let items = match resolve_loop_items(loop_source, &vars, &template_data) {
                        Ok(items) => items,
                        Err(error) => {
                            self.emit_step_error(&step.name, &error);
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
                    };

                    let mut any_iteration_changed = false;
                    for (iter_idx, item) in items.iter().enumerate() {
                        let injected = inject_loop_item(&mut vars, item);
                        let result = self
                            .run_step_tasks(
                                step,
                                step_idx,
                                iter_idx,
                                &mut vars,
                                &template_data,
                                &session,
                                &os_info,
                                &mut total_changed,
                                force_apply,
                            )
                            .await;
                        for key in &injected {
                            vars.remove(key);
                        }
                        match result {
                            Ok(changed) => {
                                any_iteration_changed |= changed;
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

    /// Report a failure that happens while preparing a task (unknown module,
    /// `${...}` interpolation error) — paths that never reach `check`/`apply`
    /// and so would otherwise produce no log line at all.
    fn emit_task_error(&self, module: &str, resource: &str, error: &str) {
        let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
            host: self.host.name.clone(),
            module: module.to_string(),
            resource: resource.to_string(),
            error: error.to_string(),
        });
    }

    fn emit_step_error(&self, step: &str, error: &str) {
        let _ = self.event_tx.send(ExecutorEvent::StepFailed {
            host: self.host.name.clone(),
            step: step.to_string(),
            error: error.to_string(),
        });
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_step_tasks(
        &self,
        step: &Step,
        step_idx: usize,
        loop_iter: usize,
        vars: &mut HashMap<String, String>,
        template_data: &TemplateData,
        session: &SshSession,
        os_info: &OsInfo,
        total_changed: &mut usize,
        force_apply: bool,
    ) -> Result<bool, (String, String)> {
        let mut any_changed = false;

        for (task_idx, task) in step.tasks.iter().enumerate() {
            if task.module == host_module::MODULE_NAME {
                let changed = self
                    .run_host_task(
                        step,
                        step_idx,
                        task_idx,
                        loop_iter,
                        task,
                        vars,
                        total_changed,
                    )
                    .await?;
                any_changed |= changed;
                continue;
            }

            let module = match self.registry.get(&task.module) {
                Some(m) => m,
                None => {
                    let error = format!("Unknown module: {}", task.module);
                    self.emit_task_error(&task.module, &task.resource, &error);
                    return Err((step.name.clone(), error));
                }
            };

            let interpolated_args = match interpolate_args(&task.args, vars) {
                Ok(a) => a,
                Err(e) => {
                    self.emit_task_error(&task.module, &task.resource, &e.to_string());
                    return Err((step.name.clone(), e.to_string()));
                }
            };

            let mut resource_name =
                match glidesh::config::template::interpolate(&task.resource, vars) {
                    Ok(r) => r,
                    Err(e) => {
                        self.emit_task_error(&task.module, &task.resource, &e.to_string());
                        return Err((step.name.clone(), e.to_string()));
                    }
                };

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

            // Escalation precedence: module > step > plan > host (host already
            // carries group/global/CLI defaults merged during target resolution).
            let run_as = task
                .run_as
                .clone()
                .merge_over(&step.run_as)
                .merge_over(&self.plan.run_as)
                .merge_over(&self.host.run_as)
                .resolve(glidesh::modules::escalation::password());

            let ctx = ModuleContext {
                ssh: session,
                os_info,
                vars,
                template_data,
                dry_run: self.dry_run,
                plan_base_dir: &self.plan_base_dir,
                run_as,
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
                            stdout: result.output.clone(),
                            stderr: result.stderr.clone(),
                            exit_code: result.exit_code,
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
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: 0,
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

    #[allow(clippy::too_many_arguments)]
    async fn run_host_task(
        &self,
        step: &Step,
        step_idx: usize,
        task_idx: usize,
        loop_iter: usize,
        task: &glidesh::config::types::TaskDef,
        vars: &mut HashMap<String, String>,
        total_changed: &mut usize,
    ) -> Result<bool, (String, String)> {
        let interpolated_args = match interpolate_args(&task.args, vars) {
            Ok(a) => a,
            Err(e) => {
                self.emit_task_error(&task.module, &task.resource, &e.to_string());
                return Err((step.name.clone(), e.to_string()));
            }
        };
        let mut resource_name = match glidesh::config::template::interpolate(&task.resource, vars) {
            Ok(r) => r,
            Err(e) => {
                self.emit_task_error(&task.module, &task.resource, &e.to_string());
                return Err((step.name.clone(), e.to_string()));
            }
        };

        if resource_name.is_empty() {
            match interpolated_args.get("cmd") {
                Some(ParamValue::List(cmds)) => resource_name = cmds.join(" && "),
                Some(ParamValue::String(s)) => resource_name = s.clone(),
                _ => {}
            }
        }

        let params = ModuleParams {
            resource_name: resource_name.clone(),
            args: interpolated_args,
        };

        let _ = self.event_tx.send(ExecutorEvent::ModuleCheck {
            host: self.host.name.clone(),
            module: task.module.clone(),
            resource: resource_name.clone(),
        });

        let key = TaskKey {
            step_idx,
            task_idx,
            loop_iter,
        };

        let targets = self.all_targets.clone();
        let ssh_key = self.key.clone();
        let policy = self.host_key_policy;
        let dry_run = self.dry_run;
        let params_for_exec = params.clone();

        let result = self
            .coordinator
            .get_or_run(key, move || async move {
                host_module::run_host_task(&params_for_exec, &targets, &ssh_key, policy, dry_run)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

        match result {
            Ok(out) => {
                if let Some(ref var_name) = task.register {
                    vars.insert(var_name.clone(), out.stdout.trim().to_string());
                }
                let changed = !self.dry_run;
                if changed {
                    *total_changed += 1;
                }
                let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                    host: self.host.name.clone(),
                    module: task.module.clone(),
                    resource: resource_name,
                    changed,
                    stdout: out.stdout.clone(),
                    stderr: out.stderr.clone(),
                    exit_code: out.exit_code,
                });
                Ok(changed)
            }
            Err(msg) => {
                let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                    host: self.host.name.clone(),
                    module: task.module.clone(),
                    resource: resource_name,
                    error: msg.clone(),
                });
                Err((step.name.clone(), msg))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collection(rows: Vec<Vec<(&str, &str)>>) -> Vec<HashMap<String, String>> {
        rows.into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn resolve_structured_collection_loop() {
        let mut td = TemplateData::default();
        td.collections.insert(
            "vms".to_string(),
            collection(vec![
                vec![("name", "vm-a"), ("port", "2301")],
                vec![("name", "vm-b"), ("port", "2302")],
            ]),
        );
        let vars = HashMap::new();

        let items =
            resolve_loop_items(&LoopSource::Variable("vms".to_string()), &vars, &td).unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], LoopItem::Structured(_)));
    }

    #[test]
    fn structured_item_binds_dotted_fields() {
        let row = collection(vec![vec![("name", "vm-a"), ("port", "2301")]])
            .pop()
            .unwrap();
        let mut vars = HashMap::new();
        let injected = inject_loop_item(&mut vars, &LoopItem::Structured(row));

        assert_eq!(vars.get("item.name").map(String::as_str), Some("vm-a"));
        assert_eq!(vars.get("item.port").map(String::as_str), Some("2301"));

        for key in &injected {
            vars.remove(key);
        }
        assert!(vars.is_empty());
    }

    #[test]
    fn flat_variable_falls_back_to_newline_split() {
        let mut vars = HashMap::new();
        vars.insert("disks".to_string(), "sda\nsdb\n".to_string());
        let td = TemplateData::default();

        let items =
            resolve_loop_items(&LoopSource::Variable("disks".to_string()), &vars, &td).unwrap();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], LoopItem::Flat(s) if s == "sda"));
    }

    #[test]
    fn flat_item_binds_bare_item() {
        let mut vars = HashMap::new();
        let injected = inject_loop_item(&mut vars, &LoopItem::Flat("sda".to_string()));
        assert_eq!(vars.get("item").map(String::as_str), Some("sda"));
        assert_eq!(injected, vec!["item".to_string()]);
    }

    #[test]
    fn undefined_loop_variable_is_an_error() {
        let vars = HashMap::new();
        let td = TemplateData::default();
        let err =
            resolve_loop_items(&LoopSource::Variable("vms".to_string()), &vars, &td).unwrap_err();
        assert!(err.contains("vms"));
        assert!(err.contains("not defined"));
    }

    #[test]
    fn collection_takes_precedence_over_flat_var() {
        let mut td = TemplateData::default();
        td.collections
            .insert("x".to_string(), collection(vec![vec![("name", "a")]]));
        let mut vars = HashMap::new();
        vars.insert("x".to_string(), "flat".to_string());

        let items = resolve_loop_items(&LoopSource::Variable("x".to_string()), &vars, &td).unwrap();
        assert!(matches!(items[0], LoopItem::Structured(_)));
    }
}
