use crate::executor::barrier::StepBarrier;
use crate::executor::event_sink::EventSink;
use crate::executor::host_coordinator::HostCoordinator;
use crate::executor::node_runner::{NodeRunner, SyncSlot};
use crate::executor::result::{ExecutorEvent, NodeResult, RunSummary};
use crate::executor::rollout;
use glidesh::config::tags::TagFilter;
use glidesh::config::template::TemplateData;
use glidesh::config::types::{ExecutionMode, Plan, ResolvedHost};
use glidesh::error::GlideshError;
use glidesh::modules::ModuleRegistry;
use glidesh::secrets::Secrets;
use glidesh::ssh::HostKeyPolicy;
use russh_keys::key::PrivateKeyWithHashAlg;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};

pub struct Engine {
    pub plan: Arc<Plan>,
    pub targets: Vec<ResolvedHost>,
    pub registry: Arc<ModuleRegistry>,
    pub key: PrivateKeyWithHashAlg,
    pub concurrency: usize,
    pub dry_run: bool,
    pub diff: bool,
    pub tags: Arc<TagFilter>,
    pub host_key_policy: HostKeyPolicy,
    pub inventory_template_data: Arc<TemplateData>,
    pub plan_base_dir: Arc<PathBuf>,
    pub secrets: Arc<Secrets>,
}

/// What every batch of one run shares.
struct RunShared {
    semaphore: Arc<Semaphore>,
    all_targets: Arc<Vec<ResolvedHost>>,
    /// One for the whole run, not per batch: a `host` task runs once and later batches reuse
    /// its result.
    coordinator: Arc<HostCoordinator>,
    sink: EventSink,
}

impl Engine {
    pub async fn run(
        self,
        event_tx: mpsc::UnboundedSender<ExecutorEvent>,
    ) -> Result<RunSummary, GlideshError> {
        let shared = RunShared {
            semaphore: Arc::new(Semaphore::new(self.concurrency)),
            all_targets: Arc::new(self.targets.clone()),
            coordinator: Arc::new(HostCoordinator::new()),
            sink: EventSink::new(event_tx.clone(), self.secrets.registry()),
        };
        let total = self.targets.len();
        let sizes = rollout::batch_sizes(total, &self.plan.serial);

        let mut hosts = self.targets.iter().cloned();
        let mut results: Vec<NodeResult> = Vec::new();
        let (mut failed, mut aborted) = (0, 0);
        for (index, &size) in sizes.iter().enumerate() {
            let batch: Vec<ResolvedHost> = hosts.by_ref().take(size).collect();
            if sizes.len() > 1 {
                let _ = shared.sink.send(ExecutorEvent::BatchStarted {
                    index,
                    total: sizes.len(),
                    hosts: batch.iter().map(|h| h.name.clone()).collect(),
                });
            }

            let batch_results = self.run_batch(batch, &shared).await;
            let batch_failed = batch_results.iter().filter(|r| !r.success).count();
            failed += batch_failed;
            results.extend(batch_results);

            let is_last = index + 1 == sizes.len();
            if !is_last {
                if let Some(reason) =
                    rollout::stop_reason(self.plan.max_fail, failed, total, size, batch_failed)
                {
                    let rest: Vec<String> = hosts.map(|h| h.name).collect();
                    aborted = rest.len();
                    let _ = shared.sink.send(ExecutorEvent::HostsAborted {
                        hosts: rest,
                        reason,
                    });
                    break;
                }
            }
        }

        let summary = RunSummary {
            total_hosts: total,
            succeeded: results.iter().filter(|r| r.success).count(),
            failed: results.iter().filter(|r| !r.success).count(),
            total_changed: results.iter().map(|r| r.total_changed).sum(),
            total_skipped: results.iter().map(|r| r.total_skipped).sum(),
            aborted,
            dry_run: self.dry_run,
        };

        let _ = event_tx.send(ExecutorEvent::RunComplete {
            summary: summary.clone(),
        });

        Ok(summary)
    }

