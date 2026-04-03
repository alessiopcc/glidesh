use crate::executor::node_runner::NodeRunner;
use crate::executor::result::{ExecutorEvent, NodeResult, RunSummary};
use glidesh::config::template::TemplateData;
use glidesh::config::types::{Plan, ResolvedHost};
use glidesh::error::GlideshError;
use glidesh::modules::ModuleRegistry;
use glidesh::ssh::HostKeyPolicy;
use russh_keys::key::PrivateKeyWithHashAlg;
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};

pub struct Engine {
    pub plan: Arc<Plan>,
    pub targets: Vec<ResolvedHost>,
    pub registry: Arc<ModuleRegistry>,
    pub key: PrivateKeyWithHashAlg,
    pub concurrency: usize,
    pub dry_run: bool,
    pub host_key_policy: HostKeyPolicy,
    pub inventory_template_data: Arc<TemplateData>,
}

impl Engine {
    pub async fn run(
        self,
        event_tx: mpsc::UnboundedSender<ExecutorEvent>,
    ) -> Result<RunSummary, GlideshError> {
        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let plan = self.plan.clone();
        let registry = self.registry.clone();

        let mut handles = Vec::new();

        let inv_data = self.inventory_template_data.clone();

        for host in self.targets {
            let sem = semaphore.clone();
            let fp = plan.clone();
            let reg = registry.clone();
            let key = self.key.clone();
            let dry_run = self.dry_run;
            let host_key_policy = self.host_key_policy;
            let tx = event_tx.clone();
            let inv = inv_data.clone();

            let handle = tokio::spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                let runner = NodeRunner {
                    host,
                    plan: fp,
                    registry: reg,
                    key,
                    dry_run,
                    host_key_policy,
                    event_tx: tx,
                    inventory_template_data: inv,
                };
                runner.run().await
            });

            handles.push(handle);
        }

        let mut results: Vec<NodeResult> = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    tracing::error!("Task panicked: {}", e);
                }
            }
        }

        let summary = RunSummary {
            total_hosts: results.len(),
            succeeded: results.iter().filter(|r| r.success).count(),
            failed: results.iter().filter(|r| !r.success).count(),
            total_changed: results.iter().map(|r| r.total_changed).sum(),
        };

        let _ = event_tx.send(ExecutorEvent::RunComplete {
            summary: summary.clone(),
        });

        Ok(summary)
    }
}

/// A group-plan pair for multi-plan execution.
pub struct GroupPlan {
    pub plan: Arc<Plan>,
    pub targets: Vec<ResolvedHost>,
    pub inventory_template_data: Arc<TemplateData>,
}

/// Run multiple group-plan pairs concurrently. Each group's hosts execute
/// their plan with sync semantics within the group, but groups are fully
/// independent of each other.
pub async fn run(
    group_plans: Vec<GroupPlan>,
    registry: Arc<ModuleRegistry>,
    key: PrivateKeyWithHashAlg,
    concurrency: usize,
    dry_run: bool,
    host_key_policy: HostKeyPolicy,
    event_tx: mpsc::UnboundedSender<ExecutorEvent>,
) -> Result<RunSummary, GlideshError> {
    let mut group_handles = Vec::new();

    for gp in group_plans {
        let reg = registry.clone();
        let k = key.clone();
        let tx = event_tx.clone();

        let handle = tokio::spawn(async move {
            let engine = Engine {
                plan: gp.plan,
                targets: gp.targets,
                registry: reg,
                key: k,
                concurrency,
                dry_run,
                host_key_policy,
                inventory_template_data: gp.inventory_template_data,
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

    for handle in group_handles {
        match handle.await {
            Ok(Ok(summary)) => {
                total_hosts += summary.total_hosts;
                total_succeeded += summary.succeeded;
                total_failed += summary.failed;
                total_changed += summary.total_changed;
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
    };

    let _ = event_tx.send(ExecutorEvent::RunComplete {
        summary: summary.clone(),
    });

    Ok(summary)
}
