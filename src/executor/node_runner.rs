use crate::executor::barrier::Seat;
use crate::executor::event_sink::EventSink;
use crate::executor::host_coordinator::{HostCoordinator, TaskKey};
use crate::executor::result::{ExecutorEvent, NodeResult};
use glidesh::config::condition::{Condition, Outcome, Scope};
use glidesh::config::tags::TagFilter;
use glidesh::config::template::{TemplateData, interpolate_args};
use glidesh::config::types::{
    LoopSource, ParamValue, Plan, ResolvedHost, Step, TaskDef, UntilGate,
};
use glidesh::error::GlideshError;
use glidesh::modules::context::{ModuleContext, Trigger};
use glidesh::modules::detect::{OsInfo, detect_os};
use glidesh::modules::host as host_module;
use glidesh::modules::{ModuleParams, ModuleRegistry, ModuleStatus};
use glidesh::secrets::{Secrets, token};
use glidesh::ssh::connection::{CommandOutput, OUTPUT_LIMIT};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use russh_keys::key::PrivateKeyWithHashAlg;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// One iteration of a step `loop`. A flat item binds `${@item}`; a structured
/// item (a row from a `vars` collection) binds `${@item.<field>}` for each field.
#[derive(Debug)]
enum LoopItem {
    Flat(String),
    Structured(HashMap<String, String>),
}

/// Resolve a step's `loop` source into the items to iterate over. A `${name}`
/// referencing a `vars` collection yields structured rows (`${@item.field}`);
/// one referencing a flat var yields its newline-split values (`${@item}`); a
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

/// Built-in per-host variables, exposed under the reserved `@host.*` namespace. Only these
/// `@`-prefixed names are injected — the legacy bare `host.*` forms were removed.
pub(crate) fn host_builtin_vars(host: &ResolvedHost) -> [(String, String); 4] {
    [
        ("@host.name".to_string(), host.name.clone()),
        ("@host.address".to_string(), host.address.clone()),
        ("@host.user".to_string(), host.user.clone()),
        ("@host.port".to_string(), host.port.to_string()),
    ]
}

/// Built-in OS facts, exposed under the reserved `@os.*` namespace. They come from the
/// detection every run already performs on connect, so they cost no extra round trip.
fn os_builtin_vars(os: &OsInfo) -> [(String, String); 7] {
    // Always defined, so a host with no runtime expands to "" rather than failing the
    // task on an undefined variable.
    let runtime = os.container_runtime.as_ref().map_or("", |rt| rt.as_str());
    [
        ("@os.id".to_string(), os.id.clone()),
        ("@os.version".to_string(), os.version.clone()),
        ("@os.family".to_string(), os.family.as_str().to_string()),
        (
            "@os.pkg-manager".to_string(),
            os.pkg_manager.as_str().to_string(),
        ),
        ("@os.init".to_string(), os.init_system.as_str().to_string()),
        ("@os.container-runtime".to_string(), runtime.to_string()),
        (
            "@os.nix-installed".to_string(),
            os.nix_installed.to_string(),
        ),
    ]
}

