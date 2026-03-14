mod cli;
mod executor;
mod logging;
mod tui;

use clap::Parser;
use cli::{Cli, Commands};
use executor::result::ExecutorEvent;
use glidesh::config;
use glidesh::config::types::ExecutionMode;
use glidesh::error::GlideshError;
use glidesh::modules::ModuleRegistry;
use glidesh::ssh::{HostKeyPolicy, SshSession};
use logging::RunLogger;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("glidesh=info")),
        )
        .init();

    match cli.command {
        Commands::Run(args) => cmd_run(args).await?,
        Commands::Logs(args) => cmd_logs(args)?,
        Commands::Validate(args) => cmd_validate(args)?,
    }

    Ok(())
}

async fn cmd_run(args: cli::RunArgs) -> Result<(), GlideshError> {
    if let (Some(host), Some(command)) = (&args.host, &args.command) {
        let user = args.user.as_deref().unwrap_or("root");
        let key_path = expand_tilde(&args.key.clone().unwrap_or_else(default_ssh_key));

        tracing::info!("Connecting to {}@{}:{}", user, host, args.port);
        tracing::debug!("Using SSH key: {}", key_path.display());

        let key_pair = russh_keys::load_secret_key(&key_path, None)?;
        let hash_alg = match key_pair.algorithm() {
            ssh_key::Algorithm::Rsa { .. } => Some(ssh_key::HashAlg::Sha256),
            _ => None,
        };
        let key = russh_keys::key::PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash_alg)?;

        let host_key_policy = HostKeyPolicy {
            verify: !args.no_host_key_check,
            accept_new: args.accept_new_host_key,
        };
        let session = SshSession::connect(host, args.port, user, &key, host_key_policy).await?;
        tracing::info!("Connected. Running command: {}", command);

        let output = session.exec(command).await?;

        if !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }

        let exit_code = output.exit_code;
        session.close().await?;

        if exit_code != 0 {
            return Err(GlideshError::SshCommand {
                exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            });
        }

        return Ok(());
    }

    let inventory = if let Some(ref inv_path) = args.inventory {
        let inv_content = std::fs::read_to_string(inv_path).map_err(|e| {
            GlideshError::Other(format!(
                "Failed to read inventory '{}': {}",
                inv_path.display(),
                e
            ))
        })?;
        Some(config::parse_inventory(&inv_content)?)
    } else {
        None
    };

    let inv_base_dir = args
        .inventory
        .as_ref()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| std::path::Path::new("."));

    let mut group_plans = Vec::new();
    let mut all_host_names: Vec<(String, String, String)> = Vec::new();
    let mut run_name_parts = Vec::new();

    if let Some(fp_path) = &args.plan {
        let fp_content = std::fs::read_to_string(fp_path).map_err(|e| {
            GlideshError::Other(format!(
                "Failed to read plan '{}': {}",
                fp_path.display(),
                e
            ))
        })?;
        let mut plan = config::parse_plan(&fp_content)?;
        let plan_base_dir = fp_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        config::resolve_includes(&mut plan, plan_base_dir)?;

        if args.mode == "async" {
            plan.mode = ExecutionMode::Async;
        }

        let targets = if let Some(ref host) = args.host {
            let user = args.user.as_deref().unwrap_or("root").to_string();
            vec![config::types::ResolvedHost {
                name: host.clone(),
                address: host.clone(),
                user,
                port: args.port,
                vars: plan.vars.clone(),
            }]
        } else if let Some(ref inventory) = inventory {
            let target_filter = args.target.as_deref();
            let resolved = inventory.resolve_targets(target_filter);
            if resolved.is_empty() {
                return Err(GlideshError::NoTargets);
            }
            resolved
        } else {
            return Err(GlideshError::Other(
                "Plan mode requires --inventory or --host".to_string(),
            ));
        };

        let pn = plan.name.clone();
        run_name_parts.push(pn.clone());
        all_host_names.extend(
            targets
                .iter()
                .map(|h| (h.name.clone(), String::new(), pn.clone())),
        );

        group_plans.push(executor::GroupPlan {
            plan: Arc::new(plan),
            targets,
        });
    } else if let Some(ref inventory) = inventory {
        let group_plans_raw = inventory.resolve_group_plans();
        if group_plans_raw.is_empty() {
            return Err(GlideshError::Other(
                "No --plan provided and no groups have a plan= attribute in the inventory"
                    .to_string(),
            ));
        }

        // Parse --target filter:
        //   "name"       — matches a group or ungrouped host by name
        //   "group:host" — matches a specific host within a specific group
        let (filter_group, filter_host) = match args.target.as_deref() {
            Some(t) if t.contains(':') => {
                let mut parts = t.splitn(2, ':');
                (
                    Some(parts.next().unwrap().to_string()),
                    Some(parts.next().unwrap().to_string()),
                )
            }
            Some(t) => (Some(t.to_string()), None),
            None => (None, None),
        };

        for (group_name, plan_path, targets) in &group_plans_raw {
            if let Some(ref fg) = filter_group {
                if filter_host.is_some() {
                    // "group:host" — group must match exactly
                    if fg != group_name {
                        continue;
                    }
                } else if !group_name.is_empty() {
                    // Grouped entry — plain name must match group name
                    if fg != group_name {
                        continue;
                    }
                } else {
                    // Ungrouped host — plain name must match host name
                    let has_host = targets.iter().any(|h| h.name == *fg);
                    if !has_host {
                        continue;
                    }
                }
            }

            let filtered_targets: Vec<_> = targets
                .iter()
                .filter(|h| {
                    if let Some(ref fh) = filter_host {
                        h.name == *fh
                    } else if let Some(ref fg) = filter_group {
                        // Ungrouped: only keep the host matching the filter
                        !group_name.is_empty() || h.name == *fg
                    } else {
                        true // group matched or no filter, keep all hosts
                    }
                })
                .cloned()
                .collect();

            if filtered_targets.is_empty() {
                continue;
            }

            let resolved_path = if std::path::Path::new(plan_path).is_absolute() {
                PathBuf::from(plan_path)
            } else {
                inv_base_dir.join(plan_path)
            };

            let fp_content = std::fs::read_to_string(&resolved_path).map_err(|e| {
                GlideshError::Other(format!(
                    "Failed to read plan '{}' for group '{}': {}",
                    resolved_path.display(),
                    group_name,
                    e
                ))
            })?;
            let mut plan = config::parse_plan(&fp_content)?;
            let include_base = resolved_path.parent().unwrap_or(inv_base_dir);
            config::resolve_includes(&mut plan, include_base)?;

            if args.mode == "async" {
                plan.mode = ExecutionMode::Async;
            }

            let pn = plan.name.clone();
            let gn = group_name.clone();
            run_name_parts.push(format!("{}-{}", gn, pn));
            all_host_names.extend(
                filtered_targets
                    .iter()
                    .map(|h| (h.name.clone(), gn.clone(), pn.clone())),
            );

            group_plans.push(executor::GroupPlan {
                plan: Arc::new(plan),
                targets: filtered_targets,
            });
        }
    } else {
        return Err(GlideshError::Other(
            "Please provide either --host + --command for ad-hoc mode, or --plan/--inventory for plan mode"
                .to_string(),
        ));
    }

    if group_plans.is_empty() {
        return Err(GlideshError::NoTargets);
    }

    let all_targets: Vec<&config::types::ResolvedHost> =
        group_plans.iter().flat_map(|gp| &gp.targets).collect();
    let key = load_ssh_key(&args, &all_targets)?;
    let registry = Arc::new(ModuleRegistry::with_external(Some(inv_base_dir)));

    for gp in &group_plans {
        registry.validate_plan(&gp.plan)?;
    }

    let run_name = run_name_parts.join("+");

    tracing::info!(
        "{} group(s), {} total host(s)",
        group_plans.len(),
        all_host_names.len()
    );

    run_with_ui(
        group_plans,
        registry,
        key,
        &run_name,
        &all_host_names,
        &args,
    )
    .await
}