    /// Run one batch to completion. In sync mode the batch has its own barrier, so its hosts
    /// move through the steps together.
    async fn run_batch(&self, batch: Vec<ResolvedHost>, shared: &RunShared) -> Vec<NodeResult> {
        let barrier =
            (self.plan.mode == ExecutionMode::Sync).then(|| StepBarrier::new(batch.len()));

        let mut handles = Vec::new();
        for host in batch {
            let name = host.name.clone();
            let sync = barrier.as_ref().map(|b| SyncSlot {
                seat: b.seat(),
                permits: shared.semaphore.clone(),
            });
            let sem = shared.semaphore.clone();
            let fp = self.plan.clone();
            let reg = self.registry.clone();
            let key = self.key.clone();
            let dry_run = self.dry_run;
            let diff = self.diff;
            let tags = self.tags.clone();
            let host_key_policy = self.host_key_policy;
            let tx = shared.sink.clone();
            let inv = self.inventory_template_data.clone();
            let base_dir = self.plan_base_dir.clone();
            let coord = shared.coordinator.clone();
            let targets = shared.all_targets.clone();
            let secrets = self.secrets.clone();

            let handle = tokio::spawn(async move {
                // A sync host takes permits per phase instead — see `SyncSlot`.
                let _permit = match sync {
                    Some(_) => None,
                    None => Some(sem.acquire().await.expect("semaphore closed")),
                };
                let runner = NodeRunner {
                    host,
                    plan: fp,
                    registry: reg,
                    key,
                    dry_run,
                    diff,
                    tags,
                    host_key_policy,
                    event_tx: tx,
                    inventory_template_data: inv,
                    plan_base_dir: base_dir,
                    coordinator: coord,
                    all_targets: targets,
                    secrets,
                    sync,
                };
                runner.run().await
            });

            handles.push((name, handle));
        }

        let mut results = Vec::new();
        for (host, handle) in handles {
            match handle.await {
                Ok(result) => results.push(result),
                // The runner never reached its own completion, so report it here: otherwise the
                // TUI leaves the host RUNNING and the run log never records it as failed.
                Err(e) => {
                    tracing::error!("Task for {} panicked: {}", host, e);
                    let _ = shared.sink.send(ExecutorEvent::ModuleFailed {
                        host: host.clone(),
                        module: "glidesh".to_string(),
                        resource: String::new(),
                        error: format!("internal error: {e}"),
                    });
                    let _ = shared.sink.send(ExecutorEvent::NodeComplete {
                        host,
                        success: false,
                        changed: 0,
                        skipped: 0,
                        dry_run: self.dry_run,
                    });
                    results.push(NodeResult {
                        success: false,
                        total_changed: 0,
                        total_skipped: 0,
                    });
                }
            }
        }
        results
    }
}

/// A group-plan pair for multi-plan execution.
pub struct GroupPlan {
    pub plan: Arc<Plan>,
    pub targets: Vec<ResolvedHost>,
    pub inventory_template_data: Arc<TemplateData>,
    pub plan_base_dir: Arc<PathBuf>,
}

/// Run multiple group-plan pairs concurrently. Each group's hosts execute
/// their plan with sync semantics within the group, but groups are fully
/// independent of each other.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    group_plans: Vec<GroupPlan>,
    registry: Arc<ModuleRegistry>,
    key: PrivateKeyWithHashAlg,
    concurrency: usize,
    dry_run: bool,
    diff: bool,
    tags: Arc<TagFilter>,
    host_key_policy: HostKeyPolicy,
    secrets: Arc<Secrets>,
    event_tx: mpsc::UnboundedSender<ExecutorEvent>,
) -> Result<RunSummary, GlideshError> {
    let mut group_handles = Vec::new();

    for gp in group_plans {
        let reg = registry.clone();
        let k = key.clone();
        let tx = event_tx.clone();
        let secrets = secrets.clone();
        let tags = tags.clone();

        let handle = tokio::spawn(async move {
            let engine = Engine {
                plan: gp.plan,
                targets: gp.targets,
                registry: reg,
                key: k,
                concurrency,
                dry_run,
                diff,
                tags,
                host_key_policy,
                inventory_template_data: gp.inventory_template_data,
                plan_base_dir: gp.plan_base_dir,
                secrets,
            };
            // Use a local channel so RunComplete events don't fire per-group.
            // Instead, forward all events except RunComplete to the parent.
            let (local_tx, mut local_rx) = mpsc::unbounded_channel();
            let forwarder = {
                let tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(event) = local_rx.recv().await {
                        if matches!(&event, ExecutorEvent::RunComplete { .. }) {
                            continue; // suppress per-group RunComplete
                        }
                        let _ = tx.send(event);
                    }
                })
            };

            let result = engine.run(local_tx).await;
            let _ = forwarder.await;
            result
        });

        group_handles.push(handle);
    }

    let mut total_hosts = 0;
    let mut total_succeeded = 0;
    let mut total_failed = 0;
    let mut total_changed = 0;
    let mut total_skipped = 0;
    let mut total_aborted = 0;

    for handle in group_handles {
        match handle.await {
            Ok(Ok(summary)) => {
                total_hosts += summary.total_hosts;
                total_succeeded += summary.succeeded;
                total_failed += summary.failed;
                total_changed += summary.total_changed;
                total_skipped += summary.total_skipped;
                total_aborted += summary.aborted;
            }
            Ok(Err(e)) => {
                tracing::error!("Group execution failed: {}", e);
                return Err(e);
            }
            Err(e) => {
                tracing::error!("Group task panicked: {}", e);
            }
        }
    }

    let summary = RunSummary {
        total_hosts,
        succeeded: total_succeeded,
        failed: total_failed,
        total_changed,
        total_skipped,
        aborted: total_aborted,
        dry_run,
    };

    let _ = event_tx.send(ExecutorEvent::RunComplete {
        summary: summary.clone(),
    });

    Ok(summary)
}