/// Bind a loop item's variables into `vars` under the reserved `@item` namespace,
/// returning the keys that were inserted so the caller can remove them after the iteration.
/// A flat item binds `@item`; a structured row binds `@item.<field>` for each field.
fn inject_loop_item(vars: &mut HashMap<String, String>, item: &LoopItem) -> Vec<String> {
    match item {
        LoopItem::Flat(value) => {
            vars.insert("@item".to_string(), value.clone());
            vec!["@item".to_string()]
        }
        LoopItem::Structured(row) => {
            let mut keys = Vec::with_capacity(row.len());
            for (field, value) in row {
                let key = format!("@item.{field}");
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
    pub diff: bool,
    pub tags: Arc<TagFilter>,
    pub host_key_policy: HostKeyPolicy,
    pub event_tx: EventSink,
    pub inventory_template_data: Arc<TemplateData>,
    pub plan_base_dir: Arc<PathBuf>,
    pub coordinator: Arc<HostCoordinator>,
    pub all_targets: Arc<Vec<ResolvedHost>>,
    pub secrets: Arc<Secrets>,
    /// Set in `mode "sync"`; `None` runs the plan free, as `async` does.
    pub sync: Option<SyncSlot>,
}

/// A host's part in a sync run.
///
/// `--concurrency` bounds how many hosts connect or run a step at once, not how many are in
/// the run: a host holds a permit only while connecting and while running a step, and none
/// while it waits at the barrier. Holding one for the whole run, as async does, would
/// deadlock as soon as there are more hosts than permits — the hosts at the barrier would
/// wait for hosts that can never start.
pub struct SyncSlot {
    pub seat: Seat,
    pub permits: Arc<Semaphore>,
}

/// How often a long `until=` wait reports that it is still waiting.
const WAIT_REPORT_EVERY: Duration = Duration::from_secs(30);

/// When an `until=` gate last said it was waiting.
struct WaitReports {
    started: Instant,
    last: Option<Instant>,
}

impl WaitReports {
    /// The first report comes as soon as an attempt fails, or this long into a slow first
    /// attempt; later ones this long after the previous.
    fn next_due(&self) -> Instant {
        self.last.unwrap_or(self.started) + WAIT_REPORT_EVERY
    }
}

/// Lines of the gate command's last output kept in a timeout error.
const GATE_OUTPUT_LINES: usize = 20;

/// The failure of a gate that never opened, with the output of the last attempt that
/// finished — it is nearly always where the real answer is. `still_running` when the final
/// attempt was cut off at the deadline, so it has no exit code and its output never arrived;
/// `last` is then the attempt before it, if any.
fn gate_timeout_error(
    gate: &UntilGate,
    last: Option<&CommandOutput>,
    still_running: bool,
) -> String {
    let mut message = format!(
        "until= did not succeed within {}s: `{}`",
        gate.timeout, gate.command
    );
    if still_running {
        message.push_str(" was still running at the deadline");
    }
    let Some(last) = last else {
        return message;
    };
    message.push_str(&if still_running {
        format!("; the attempt before exited {}", last.exit_code)
    } else {
        format!(" last exited {}", last.exit_code)
    });
    let output: Vec<&str> = last
        .stdout
        .lines()
        .chain(last.stderr.lines())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail = &output[output.len().saturating_sub(GATE_OUTPUT_LINES)..];
    if !tail.is_empty() {
        message.push_str(&format!("; last output:\n{}", tail.join("\n")));
    }
    message
}

/// Whether a task counts toward the run's changed total.
///
/// In a dry run the answer comes from `check` (did it report work outstanding?), because
/// `apply` deliberately reports `changed: false` when it is told not to touch the host.
/// A triggered subscriber is no exception: its module's `check` reports the restart or
/// rerun it would do as pending, so a preview and the run it previews count it alike.
fn resolve_changed(dry_run: bool, was_pending: bool, applied_changed: bool) -> bool {
    if dry_run {
        was_pending
    } else {
        applied_changed
    }
}

/// `changed-when=#false` declares a task never changes anything. It is read here as well as
/// in the module because a preview's count comes from `check`, which knows only that the
/// task is pending — so without this the preview would say "would change" for a task the
/// real run reports as `ok`. It also keeps such a task from triggering subscribers.
fn never_changes(task: &TaskDef) -> bool {
    matches!(task.args.get("changed-when"), Some(ParamValue::Bool(false)))
}

/// What a task reports, given what `check` found and what the run asked to see.
///
/// A preview leads with `check`'s description of the pending work, so the reason
/// ("Recreate container web (configuration changed)") sits above the module's own
/// "[dry-run] would ..." line. A real run leads with its own output, as it always has —
/// the reason would be describing something that has already happened.
///
/// `--diff` applies to both: it is the request to see the detail behind a change, whether
/// that change is about to be made or has just been made. A module may hand back a `diff`
/// whether or not it was asked for — an external plugin decides that for itself — so it is
/// dropped unless `show_diff` says the run wants it.
fn task_output(
    dry_run: bool,
    show_diff: bool,
    pending: Option<(&str, Option<&str>)>,
    output: &str,
) -> String {
    let (plan, diff) = match pending {
        Some((plan, diff)) => (Some(plan), if show_diff { diff } else { None }),
        None => (None, None),
    };
    let plan = if dry_run { plan } else { None };

    let mut out = String::new();
    for part in [plan, diff, Some(output)].into_iter().flatten() {
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(part);
    }
    out
}

/// What `register` captures from a task's output.
///
/// A preview never runs the task's own command, so there is no output to capture: what a
/// module returns there describes what it *would* do. Registering that would hand a later
/// `loop="${var}"` a "[dry-run] ..." sentence to iterate over, so capture nothing instead —
/// which is what a satisfied task already does. (Read-only probes such as a `shell`
/// `check=` guard do run during a preview; their output is not a task's output.)
///
/// Output cut at [`OUTPUT_LIMIT`] is refused rather than registered: its middle is gone, and
/// a later `loop=` would iterate over the marker line as if it were an item.
fn captured_output(dry_run: bool, output: &str, cut: bool) -> Result<String, String> {
    if dry_run {
        Ok(String::new())
    } else if cut {
        Err(format!(
            "output is over {} MiB, too long for register= (only its start and end were kept)",
            OUTPUT_LIMIT / (1024 * 1024)
        ))
    } else {
        Ok(output.trim().to_string())
    }
}

/// What one host's run has accumulated so far.
#[derive(Default)]
struct Progress {
    changed: usize,
    skipped: usize,
    /// Variables a preview registered from a task that would have run. They are defined —
    /// registering always defines — but hold "" where the real run would capture output, so
    /// a `when=` that reads their value cannot be answered.
    unknown: HashSet<String>,
}

impl Progress {
    fn register(
        &mut self,
        vars: &mut HashMap<String, String>,
        name: &str,
        value: String,
        real: bool,
    ) {
        vars.insert(name.to_string(), value);
        if real {
            self.unknown.remove(name);
        } else {
            self.unknown.insert(name.to_string());
        }
    }

    /// Record what a skipped task's `register` leaves behind.
    ///
    /// A skip the real run would also make registers nothing: the variable is left undefined
    /// rather than empty, so a later reference fails loudly instead of expanding to "" inside
    /// a command. A skip a preview could not decide might not happen on the real run, which
    /// could define the variable — so neither its value nor its presence is known, and a later
    /// `defined` must not answer as if it were.
    fn skip_register(&mut self, vars: &mut HashMap<String, String>, name: &str, decided: bool) {
        vars.remove(name);
        if decided {
            self.unknown.remove(name);
        } else {
            self.unknown.insert(name.to_string());
        }
    }
}

/// Whether a `when=` lets a step or task run.
#[derive(Debug, PartialEq)]
enum Gate {
    Run,
    /// `decided` is false when a preview could not evaluate the condition — the real run may
    /// not skip at all.
    Skip {
        reason: String,
        decided: bool,
    },
}

/// Decide a `when=`.
///
/// A preview cannot know a value registered earlier in the same run. A condition that needs
/// one is skipped with a reason saying so, rather than decided against the placeholder ""
/// the preview registered — which could report a skip the real run would not make.
fn gate(when: Option<&Condition>, scope: &Scope) -> Result<Gate, String> {
    let Some(cond) = when else {
        return Ok(Gate::Run);
    };
    Ok(match cond.eval(scope)? {
        Outcome::True => Gate::Run,
        Outcome::False => Gate::Skip {
            reason: format!("when: {}", cond.source()),
            decided: true,
        },
        Outcome::Undetermined(var) => Gate::Skip {
            reason: format!(
                "undetermined in preview: when: {} depends on ${{{var}}}, which is not known \
                 until the real run",
                cond.source()
            ),
            decided: false,
        },
    })
}

/// A task's resource as written in the plan, with the same `cmd` fallback as
/// [`NodeRunner::build_params`] but no interpolation — a skipped task is reported without
/// resolving variables its condition may have found undefined.
fn raw_resource(task: &TaskDef) -> String {
    if !task.resource.is_empty() {
        return task.resource.clone();
    }
    match task.args.get("cmd") {
        Some(ParamValue::List(cmds)) => cmds.join(" && "),
        Some(ParamValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}

impl NodeRunner {
    pub async fn run(self) -> NodeResult {
        match self.run_inner().await {
            Ok(result) => result,
            Err(_) => self.finish(false, &Progress::default()),
        }
    }

    /// Report the host as finished. Every exit path goes through here so the counts in the
    /// event and in the result cannot disagree.
    fn finish(&self, success: bool, progress: &Progress) -> NodeResult {
        let _ = self.event_tx.send(ExecutorEvent::NodeComplete {
            host: self.host.name.clone(),
            success,
            changed: progress.changed,
            skipped: progress.skipped,
            dry_run: self.dry_run,
        });
        NodeResult {
            success,
            total_changed: progress.changed,
            total_skipped: progress.skipped,
        }
    }

    /// In a sync run, a permit for one phase of work; in async the engine already holds one
    /// for the whole run.
    async fn sync_permit(&self) -> Option<OwnedSemaphorePermit> {
        match &self.sync {
            Some(sync) => sync.permits.clone().acquire_owned().await.ok(),
            None => None,
        }
    }

    async fn run_inner(&self) -> Result<NodeResult, GlideshError> {
        let connect_permit = self.sync_permit().await;
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

        // Last, so nothing can shadow these — and user var names may not start with `@`
        // anyway, so the reserved namespace cannot be reached from a config file at all.
        vars.extend(host_builtin_vars(&self.host));
        vars.extend(os_builtin_vars(&os_info));

        // Build template data: inventory @-refs + plan structured vars.
        // Preserve inventory-provided collections so plan structured vars
        // cannot overwrite reserved @group.* or @inventory.* namespaces.
        let mut template_data = (*self.inventory_template_data).clone();
        for (key, value) in &self.plan.structured_vars {
            if !template_data.collections.contains_key(key) {
                template_data.collections.insert(key.clone(), value.clone());
            }
        }

        // Decrypt secret tokens once, up front — across flat vars, inventory @-refs, and
        // structured collections — so check/apply (and --dry-run) see plaintext, and every
        // plaintext is registered for redaction before any event is emitted.
        if let Err(e) = self
            .secrets
            .decrypt_vars(&mut vars)
            .and_then(|_| self.secrets.decrypt_template_data(&mut template_data))
        {
            let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                host: self.host.name.clone(),
                module: "secret".to_string(),
                resource: String::new(),
                error: e.to_string(),
            });
            let _ = session.close().await;
            return Ok(self.finish(false, &Progress::default()));
        }

        let steps = self.plan.steps();
        let total_steps = steps.len();
        let mut progress = Progress::default();
        let mut step_changed: HashMap<String, bool> = HashMap::new();
        drop(connect_permit);

        for (step_idx, step) in steps.iter().enumerate() {
            // At the top of the loop so a skipped step, which `continue`s, still arrives.
            // Waiting holds no permit — see `SyncSlot`.
            if let Some(sync) = &self.sync {
                sync.seat.arrive().await;
            }
            let _step_permit = self.sync_permit().await;

            let _ = self.event_tx.send(ExecutorEvent::StepStarted {
                host: self.host.name.clone(),
                step: step.name.clone(),
                step_index: step_idx,
                total_steps,
            });

            let trigger = if step.subscribe.is_empty() {
                Trigger::None
            } else if step
                .subscribe
                .iter()
                .any(|s| step_changed.get(s).copied().unwrap_or(false))
            {
                Trigger::Fired
            } else {
                Trigger::Idle
            };

            // Before the loop is resolved, so a step can guard a loop over a variable that
            // may not exist.
            let scope = Scope {
                vars: &vars,
                collections: &template_data.collections,
                unknown: &progress.unknown,
            };
            // Tags first: a step the run did not select never has its condition evaluated.
            let decision = match self.tags.excludes(&step.tags) {
                Some(reason) => Ok(Gate::Skip {
                    reason,
                    decided: true,
                }),
                None => gate(step.when.as_ref(), &scope),
            };
            match decision {
                Ok(Gate::Run) => {}
                Ok(Gate::Skip { reason, decided }) => {
                    let _ = self.event_tx.send(ExecutorEvent::StepSkipped {
                        host: self.host.name.clone(),
                        step: step.name.clone(),
                        tasks: step.tasks.len(),
                        reason,
                    });
                    progress.skipped += step.tasks.len();
                    for name in step.tasks.iter().filter_map(|t| t.register.as_deref()) {
                        progress.skip_register(&mut vars, name, decided);
                    }
                    step_changed.insert(step.name.clone(), false);
                    continue;
                }
                Err(error) => {
                    self.emit_step_error(&step.name, &error);
                    let _ = session.close().await;
                    return Ok(self.finish(false, &progress));
                }
            }

            if let Some(gate) = &step.until {
                if let Err(error) = self.wait_for_gate(step, gate, &vars, &session).await {
                    self.emit_step_error(&step.name, &error);
                    let _ = session.close().await;
                    return Ok(self.finish(false, &progress));
                }
            }

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
                            &mut progress,
                            trigger,
                        )
                        .await
                    {
                        Ok(changed) => {
                            step_changed.insert(step.name.clone(), changed);
                        }
                        Err(_) => {
                            let _ = session.close().await;
                            return Ok(self.finish(false, &progress));
                        }
                    }
                }
                Some(loop_source) => {
                    let items = match resolve_loop_items(loop_source, &vars, &template_data) {
                        Ok(items) => items,
                        Err(error) => {
                            self.emit_step_error(&step.name, &error);
                            let _ = session.close().await;
                            return Ok(self.finish(false, &progress));
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
                                &mut progress,
                                trigger,
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
                                let _ = session.close().await;
                                return Ok(self.finish(false, &progress));
                            }
                        }
                    }
                    step_changed.insert(step.name.clone(), any_iteration_changed);
                }
            }
        }

        let _ = session.close().await;
        Ok(self.finish(true, &progress))
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

    /// Poll a step's `until=` gate until its command exits 0, or fail once it has not
    /// within the timeout. A preview checks once and never waits. The gate is never a change.
    async fn wait_for_gate(
        &self,
        step: &Step,
        gate: &UntilGate,
        vars: &HashMap<String, String>,
        session: &SshSession,
    ) -> Result<(), String> {
        let mut command = glidesh::config::template::interpolate(&gate.command, vars)
            .map_err(|e| format!("until=: {e}"))?;
        if token::contains_secret_token(&command) {
            command = self
                .secrets
                .decrypt_inline(&command)
                .map_err(|e| format!("until=: {e}"))?;
        }
        let run_as = step
            .run_as
            .clone()
            .merge_over(&self.plan.run_as)
            .merge_over(&self.host.run_as)
            .resolve(glidesh::modules::escalation::password());

        let started = Instant::now();
        // The parser caps the timeout, but a deadline past what `Instant` can hold must not
        // panic; it just never comes.
        let deadline = started.checked_add(Duration::from_secs(gate.timeout));
        let left =
            |now: Instant| deadline.map_or(Duration::MAX, |d| d.saturating_duration_since(now));
        let mut reports = WaitReports {
            started,
            last: None,
        };
        let mut finished: Option<CommandOutput> = None;
        loop {
            // Each attempt is bounded by what is left of the timeout: a command that never
            // exits would otherwise hold the step forever, and one that exits 0 after the
            // deadline must not open the gate late.
            let remaining = left(Instant::now());
            let attempt =
                tokio::time::timeout(remaining, session.exec_as(&command, run_as.as_ref()));
            let result = if self.dry_run {
                attempt.await
            } else {
                self.reporting(step, gate, &mut reports, attempt).await
            };
            let out = match result {
                Ok(result) => result.map_err(|e| format!("until=: {e}"))?,
                Err(_) if self.dry_run => {
                    self.report_preview(step, gate);
                    return Ok(());
                }
                Err(_) => return Err(gate_timeout_error(gate, finished.as_ref(), true)),
            };
            if out.exit_code == 0 {
                return Ok(());
            }
            if self.dry_run {
                self.report_preview(step, gate);
                return Ok(());
            }
            if left(Instant::now()) < Duration::from_secs(gate.interval) {
                return Err(gate_timeout_error(gate, Some(&out), false));
            }
            finished = Some(out);
            if reports.last.is_none() {
                self.report_waiting(step, gate, &mut reports);
            }
            let pause = tokio::time::sleep(Duration::from_secs(gate.interval));
            self.reporting(step, gate, &mut reports, pause).await;
        }
    }

    /// Await `work`, reporting that the gate is still waiting whenever a report falls due —
    /// so neither a slow attempt nor a long `until-interval` goes quiet.
    async fn reporting<F: std::future::Future>(
        &self,
        step: &Step,
        gate: &UntilGate,
        reports: &mut WaitReports,
        work: F,
    ) -> F::Output {
        tokio::pin!(work);
        loop {
            let due = tokio::time::Instant::from_std(reports.next_due());
            tokio::select! {
                out = &mut work => return out,
                _ = tokio::time::sleep_until(due) => self.report_waiting(step, gate, reports),
            }
        }
    }

    fn report_waiting(&self, step: &Step, gate: &UntilGate, reports: &mut WaitReports) {
        self.send_waiting(
            step,
            gate,
            reports.started.elapsed(),
            reports.last.is_none(),
            false,
        );
        reports.last = Some(Instant::now());
    }

    fn report_preview(&self, step: &Step, gate: &UntilGate) {
        self.send_waiting(step, gate, Duration::ZERO, true, true);
    }

    fn send_waiting(
        &self,
        step: &Step,
        gate: &UntilGate,
        elapsed: Duration,
        first: bool,
        preview: bool,
    ) {
        let _ = self.event_tx.send(ExecutorEvent::StepWaiting {
            host: self.host.name.clone(),
            step: step.name.clone(),
            command: gate.command.clone(),
            elapsed_secs: elapsed.as_secs(),
            timeout_secs: gate.timeout,
            first,
            preview,
        });
    }

    fn emit_step_error(&self, step: &str, error: &str) {
        let _ = self.event_tx.send(ExecutorEvent::StepFailed {
            host: self.host.name.clone(),
            step: step.to_string(),
            error: error.to_string(),
        });
    }

    /// Decrypt any `secret:v1:…` tokens written inline in interpolated argument values, in
    /// place — the var-based ones are already plaintext from the up-front sweep. Only values
    /// that actually contain a token are rewritten, so a task with no inline secret (the
    /// common case) pays nothing beyond a substring scan.
    fn decrypt_inline_params(
        &self,
        args: &mut HashMap<String, ParamValue>,
    ) -> Result<(), GlideshError> {
        let decrypt = |s: &mut String| -> Result<(), GlideshError> {
            if token::contains_secret_token(s) {
                *s = self.secrets.decrypt_inline(s)?;
            }
            Ok(())
        };
        for value in args.values_mut() {
            match value {
                ParamValue::String(s) => decrypt(s)?,
                ParamValue::List(list) => list.iter_mut().try_for_each(&decrypt)?,
                ParamValue::Map(map) => map.values_mut().try_for_each(&decrypt)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Interpolate a task's args and resource, decrypt any inline secret tokens, and apply
    /// the empty-resource `cmd` fallback — the assembly shared by [`Self::run_step_tasks`]
    /// and [`Self::run_host_task`]. On failure, emits the task-error event and returns the
    /// `(step name, message)` the task loop propagates.
    fn build_params(
        &self,
        step: &Step,
        task: &TaskDef,
        vars: &HashMap<String, String>,
    ) -> Result<ModuleParams, (String, String)> {
        let fail = |e: GlideshError| -> (String, String) {
            self.emit_task_error(&task.module, &task.resource, &e.to_string());
            (step.name.clone(), e.to_string())
        };

        let mut args = interpolate_args(&task.args, vars).map_err(fail)?;
        self.decrypt_inline_params(&mut args).map_err(fail)?;

        let mut resource_name =
            glidesh::config::template::interpolate(&task.resource, vars).map_err(fail)?;
        if resource_name.is_empty() {
            match args.get("cmd") {
                Some(ParamValue::List(cmds)) => resource_name = cmds.join(" && "),
                Some(ParamValue::String(s)) => resource_name = s.clone(),
                _ => {}
            }
        }
        if token::contains_secret_token(&resource_name) {
            resource_name = self.secrets.decrypt_inline(&resource_name).map_err(fail)?;
        }

        Ok(ModuleParams {
            resource_name,
            args,
        })
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
        progress: &mut Progress,
        trigger: Trigger,
    ) -> Result<bool, (String, String)> {
        let mut any_changed = false;

        for (task_idx, task) in step.tasks.iter().enumerate() {
            // Ahead of the `host` branch, so a host that skips a `host` task never joins its
            // run-once cell; the hosts that do still share the single execution.
            let scope = Scope {
                vars,
                collections: &template_data.collections,
                unknown: &progress.unknown,
            };
            match gate(task.when.as_ref(), &scope) {
                Ok(Gate::Run) => {}
                Ok(Gate::Skip { reason, decided }) => {
                    let _ = self.event_tx.send(ExecutorEvent::TaskSkipped {
                        host: self.host.name.clone(),
                        module: task.module.clone(),
                        resource: raw_resource(task),
                        reason,
                    });
                    progress.skipped += 1;
                    if let Some(name) = &task.register {
                        progress.skip_register(vars, name, decided);
                    }
                    continue;
                }
                Err(error) => {
                    self.emit_task_error(&task.module, &raw_resource(task), &error);
                    return Err((step.name.clone(), error));
                }
            }

            if task.module == host_module::MODULE_NAME {
                let changed = self
                    .run_host_task(step, step_idx, task_idx, loop_iter, task, vars, progress)
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

            let params = self.build_params(step, task, vars)?;

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
                diff: self.diff,
                plan_base_dir: step.base_dir(&self.plan_base_dir),
                run_as,
                secrets: Some(self.secrets.registry()),
                trigger,
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
                ModuleStatus::Satisfied => false,
                ModuleStatus::Pending { .. } => true,
                ModuleStatus::Unknown { .. } => false,
            };

            let pending_plan = match &status {
                ModuleStatus::Pending { plan, diff } => Some((plan.clone(), diff.clone())),
                _ => None,
            };

            if should_apply {
                match module.apply(&ctx, &params).await {
                    Ok(result) => {
                        let changed =
                            resolve_changed(self.dry_run, pending_plan.is_some(), result.changed)
                                && !never_changes(task);
                        if changed {
                            progress.changed += 1;
                            any_changed = true;
                        }
                        if let Some(ref var_name) = task.register {
                            let value = match captured_output(
                                self.dry_run,
                                &result.output,
                                result.output_cut,
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    let _ = self.event_tx.send(ExecutorEvent::ModuleFailed {
                                        host: self.host.name.clone(),
                                        module: task.module.clone(),
                                        resource: params.resource_name.clone(),
                                        error: error.clone(),
                                    });
                                    return Err((step.name.clone(), error));
                                }
                            };
                            progress.register(vars, var_name, value, !self.dry_run);
                        }
                        let stdout = task_output(
                            self.dry_run,
                            self.diff,
                            pending_plan
                                .as_ref()
                                .map(|(plan, diff)| (plan.as_str(), diff.as_deref())),
                            &result.output,
                        );
                        let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            changed,
                            dry_run: self.dry_run,
                            stdout,
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
                        // The real run registers "" here too, so the value is known even
                        // in a preview.
                        if let Some(ref var_name) = task.register {
                            progress.register(vars, var_name, String::new(), true);
                        }
                        let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                            host: self.host.name.clone(),
                            module: task.module.clone(),
                            resource: params.resource_name.clone(),
                            changed: false,
                            dry_run: self.dry_run,
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
        task: &TaskDef,
        vars: &mut HashMap<String, String>,
        progress: &mut Progress,
    ) -> Result<bool, (String, String)> {
        let params = self.build_params(step, task, vars)?;
        let resource_name = params.resource_name.clone();

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

        let result = result.and_then(|out| match &task.register {
            Some(name) => captured_output(self.dry_run, &out.stdout, out.stdout_cut)
                .map(|value| (out, Some((name, value)))),
            None => Ok((out, None)),
        });
        match result {
            Ok((out, registered)) => {
                if let Some((name, value)) = registered {
                    progress.register(vars, name, value, !self.dry_run);
                }
                // A `host` task is a command, not a desired state: it has nothing to
                // compare against, so it always counts — and in a dry run it is always
                // something that *would* run.
                let changed = true;
                progress.changed += 1;
                let _ = self.event_tx.send(ExecutorEvent::ModuleResult {
                    host: self.host.name.clone(),
                    module: task.module.clone(),
                    resource: resource_name,
                    changed,
                    dry_run: self.dry_run,
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
    use glidesh::modules::detect::{ContainerRuntime, InitSystem, OsFamily, PkgManager};

    fn until_gate() -> UntilGate {
        UntilGate {
            command: "curl -sf ${url}".into(),
            timeout: 60,
            interval: 3,
        }
    }

    /// The command as written, not interpolated: the error goes to logs and the terminal.
    #[test]
    fn a_gate_timeout_names_the_command_its_exit_and_its_last_output() {
        let out = CommandOutput {
            exit_code: 7,
            stdout: "partial\n".into(),
            stderr: "connection refused\n".into(),
            stdout_cut: false,
        };
        let err = gate_timeout_error(&until_gate(), Some(&out), false);
        assert!(
            err.starts_with("until= did not succeed within 60s: `curl -sf ${url}` last exited 7"),
            "{err}"
        );
        assert!(
            err.ends_with("last output:\npartial\nconnection refused"),
            "{err}"
        );
    }

    #[test]
    fn a_gate_timeout_keeps_only_the_tail_of_long_output() {
        let stdout: String = (1..=50).map(|n| format!("line {n}\n")).collect();
        let out = CommandOutput {
            exit_code: 1,
            stdout,
            stderr: String::new(),
            stdout_cut: false,
        };
        let err = gate_timeout_error(&until_gate(), Some(&out), false);
        assert!(
            err.contains("line 31\n") && !err.contains("line 30\n"),
            "{err}"
        );
        assert!(err.ends_with("line 50"), "{err}");
    }

    /// A slow first attempt still opens the wait on time; afterwards reports keep a steady
    /// pace from the previous one.
    #[test]
    fn wait_reports_fall_due_from_the_start_then_from_the_last_report() {
        let started = Instant::now();
        let mut reports = WaitReports {
            started,
            last: None,
        };
        assert_eq!(reports.next_due(), started + WAIT_REPORT_EVERY);
        let at = started + Duration::from_secs(5);
        reports.last = Some(at);
        assert_eq!(reports.next_due(), at + WAIT_REPORT_EVERY);
    }

    #[test]
    fn a_gate_still_running_at_the_deadline_says_so() {
        assert_eq!(
            gate_timeout_error(&until_gate(), None, true),
            "until= did not succeed within 60s: `curl -sf ${url}` was still running at the deadline"
        );
    }

    /// A final attempt cut off at the deadline has nothing to show, but the one before it
    /// finished, and its exit and output are still the best clue.
    #[test]
    fn a_hung_last_attempt_keeps_the_previous_attempts_output() {
        let out = CommandOutput {
            exit_code: 7,
            stdout: String::new(),
            stderr: "connection refused\n".into(),
            stdout_cut: false,
        };
        let err = gate_timeout_error(&until_gate(), Some(&out), true);
        assert_eq!(
            err,
            "until= did not succeed within 60s: `curl -sf ${url}` was still running at the \
             deadline; the attempt before exited 7; last output:\nconnection refused"
        );
    }

    #[test]
    fn a_silent_gate_timeout_has_no_output_section() {
        let out = CommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: " \n".into(),
            stdout_cut: false,
        };
        assert!(!gate_timeout_error(&until_gate(), Some(&out), false).contains("last output"));
    }

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
    fn dry_run_counts_pending_not_the_apply_result() {
        let applied_changed = false;
        assert!(resolve_changed(true, true, applied_changed));
        assert!(!resolve_changed(true, false, applied_changed));
    }

    #[test]
    fn real_run_counts_the_apply_result() {
        assert!(resolve_changed(false, true, true));
        assert!(!resolve_changed(false, true, false));
    }

    fn shell_task(changed_when: Option<ParamValue>) -> TaskDef {
        TaskDef {
            module: "shell".into(),
            resource: "lsblk".into(),
            args: changed_when
                .into_iter()
                .map(|v| ("changed-when".to_string(), v))
                .collect(),
            register: None,
            run_as: Default::default(),
            when: None,
        }
    }

    /// Only `#false` is decided here; the command form is the module's to answer.
    #[test]
    fn only_changed_when_false_is_read_by_the_executor() {
        assert!(never_changes(&shell_task(Some(ParamValue::Bool(false)))));
        assert!(!never_changes(&shell_task(Some(ParamValue::Bool(true)))));
        assert!(!never_changes(&shell_task(Some(ParamValue::String(
            "true".into()
        )))));
        assert!(!never_changes(&shell_task(None)));
    }

    #[test]
    fn plan_leads_the_output_and_skips_empty_parts() {
        let pending = Some(("Recreate container web", None));
        assert_eq!(
            task_output(true, false, pending, "[dry-run] docker run ..."),
            "Recreate container web\n[dry-run] docker run ..."
        );
        assert_eq!(
            task_output(true, false, Some(("Upload a -> b", None)), ""),
            "Upload a -> b"
        );
    }

    #[test]
    fn a_real_run_does_not_lead_with_the_plan() {
        assert_eq!(
            task_output(
                false,
                false,
                Some(("Upload a -> b", None)),
                "copied 40 bytes"
            ),
            "copied 40 bytes"
        );
    }

    #[test]
    fn a_diff_is_shown_only_when_the_run_asked_for_one() {
        let pending = Some(("Upload a -> b", Some("-old\n+new")));
        assert_eq!(
            task_output(true, true, pending, "[dry-run] would copy"),
            "Upload a -> b\n-old\n+new\n[dry-run] would copy"
        );
        assert_eq!(
            task_output(true, false, pending, "[dry-run] would copy"),
            "Upload a -> b\n[dry-run] would copy"
        );
    }

    #[test]
    fn a_diff_is_shown_on_a_real_run_too() {
        let pending = Some(("Upload a -> b", Some("-old\n+new")));
        assert_eq!(
            task_output(false, true, pending, "copied 40 bytes"),
            "-old\n+new\ncopied 40 bytes"
        );
        assert_eq!(
            task_output(false, false, pending, "copied 40 bytes"),
            "copied 40 bytes"
        );
    }

    #[test]
    fn a_preview_registers_nothing() {
        assert_eq!(
            captured_output(true, "[dry-run] Would run: lsblk -dn -o NAME", false).unwrap(),
            ""
        );
        assert_eq!(
            captured_output(false, "  sda\nsdb\n", false).unwrap(),
            "sda\nsdb"
        );
    }

    #[test]
    fn output_cut_at_the_limit_cannot_be_registered() {
        let err = captured_output(false, "sda\nsdz", true).unwrap_err();
        assert!(err.contains("too long for register="), "{err}");
        let marker = "sda\n[glidesh: 42 bytes of output dropped here]\nsdz";
        assert_eq!(
            captured_output(false, marker, false).unwrap(),
            marker,
            "only an actual cut is refused, not output that prints the marker"
        );
    }

    #[test]
    fn structured_item_binds_dotted_fields() {
        let row = collection(vec![vec![("name", "vm-a"), ("port", "2301")]])
            .pop()
            .unwrap();
        let mut vars = HashMap::new();
        let injected = inject_loop_item(&mut vars, &LoopItem::Structured(row));

        assert_eq!(vars.get("@item.name").map(String::as_str), Some("vm-a"));
        assert_eq!(vars.get("@item.port").map(String::as_str), Some("2301"));

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
    fn host_builtins_use_at_namespace_only() {
        let host = ResolvedHost {
            name: "web-1".to_string(),
            address: "10.0.0.1".to_string(),
            user: "deploy".to_string(),
            port: 2222,
            vars: HashMap::new(),
            jump: None,
            run_as: Default::default(),
        };
        let vars: HashMap<String, String> = host_builtin_vars(&host).into_iter().collect();
        assert_eq!(vars.get("@host.name").map(String::as_str), Some("web-1"));
        assert_eq!(
            vars.get("@host.address").map(String::as_str),
            Some("10.0.0.1")
        );
        assert_eq!(vars.get("@host.user").map(String::as_str), Some("deploy"));
        assert_eq!(vars.get("@host.port").map(String::as_str), Some("2222"));
        assert!(!vars.contains_key("host.name"));
        assert!(!vars.contains_key("host.port"));
    }

    fn cond(src: &str) -> Condition {
        Condition::parse(src).unwrap()
    }

    fn strings(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn gate_with(
        when: Option<&Condition>,
        vars: &HashMap<String, String>,
        unknown: &[&str],
    ) -> Result<Gate, String> {
        let unknown = unknown.iter().map(|s| s.to_string()).collect();
        gate(
            when,
            &Scope {
                vars,
                collections: &HashMap::new(),
                unknown: &unknown,
            },
        )
    }

    fn skip_reason(gate: Result<Gate, String>) -> (String, bool) {
        match gate {
            Ok(Gate::Skip { reason, decided }) => (reason, decided),
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn no_condition_always_runs() {
        assert_eq!(gate_with(None, &HashMap::new(), &[]), Ok(Gate::Run));
    }

    #[test]
    fn a_false_condition_skips_and_quotes_itself() {
        let vars = strings(&[("@os.family", "debian")]);
        let c = cond("${@os.family} == redhat");
        assert_eq!(
            gate_with(Some(&c), &vars, &[]),
            Ok(Gate::Skip {
                reason: "when: ${@os.family} == redhat".into(),
                decided: true,
            })
        );
        let c = cond("${@os.family} == debian");
        assert_eq!(gate_with(Some(&c), &vars, &[]), Ok(Gate::Run));
    }

    /// The reason is the condition as written: interpolating it would print the value of
    /// every variable it reads, secrets included.
    #[test]
    fn a_skip_reason_never_contains_a_value() {
        let vars = strings(&[("db-password", "hunter2")]);
        let c = cond("${db-password} == ''");
        let (reason, _) = skip_reason(gate_with(Some(&c), &vars, &[]));
        assert!(!reason.contains("hunter2"), "{reason}");
        assert!(reason.contains("${db-password}"), "{reason}");
    }

    #[test]
    fn a_condition_on_a_value_the_preview_cannot_know_says_so() {
        let vars = strings(&[("out", "")]);
        let c = cond("${out} == yes");
        let (reason, decided) = skip_reason(gate_with(Some(&c), &vars, &["out"]));
        assert!(!decided);
        assert!(reason.starts_with("undetermined in preview"), "{reason}");
        assert!(reason.contains("${out}"), "{reason}");
    }

    #[test]
    fn an_undefined_variable_fails_the_gate() {
        let err = gate_with(Some(&cond("${nope} == x")), &HashMap::new(), &[]);
        assert!(err.unwrap_err().contains("nope"));
    }

    #[test]
    fn a_preview_marks_what_it_registers_as_unknown() {
        let mut vars = HashMap::new();
        let mut progress = Progress::default();
        progress.register(&mut vars, "out", String::new(), false);
        assert_eq!(vars.get("out").map(String::as_str), Some(""));
        assert!(progress.unknown.contains("out"));

        // A later real value — a satisfied task registers "" on the real run too — makes it
        // known again.
        progress.register(&mut vars, "out", String::new(), true);
        assert!(!progress.unknown.contains("out"));
    }

    /// Undefined, not empty: `rm -rf /data/${out}` must fail rather than expand to
    /// `rm -rf /data/`.
    #[test]
    fn a_skipped_register_leaves_the_variable_undefined() {
        let mut vars = strings(&[("out", "from an earlier iteration")]);
        let mut progress = Progress::default();
        progress.unknown.insert("out".into());
        progress.skip_register(&mut vars, "out", true);
        assert!(!vars.contains_key("out"));
        assert!(!progress.unknown.contains("out"));
    }

    /// The real run might not skip, and would then define the variable — so a later
    /// `defined ${out}` in the preview must be undetermined, not a confident `false`.
    #[test]
    fn an_undecided_skip_leaves_even_the_presence_unknown() {
        let mut vars = strings(&[("out", "")]);
        let mut progress = Progress::default();
        progress.skip_register(&mut vars, "out", false);
        assert!(!vars.contains_key("out"));
        assert!(progress.unknown.contains("out"));

        let unknown: Vec<&str> = progress.unknown.iter().map(String::as_str).collect();
        let c = cond("defined ${out}");
        let (_, decided) = skip_reason(gate_with(Some(&c), &vars, &unknown));
        assert!(!decided);
    }

    fn task(resource: &str, cmd: Option<ParamValue>) -> TaskDef {
        TaskDef {
            module: "shell".into(),
            resource: resource.into(),
            args: cmd.into_iter().map(|c| ("cmd".to_string(), c)).collect(),
            register: None,
            run_as: Default::default(),
            when: None,
        }
    }

    #[test]
    fn a_skipped_task_is_named_as_written() {
        assert_eq!(raw_resource(&task("${@item}", None)), "${@item}");
        let cmds = ParamValue::List(vec!["a".into(), "b".into()]);
        assert_eq!(raw_resource(&task("", Some(cmds))), "a && b");
        let cmd = ParamValue::String("uptime".into());
        assert_eq!(raw_resource(&task("", Some(cmd))), "uptime");
    }

    fn os_info(family: OsFamily, container_runtime: Option<ContainerRuntime>) -> OsInfo {
        OsInfo {
            id: "ubuntu".to_string(),
            version: "22.04".to_string(),
            family,
            pkg_manager: PkgManager::Apt,
            init_system: InitSystem::Systemd,
            container_runtime,
            nix_installed: false,
        }
    }

    #[test]
    fn os_builtins_expose_detected_facts() {
        let os = os_info(OsFamily::Debian, Some(ContainerRuntime::Podman));
        let vars: HashMap<String, String> = os_builtin_vars(&os).into_iter().collect();
        let get = |k: &str| vars.get(k).map(String::as_str);
        assert_eq!(get("@os.id"), Some("ubuntu"));
        assert_eq!(get("@os.version"), Some("22.04"));
        assert_eq!(get("@os.family"), Some("debian"));
        assert_eq!(get("@os.pkg-manager"), Some("apt"));
        assert_eq!(get("@os.init"), Some("systemd"));
        assert_eq!(get("@os.container-runtime"), Some("podman"));
        assert_eq!(get("@os.nix-installed"), Some("false"));
    }

    /// Referencing an undefined variable fails the task, so a host with no container
    /// runtime must still define the var — as empty — rather than leave it out.
    #[test]
    fn a_missing_container_runtime_is_empty_not_undefined() {
        let os = os_info(OsFamily::Debian, None);
        let vars: HashMap<String, String> = os_builtin_vars(&os).into_iter().collect();
        assert_eq!(
            vars.get("@os.container-runtime").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn os_builtins_resolve_in_a_template() {
        let os = os_info(OsFamily::Unknown("plan9".to_string()), None);
        let vars: HashMap<String, String> = os_builtin_vars(&os).into_iter().collect();
        let out = glidesh::config::template::interpolate(
            "${@os.family}/${@os.pkg-manager}/[${@os.container-runtime}]",
            &vars,
        )
        .unwrap();
        assert_eq!(out, "plan9/apt/[]");
    }

    #[test]
    fn flat_item_binds_canonical() {
        let mut vars = HashMap::new();
        let injected = inject_loop_item(&mut vars, &LoopItem::Flat("sda".to_string()));
        assert_eq!(vars.get("@item").map(String::as_str), Some("sda"));
        assert_eq!(injected, vec!["@item".to_string()]);
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