fn display_id(host: &str, display_ids: &std::collections::HashMap<String, String>) -> String {
    display_ids
        .get(host)
        .cloned()
        .unwrap_or_else(|| host.to_string())
}

fn print_event(event: &ExecutorEvent, display_ids: &std::collections::HashMap<String, String>) {
    match event {
        ExecutorEvent::NodeConnecting { host } => {
            println!("[{}] Connecting...", display_id(host, display_ids))
        }
        ExecutorEvent::NodeConnected { host, os } => {
            println!("[{}] Connected ({})", display_id(host, display_ids), os.id)
        }
        ExecutorEvent::NodeAuthFailed { host, error } => {
            eprintln!("[{}] Auth failed: {}", display_id(host, display_ids), error)
        }
        ExecutorEvent::StepStarted {
            host,
            step,
            step_index,
            total_steps,
        } => {
            println!(
                "[{}] Step {}/{}: {}",
                display_id(host, display_ids),
                step_index + 1,
                total_steps,
                step
            );
        }
        ExecutorEvent::ModuleCheck {
            host,
            module,
            resource,
        } => {
            println!(
                "[{}]   Checking {} '{}'",
                display_id(host, display_ids),
                module,
                resource
            );
        }
        ExecutorEvent::ModuleResult {
            host,
            module,
            resource,
            changed,
        } => {
            let status = if *changed { "changed" } else { "ok" };
            println!(
                "[{}]   {} '{}': {}",
                display_id(host, display_ids),
                module,
                resource,
                status
            );
        }
        ExecutorEvent::ModuleFailed {
            host,
            module,
            resource,
            error,
        } => {
            eprintln!(
                "[{}]   FAILED {} '{}': {}",
                display_id(host, display_ids),
                module,
                resource,
                error
            );
        }
        ExecutorEvent::NodeComplete {
            host,
            success,
            changed,
        } => {
            let status = if *success { "OK" } else { "FAILED" };
            println!(
                "[{}] {} ({} changed)",
                display_id(host, display_ids),
                status,
                changed
            );
        }
        ExecutorEvent::RunComplete { summary } => {
            println!("\n--- Run Complete ---");
            println!(
                "Hosts: {} total, {} ok, {} failed, {} changed",
                summary.total_hosts, summary.succeeded, summary.failed, summary.total_changed
            );
        }
    }
}

fn cmd_logs(args: cli::LogsArgs) -> Result<(), GlideshError> {
    let runs = logging::storage::list_runs()?;

    if runs.is_empty() {
        println!("No runs found.");
        return Ok(());
    }

    if let Some(ref run_name) = args.run {
        let run_dir = runs
            .iter()
            .find(|p| {
                p.file_name()
                    .map(|n| n.to_string_lossy().contains(run_name))
                    .unwrap_or(false)
            })
            .ok_or_else(|| GlideshError::Other(format!("Run '{}' not found", run_name)))?;

        return show_run_details(run_dir, args.node.as_deref());
    }

    if args.last {
        let last_run = &runs[0];
        return show_run_details(last_run, args.node.as_deref());
    }

    if tui::is_tty() {
        tui::run_logs_tui(runs).map_err(|e| GlideshError::Other(format!("TUI error: {}", e)))?;
        return Ok(());
    }

    println!("Recent runs:");
    for run_dir in runs.iter().take(20) {
        let name = run_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if let Ok(summary) = logging::storage::read_summary(run_dir) {
            let node_count = summary.nodes.len();
            let ok = summary.nodes.values().filter(|n| n.status == "ok").count();
            let failed = summary
                .nodes
                .values()
                .filter(|n| n.status == "failed")
                .count();
            println!(
                "  {}  ({} nodes: {} ok, {} failed)",
                name, node_count, ok, failed
            );
        } else {
            println!("  {}  (no summary)", name);
        }
    }

    Ok(())
}

fn show_run_details(
    run_dir: &std::path::Path,
    node_filter: Option<&str>,
) -> Result<(), GlideshError> {
    if let Some(node) = node_filter {
        match logging::storage::read_node_log(run_dir, node) {
            Ok(content) => {
                println!("{}", content);
            }
            Err(_) => {
                println!("No log found for node '{}'", node);
            }
        }
        return Ok(());
    }

    match logging::storage::read_summary(run_dir) {
        Ok(summary) => {
            println!("Run: {} ({})", summary.plan, summary.run_id);
            println!("Started: {}", summary.started_at);
            if let Some(finished) = summary.finished_at {
                println!("Finished: {}", finished);
            }
            println!("\nNodes:");
            for (host, node) in &summary.nodes {
                print!("  {} — {}", host, node.status);
                if node.changed > 0 {
                    print!(" ({} changed)", node.changed);
                }
                if let Some(ref err) = node.error {
                    print!(" [error: {}]", err);
                }
                println!();
            }
        }
        Err(_) => {
            println!("No summary found for this run.");
        }
    }

    Ok(())
}

fn cmd_validate(args: cli::ValidateArgs) -> Result<(), GlideshError> {
    let mut valid = true;

    if let Some(ref fp_path) = args.plan {
        print!("Validating plan '{}'... ", fp_path.display());
        match std::fs::read_to_string(fp_path) {
            Ok(content) => match config::parse_plan(&content) {
                Ok(fp) => {
                    println!("OK ({} steps)", fp.steps().len());
                }
                Err(e) => {
                    println!("FAILED: {}", e);
                    valid = false;
                }
            },
            Err(e) => {
                println!("FAILED: {}", e);
                valid = false;
            }
        }
    }

    if let Some(ref inv_path) = args.inventory {
        print!("Validating inventory '{}'... ", inv_path.display());
        match std::fs::read_to_string(inv_path) {
            Ok(content) => match config::parse_inventory(&content) {
                Ok(h) => {
                    let total_hosts: usize = h.groups.iter().map(|g| g.hosts.len()).sum::<usize>()
                        + h.ungrouped_hosts.len();
                    println!("OK ({} groups, {} hosts)", h.groups.len(), total_hosts);
                }
                Err(e) => {
                    println!("FAILED: {}", e);
                    valid = false;
                }
            },
            Err(e) => {
                println!("FAILED: {}", e);
                valid = false;
            }
        }
    }

    if args.plan.is_none() && args.inventory.is_none() {
        println!("No files specified. Use --plan and/or --inventory.");
    }

    if valid {
        Ok(())
    } else {
        Err(GlideshError::Other("Validation failed".to_string()))
    }
}

fn load_ssh_key(
    args: &cli::RunArgs,
    targets: &[&config::types::ResolvedHost],
) -> Result<russh_keys::key::PrivateKeyWithHashAlg, GlideshError> {
    let key_path = if let Some(ref k) = args.key {
        expand_tilde(k)
    } else if let Some(inv_key) = targets.first().and_then(|h| h.vars.get("ssh-key")) {
        expand_tilde(&PathBuf::from(inv_key))
    } else {
        expand_tilde(&default_ssh_key())
    };
    tracing::debug!("Using SSH key: {}", key_path.display());
    let key_pair = russh_keys::load_secret_key(&key_path, None)?;
    let hash_alg = match key_pair.algorithm() {
        ssh_key::Algorithm::Rsa { .. } => Some(ssh_key::HashAlg::Sha256),
        _ => None,
    };
    Ok(russh_keys::key::PrivateKeyWithHashAlg::new(
        Arc::new(key_pair),
        hash_alg,
    )?)
}

async fn run_with_ui(
    group_plans: Vec<executor::GroupPlan>,
    registry: Arc<ModuleRegistry>,
    key: russh_keys::key::PrivateKeyWithHashAlg,
    run_name: &str,
    host_names: &[(String, String, String)],
    args: &cli::RunArgs,
) -> Result<(), GlideshError> {
    let display_ids: std::collections::HashMap<String, String> = host_names
        .iter()
        .map(|(host, group, _plan)| {
            let id = if group.is_empty() {
                host.clone()
            } else {
                format!("{}:{}", group, host)
            };
            (host.clone(), id)
        })
        .collect();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

    let mut logger = RunLogger::new(run_name)?;
    println!("Logging to: {}", logger.run_dir().display());

    let concurrency = args.concurrency;
    let dry_run = args.dry_run;
    let host_key_policy = HostKeyPolicy {
        verify: !args.no_host_key_check,
        accept_new: args.accept_new_host_key,
    };

    if tui::is_tty() && !args.no_tui {
        let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel();
        let log_consumer = tokio::spawn(async move {
            while let Some(event) = log_rx.recv().await {
                logger.handle_event(&event);
                if matches!(&event, ExecutorEvent::RunComplete { .. }) {
                    let _ = logger.write_summary();
                }
            }
        });

        let (tui_tx, tui_rx) = tokio::sync::mpsc::unbounded_channel();
        let (combined_tx, mut combined_rx) =
            tokio::sync::mpsc::unbounded_channel::<ExecutorEvent>();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = combined_rx.recv().await {
                let _ = log_tx.send(event.clone());
                let _ = tui_tx.send(event);
            }
        });

        let run_name_owned = run_name.to_string();
        let host_names_owned = host_names.to_vec();
        let engine_handle = tokio::spawn(async move {
            executor::run(
                group_plans,
                registry,
                key,
                concurrency,
                dry_run,
                host_key_policy,
                combined_tx,
            )
            .await
        });

        let abort_handle = engine_handle.abort_handle();
        let aborted = tui::run_tui(tui_rx, &run_name_owned, &host_names_owned, abort_handle)
            .await
            .map_err(|e| GlideshError::Other(format!("TUI error: {}", e)))?;

        if aborted {
            engine_handle.abort();
            forwarder.abort();
            log_consumer.abort();
            return Err(GlideshError::Other("Aborted by user".to_string()));
        }

        let engine_result = engine_handle.await;
        let _ = forwarder.await;
        let _ = log_consumer.await;

        if let Ok(Ok(summary)) = engine_result {
            if summary.failed > 0 {
                return Err(GlideshError::Executor {
                    message: format!("{} host(s) failed", summary.failed),
                });
            }
        }
    } else {
        let consumer = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                logger.handle_event(&event);
                print_event(&event, &display_ids);
                if matches!(&event, ExecutorEvent::RunComplete { .. }) {
                    let _ = logger.write_summary();
                }
            }
        });

        let summary = executor::run(
            group_plans,
            registry,
            key,
            concurrency,
            dry_run,
            host_key_policy,
            event_tx,
        )
        .await?;
        let _ = consumer.await;

        if summary.failed > 0 {
            return Err(GlideshError::Executor {
                message: format!("{} host(s) failed", summary.failed),
            });
        }
    }

    Ok(())
}

fn default_ssh_key() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("id_ed25519")
}

/// Expand a leading `~` or `~/` to the user's home directory.
/// On Windows, shells don't expand `~` so we handle it ourselves.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    } else if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(rest)
    } else {
        path.to_path_buf()
    }
}
