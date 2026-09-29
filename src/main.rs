mod cli;
mod executor;
mod logging;
mod secret_cmd;
mod tui;

use clap::Parser;
use cli::{Cli, Commands};
use executor::result::ExecutorEvent;
use glidesh::config;
use glidesh::config::tags::TagFilter;
use glidesh::config::template::TemplateData;
use glidesh::config::types::{ExecutionMode, Inventory, RunAsMethod, RunAsSpec, RunAsUser};
use glidesh::error::GlideshError;
use glidesh::modules::ModuleRegistry;
use glidesh::secrets::{
    config as secrets_config, passphrase as secret_passphrase, store as secret_store,
};
use glidesh::ssh::{HostKeyPolicy, SshSession};
use logging::RunLogger;
use secret_cmd::{
    cmd_secret, read_pass_file, secret_identity_path, secret_pass_file_from_env,
    secret_pass_from_env,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    let console_tui_mode = match &cli.command {
        None => true,
        Some(Commands::Console(a)) => a.target.is_none() && a.command.is_none(),
        _ => false,
    };

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("glidesh=info"));

    if console_tui_mode {
        let log_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".glidesh");
        let _ = std::fs::create_dir_all(&log_dir);
        let appender = tracing_appender::rolling::never(&log_dir, "console.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        // Keep the guard alive for the lifetime of the process so buffered
        // log lines actually flush.
        std::mem::forget(guard);
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match cli.command {
        Some(Commands::Run(args)) => cmd_run(args).await?,
        Some(Commands::Logs(args)) => cmd_logs(args)?,
        Some(Commands::Validate(args)) => cmd_validate(args)?,
        Some(Commands::Console(args)) => cmd_console(args).await?,
        Some(Commands::Secret(args)) => cmd_secret(args)?,
        None => cmd_console(cli::ConsoleArgs::default()).await?,
    }

    Ok(())
}

/// Build the CLI-level escalation spec from `--run-as` / `--run-as-method`.
fn build_cli_run_as(args: &cli::RunArgs) -> Result<RunAsSpec, GlideshError> {
    let user = args.run_as.as_ref().map(|u| {
        if u.is_empty() {
            RunAsUser::Disabled
        } else {
            RunAsUser::User(u.clone())
        }
    });
    let method = match &args.run_as_method {
        Some(s) => Some(RunAsMethod::parse(s).ok_or_else(|| {
            GlideshError::Other(format!(
                "Unknown --run-as-method '{}' (expected sudo, doas, or su)",
                s
            ))
        })?),
        None => None,
    };
    Ok(RunAsSpec { user, method })
}

/// Source the escalation password: `GLIDESH_RUNAS_PASS` first, then `--ask-pass`.
fn source_run_as_password(args: &cli::RunArgs) -> Result<Option<String>, GlideshError> {
    if let Ok(p) = std::env::var("GLIDESH_RUNAS_PASS") {
        if !p.is_empty() {
            return Ok(Some(p));
        }
    }
    if args.ask_pass {
        let p = rpassword::prompt_password("run-as password: ")
            .map_err(|e| GlideshError::Other(format!("Failed to read password: {}", e)))?;
        return Ok(Some(p));
    }
    Ok(None)
}

/// The error a finished run exits with, if any. Hosts a stopped rollout never started count
/// against it as much as failed ones: the change did not reach them.
fn run_failure(summary: &executor::result::RunSummary) -> Option<GlideshError> {
    if summary.failed == 0 && summary.aborted == 0 {
        return None;
    }
    let mut message = format!("{} host(s) failed", summary.failed);
    if summary.aborted > 0 {
        message.push_str(&format!(", {} not started", summary.aborted));
    }
    Some(GlideshError::Executor { message })
}

/// `--serial` and `--max-fail`, when given, override the plan's own settings — so one plan can
/// be rolled out more cautiously, say `--serial 1 --max-fail 0`, without editing it.
///
/// A bad value is a command-line error, not a plan one, so it is reported without the
/// plan-parse wording.
fn apply_rollout_override(
    plan: &mut glidesh::config::types::Plan,
    serial: Option<&str>,
    max_fail: Option<&str>,
) -> Result<(), GlideshError> {
    let as_flag_error = |e: GlideshError| match e {
        GlideshError::ConfigParse { message } => GlideshError::Other(message),
        other => other,
    };
    if let Some(sizes) = serial {
        plan.serial = sizes
            .split(',')
            .map(|s| config::plan::parse_amount_text(s, "--serial", 1))
            .collect::<Result<_, _>>()
            .map_err(as_flag_error)?;
    }
    if let Some(limit) = max_fail {
        plan.max_fail =
            Some(config::plan::parse_amount_text(limit, "--max-fail", 0).map_err(as_flag_error)?);
    }
    Ok(())
}

/// A plan's directory as an absolute path, which a module resolves local paths from and
/// reports them by. A bare `plan.kdl` has `""` as its parent, the current directory.
fn absolute_dir(dir: &std::path::Path) -> std::path::PathBuf {
    let dir = if dir.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        dir
    };
    std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
}

/// `--mode`, when given, overrides the plan's own `mode` in either direction.
fn apply_mode_override(plan: &mut glidesh::config::types::Plan, mode: Option<&str>) {
    match mode {
        Some("async") => plan.mode = ExecutionMode::Async,
        Some("sync") => plan.mode = ExecutionMode::Sync,
        _ => {}
    }
}

/// The secrets file, parsed but not yet opened: nothing has been prompted for or read
/// beyond the file itself.
struct ParsedSecrets {
    config: Option<secrets_config::SecretsConfig>,
    vars: HashMap<String, String>,
    structured: HashMap<String, Vec<HashMap<String, String>>>,
}

/// Discover and parse the secrets file, for `run` and `console` alike. With no file, the
/// result is empty and opening it prompts for nothing.
fn parse_secrets(
    flags: &cli::SecretSourceArgs,
    inv_base_dir: &std::path::Path,
) -> Result<ParsedSecrets, GlideshError> {
    let secrets_arg = flags.secrets.as_ref().map(|p| expand_tilde(p));
    let secrets_path =
        glidesh::secrets::config::discover_secrets_path(secrets_arg.as_deref(), Some(inv_base_dir));
    let Some(sp) = secrets_path else {
        return Ok(ParsedSecrets {
            config: None,
            vars: HashMap::new(),
            structured: HashMap::new(),
        });
    };
    let content = glidesh::secrets::store::read(&sp)?;
    let sf = glidesh::secrets::config::parse_secrets_file(&content)?;
    Ok(ParsedSecrets {
        config: sf.config,
        vars: sf.vars,
        structured: sf.structured,
    })
}

/// Unlock the secrets file for decryption. This is where a passphrase is prompted for or an
/// SSH identity read, so `run` calls it only once everything that can fail without them
/// has been checked.
fn open_secrets(
    flags: &cli::SecretSourceArgs,
    key: Option<&std::path::Path>,
    config: Option<&secrets_config::SecretsConfig>,
) -> Result<Arc<glidesh::secrets::Secrets>, GlideshError> {
    // Which credential to look for depends on how the file was wrapped, so this happens
    // after the config is parsed rather than from the flags alone. Without a provider block
    // there is no key to unwrap, so nothing is prompted for or read.
    let identity = match config.map(|c| &c.provider) {
        None => None,
        Some(glidesh::secrets::config::Provider::Age) => Some(glidesh::secrets::Identity::SshKey(
            secret_identity_path(flags.secret_identity.as_deref(), key),
        )),
        Some(_) => source_secret_pass(flags)?.map(glidesh::secrets::Identity::Passphrase),
    };
    glidesh::secrets::set_identity(identity);
    glidesh::secrets::Secrets::open(config, glidesh::secrets::identity())
}

/// Secret-file scalars sit under the inventory-global vars: an inline global var wins.
fn add_secret_vars(inventory: &mut Inventory, secret_vars: &HashMap<String, String>) {
    for (k, v) in secret_vars {
        inventory
            .global_vars
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }
}

/// Source the secrets passphrase, most explicit first: `--secret-pass-file`, then
/// `GLIDESH_SECRET_PASS`, then `GLIDESH_SECRET_PASS_FILE`, then `--ask-secret-pass`.
/// A flag the operator typed for this run outranks whatever the environment carries.
fn source_secret_pass(args: &cli::SecretSourceArgs) -> Result<Option<String>, GlideshError> {
    if let Some(path) = &args.secret_pass_file {
        return Ok(Some(read_pass_file(&expand_tilde(path))?));
    }
    if let Some(p) = secret_pass_from_env() {
        return Ok(Some(p));
    }
    if let Some(p) = secret_pass_file_from_env()? {
        return Ok(Some(p));
    }
    if args.ask_secret_pass {
        let p = rpassword::prompt_password("secret passphrase: ")
            .map_err(|e| GlideshError::Other(format!("Failed to read passphrase: {}", e)))?;
        return Ok(Some(p));
    }
    Ok(None)
}

async fn cmd_run(args: cli::RunArgs) -> Result<(), GlideshError> {
    if let (Some(host), Some(command)) = (&args.host, &args.command) {
        let user = args.user.as_deref().unwrap_or("root");

        // An ad-hoc command is the one thing glidesh cannot preview: there is no desired
        // state to compare a host against, only a command to run. Honor the flag by
        // refusing to run it rather than by connecting and running it anyway.
        if args.dry_run {
            println!("[dry-run] would run on {}@{}: {}", user, host, command);
            return Ok(());
        }

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

    // Establish the global escalation defaults. The CLI `--run-as*` flags are the
    // least-specific layer, so fold them into the inventory's global spec; every
    // host then resolves with CLI as the base. The password is global for the run.
    let cli_run_as = build_cli_run_as(&args)?;
    glidesh::modules::escalation::set_password(source_run_as_password(&args)?);
    let inventory = inventory.map(|mut inv| {
        inv.run_as = std::mem::take(&mut inv.run_as).merge_over(&cli_run_as);
        inv
    });

    let inv_base_dir = args
        .inventory
        .as_ref()
        .and_then(|p| p.parent())
        .unwrap_or_else(|| std::path::Path::new("."));

    let ParsedSecrets {
        config: secrets_provider,
        vars: secret_vars,
        structured: secret_structured,
    } = parse_secrets(&args.secrets, inv_base_dir)?;
    // As written, for telling which scope sets a variable a plan overrides.
    let written_inventory = inventory.clone();
    let inventory = inventory.map(|mut inv| {
        add_secret_vars(&mut inv, &secret_vars);
        inv
    });

    let inv_template_data = Arc::new(
        inventory
            .as_ref()
            .map(build_inventory_template_data)
            .unwrap_or_default(),
    );

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
        merge_secret_structured(&mut plan, &secret_structured);

        apply_mode_override(&mut plan, args.mode.as_deref());
        apply_rollout_override(&mut plan, args.serial.as_deref(), args.max_fail.as_deref())?;

        let targets = if let Some(ref host) = args.host {
            let user = args.user.as_deref().unwrap_or("root").to_string();
            // Without an inventory, secret-file scalars are the lowest tier under plan vars.
            let mut host_vars = secret_vars.clone();
            host_vars.extend(plan.vars.iter().map(|(k, v)| (k.clone(), v.clone())));
            vec![config::types::ResolvedHost {
                name: host.clone(),
                address: host.clone(),
                user,
                port: args.port,
                vars: host_vars,
                jump: None,
                run_as: cli_run_as.clone(),
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

        let plan_base_dir = absolute_dir(plan_base_dir);
        group_plans.push(executor::GroupPlan {
            plan: Arc::new(plan),
            targets,
            inventory_template_data: inv_template_data.clone(),
            plan_base_dir: Arc::new(plan_base_dir),
        });
    } else if let Some(ref inventory) = inventory {
        let group_plans_raw = inventory.resolve_group_plans();
        if group_plans_raw.is_empty() {
            return Err(GlideshError::Other(
                "No --plan provided and no groups have a plan= attribute in the inventory"
                    .to_string(),
            ));
        }

        // Parse --target filter into a list of tokens. Each token is either
        // "group", "hostname", or "group:host". The filter is a comma-separated
        // list; an entry passes if any token matches.
        let tokens: Vec<(Option<String>, Option<String>)> = match args.target.as_deref() {
            Some(t) => t
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|tok| {
                    if let Some((g, h)) = tok.split_once(':') {
                        (Some(g.to_string()), Some(h.to_string()))
                    } else {
                        (Some(tok.to_string()), None)
                    }
                })
                .collect(),
            None => Vec::new(),
        };

        for (group_name, plan_path, targets) in &group_plans_raw {
            let host_matches = |h: &config::types::ResolvedHost| -> bool {
                if tokens.is_empty() {
                    return true;
                }
                for (g_filter, h_filter) in &tokens {
                    match h_filter {
                        Some(host_name) => {
                            // "group:host" — both must match this entry.
                            if g_filter.as_deref() == Some(group_name.as_str())
                                && h.name == *host_name
                            {
                                return true;
                            }
                        }
                        None => {
                            let token = g_filter.as_deref().unwrap_or("");
                            // Plain token: match group name (keeps all hosts in
                            // entry) or match host name (this host only).
                            if !group_name.is_empty() && token == group_name {
                                return true;
                            }
                            if h.name == token {
                                return true;
                            }
                        }
                    }
                }
                false
            };

            let filtered_targets: Vec<_> = targets
                .iter()
                .filter(|h| host_matches(h))
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
            merge_secret_structured(&mut plan, &secret_structured);

            apply_mode_override(&mut plan, args.mode.as_deref());
            apply_rollout_override(&mut plan, args.serial.as_deref(), args.max_fail.as_deref())?;

            let pn = plan.name.clone();
            let gn = group_name.clone();
            run_name_parts.push(format!("{}-{}", gn, pn));
            all_host_names.extend(
                filtered_targets
                    .iter()
                    .map(|h| (h.name.clone(), gn.clone(), pn.clone())),
            );

            let plan_base_dir = absolute_dir(include_base);
            group_plans.push(executor::GroupPlan {
                plan: Arc::new(plan),
                targets: filtered_targets,
                inventory_template_data: inv_template_data.clone(),
                plan_base_dir: Arc::new(plan_base_dir),
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
    let tags = TagFilter::from_args(args.tags.as_deref(), args.skip_tags.as_deref())?;
    tags.check_known(group_plans.iter().map(|gp| gp.plan.as_ref()))?;
    // A host given with --host takes no inventory variables.
    let shadowable = written_inventory.as_ref().filter(|_| args.host.is_none());
    let secret_names: std::collections::HashSet<String> = secret_vars.keys().cloned().collect();
    for gp in &group_plans {
        let hosts = gp.targets.iter().map(|h| h.name.as_str());
        for shadow in config::shadow::shadowed(&gp.plan, shadowable, &secret_names, hosts) {
            eprintln!("warning: {}", shadow.warning());
        }
    }
    let answers = answer_prompts(&mut group_plans, &args.vars)?;
    let secrets = open_secrets(
        &args.secrets,
        args.key.as_deref(),
        secrets_provider.as_ref(),
    )?;
    for answer in answers.iter().filter(|a| a.secret) {
        secrets.register_plaintext(&answer.value);
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
        secrets,
        Arc::new(tags),
        &run_name,
        &all_host_names,
        &args,
    )
    .await
}

/// Answer the run's `vars-prompt`s — from `--var`, else on the terminal — and make each
/// answer a plan variable of the plans that ask for it. Runs before anything connects, and
/// without a terminal never waits for input.
fn answer_prompts(
    group_plans: &mut [executor::GroupPlan],
    var_flags: &[String],
) -> Result<Vec<config::prompts::Answer>, GlideshError> {
    use std::io::IsTerminal;
    let prompts = config::prompts::distinct_prompts(group_plans.iter().map(|gp| gp.plan.as_ref()));
    let answers = config::prompts::resolve_answers(
        &prompts,
        var_flags,
        std::io::stdin().is_terminal(),
        ask_prompt,
    )?;
    for gp in group_plans.iter_mut() {
        config::prompts::apply_answers(Arc::make_mut(&mut gp.plan), &answers);
    }
    Ok(answers)
}

/// Ask one prompt on the terminal. An empty answer takes the default; with no default the
/// question is asked again, since an empty value is far more often a slip than intended.
fn ask_prompt(prompt: &config::types::VarPrompt) -> Result<String, GlideshError> {
    use std::io::Write;
    let label = match (&prompt.default, prompt.secret) {
        (Some(_), true) => format!("{} [keep default]: ", prompt.text),
        (Some(default), false) => format!("{} [{}]: ", prompt.text, default),
        (None, _) => format!("{}: ", prompt.text),
    };
    let failed = |e: std::io::Error| {
        GlideshError::Other(format!(
            "Failed to read an answer for '{}': {e}",
            prompt.name
        ))
    };
    loop {
        let answer = if prompt.secret {
            rpassword::prompt_password(&label).map_err(failed)?
        } else {
            eprint!("{label}");
            std::io::stderr().flush().map_err(failed)?;
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).map_err(failed)? == 0 {
                return Err(GlideshError::Other(format!(
                    "no answer for '{}': input ended",
                    prompt.name
                )));
            }
            line.trim_end_matches(['\r', '\n']).to_string()
        };
        match config::prompts::settle_typed_answer(prompt, answer) {
            Ok(answer) => return Ok(answer),
            Err(ask_again) => eprintln!("{ask_again}"),
        }
    }
}

/// Merge secret-file structured vars into a plan (the plan's own value wins on conflict).
fn merge_secret_structured(
    plan: &mut config::types::Plan,
    secret_structured: &HashMap<String, Vec<HashMap<String, String>>>,
) {
    for (k, v) in secret_structured {
        plan.structured_vars
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }
}

fn display_id(host: &str, display_ids: &std::collections::HashMap<String, String>) -> String {
    display_ids
        .get(host)
        .cloned()
        .unwrap_or_else(|| host.to_string())
}

/// Which stream an event's lines belong on. Failures go to stderr so a plain-text run
/// can be piped with the progress narration separated from the problems.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OutStream {
    Out,
    Err,
}

/// Render an event as the lines `print_event` will write, and the stream they go to.
/// Split out from the printing so the formatting can be asserted directly.
fn event_lines(
    event: &ExecutorEvent,
    display_ids: &std::collections::HashMap<String, String>,
) -> (OutStream, Vec<String>) {
    match event {
        ExecutorEvent::NodeConnecting { host } => (
            OutStream::Out,
            vec![format!("[{}] Connecting...", display_id(host, display_ids))],
        ),
        ExecutorEvent::NodeConnected { host, os } => (
            OutStream::Out,
            vec![format!(
                "[{}] Connected ({})",
                display_id(host, display_ids),
                os.id
            )],
        ),
        ExecutorEvent::NodeAuthFailed { host, error } => (
            OutStream::Err,
            vec![format!(
                "[{}] Auth failed: {}",
                display_id(host, display_ids),
                error
            )],
        ),
        ExecutorEvent::StepStarted {
            host,
            step,
            step_index,
            total_steps,
        } => (
            OutStream::Out,
            vec![format!(
                "[{}] Step {}/{}: {}",
                display_id(host, display_ids),
                step_index + 1,
                total_steps,
                step
            )],
        ),
        ExecutorEvent::ModuleCheck {
            host,
            module,
            resource,
        } => (
            OutStream::Out,
            vec![format!(
                "[{}]   Checking {} '{}'",
                display_id(host, display_ids),
                module,
                resource
            )],
        ),
        ExecutorEvent::ModuleResult {
            host,
            module,
            resource,
            changed,
            dry_run,
            stdout,
            stderr,
            ..
        } => {
            let id = display_id(host, display_ids);
            let mut lines = vec![format!(
                "[{}]   {} '{}': {}",
                id,
                module,
                resource,
                executor::changed_label(*changed, *dry_run)
            )];
            // A preview's whole payload is the description of the pending work, so show
            // it here. A real run's stdout stays in the run log, as before.
            if *dry_run {
                lines.extend(
                    crate::logging::stream_log_lines("stdout", stdout)
                        .into_iter()
                        .map(|line| format!("[{}] {}", id, line)),
                );
            }
            lines.extend(
                crate::logging::stream_log_lines("stderr", stderr)
                    .into_iter()
                    .map(|line| format!("[{}] {}", id, line)),
            );
            (OutStream::Out, lines)
        }
        ExecutorEvent::ModuleFailed {
            host,
            module,
            resource,
            error,
        } => (
            OutStream::Err,
            vec![format!(
                "[{}]   FAILED {} '{}': {}",
                display_id(host, display_ids),
                module,
                resource,
                error
            )],
        ),
        ExecutorEvent::StepFailed { host, step, error } => (
            OutStream::Err,
            vec![format!(
                "[{}]   FAILED step '{}': {}",
                display_id(host, display_ids),
                step,
                error
            )],
        ),
        ExecutorEvent::StepSkipped { host, reason, .. } => (
            OutStream::Out,
            vec![format!(
                "[{}]   skipped ({})",
                display_id(host, display_ids),
                reason
            )],
        ),
        ExecutorEvent::StepWaiting {
            host,
            command,
            elapsed_secs,
            timeout_secs,
            first,
            preview,
            ..
        } => (
            OutStream::Out,
            vec![format!(
                "[{}]   {}",
                display_id(host, display_ids),
                executor::waiting_text(command, *elapsed_secs, *timeout_secs, *first, *preview)
            )],
        ),
        ExecutorEvent::SectionStarted {
            host,
            step,
            section,
        } => (
            OutStream::Out,
            vec![format!(
                "[{}]   {} step '{}'",
                display_id(host, display_ids),
                section.label(),
                step
            )],
        ),
        ExecutorEvent::TaskSkipped {
            host,
            module,
            resource,
            reason,
        } => (
            OutStream::Out,
            vec![format!(
                "[{}]   {} '{}': skipped ({})",
                display_id(host, display_ids),
                module,
                resource,
                reason
            )],
        ),
        ExecutorEvent::BatchStarted {
            index,
            total,
            hosts,
        } => (
            OutStream::Out,
            vec![format!(
                "--- Batch {}/{}: {} ---",
                index + 1,
                total,
                hosts
                    .iter()
                    .map(|h| display_id(h, display_ids))
                    .collect::<Vec<_>>()
                    .join(", ")
            )],
        ),
        ExecutorEvent::HostsAborted { hosts, reason } => (
            OutStream::Err,
            std::iter::once(format!("--- Rollout stopped: {} ---", reason))
                .chain(
                    hosts
                        .iter()
                        .map(|h| format!("[{}] ABORTED (not started)", display_id(h, display_ids))),
                )
                .collect(),
        ),
        ExecutorEvent::NodeComplete {
            host,
            success,
            changed,
            skipped,
            dry_run,
        } => (
            OutStream::Out,
            vec![format!(
                "[{}] {} ({} {}{})",
                display_id(host, display_ids),
                if *success { "OK" } else { "FAILED" },
                changed,
                if *dry_run { "would change" } else { "changed" },
                executor::skipped_suffix(*skipped)
            )],
        ),
        ExecutorEvent::RunComplete { summary } => (
            OutStream::Out,
            vec![
                if summary.dry_run {
                    "\n--- Dry Run Complete (nothing applied) ---".to_string()
                } else {
                    "\n--- Run Complete ---".to_string()
                },
                format!(
                    "Hosts: {} total, {} ok, {} failed{}, {} {}{}",
                    summary.total_hosts,
                    summary.succeeded,
                    summary.failed,
                    executor::aborted_suffix(summary.aborted),
                    summary.total_changed,
                    if summary.dry_run {
                        "would change"
                    } else {
                        "changed"
                    },
                    executor::skipped_suffix(summary.total_skipped)
                ),
            ],
        ),
    }
}

fn print_event(event: &ExecutorEvent, display_ids: &std::collections::HashMap<String, String>) {
    let (stream, lines) = event_lines(event, display_ids);
    for line in lines {
        match stream {
            OutStream::Out => println!("{}", line),
            OutStream::Err => eprintln!("{}", line),
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
            println!("  {}  ({})", name, summary.node_counts());
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

/// The provider-specific tail of a `validate` line: how many people can open an age file,
/// or how strongly a passphrase file's key is wrapped. Blobs are self-describing, so a file
/// created by a development build keeps its low scrypt cost forever unless someone is told —
/// this is where they are told.
fn provider_detail(cfg: &secrets_config::SecretsConfig) -> String {
    match cfg.provider {
        secrets_config::Provider::Age => format!(", {} recipient(s)", cfg.recipients.len()),
        secrets_config::Provider::Passphrase => {
            match secret_passphrase::wrap_cost(&cfg.encryptedkey) {
                Some(cost) if cost < secret_passphrase::current_cost() => format!(
                    ", key wrapped at scrypt cost 2^{cost}, below this build's default of \
                     2^{}: run `glidesh secret rekey` to strengthen it",
                    secret_passphrase::current_cost()
                ),
                Some(cost) => format!(", key wrapped at scrypt cost 2^{cost}"),
                None => String::new(),
            }
        }
    }
}

/// What `validate` found in a plan. `problems` fail validation; `warnings` do not.
#[derive(Default)]
struct PlanCheck {
    steps: usize,
    problems: Vec<String>,
    warnings: Vec<String>,
}

/// Load a plan the way `run` does and check everything that can be known without contacting
/// a host.
///
/// Includes and `vars-file` are resolved from the plan's directory, which is also where step
/// names and `subscribe` references are checked. External modules are discovered next to the
/// inventory when one is given, else in the current directory — again as `run` does.
/// `known_vars` are the names a run could define outside the plan itself, for the
/// literal-reference warning.
fn validate_plan_file(
    plan_path: &std::path::Path,
    inv_dir: Option<&std::path::Path>,
    known_vars: &std::collections::HashSet<String>,
    shadows: impl Fn(&config::types::Plan) -> Vec<String>,
) -> PlanCheck {
    let fatal = |e: String| PlanCheck {
        problems: vec![e],
        ..PlanCheck::default()
    };
    let content = match std::fs::read_to_string(plan_path) {
        Ok(c) => c,
        Err(e) => return fatal(e.to_string()),
    };
    let mut plan = match config::parse_plan(&content) {
        Ok(p) => p,
        Err(e) => return fatal(e.to_string()),
    };
    let plan_dir = plan_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    if let Err(e) = config::resolve_includes(&mut plan, plan_dir) {
        return fatal(e.to_string());
    }

    let mut check = PlanCheck {
        steps: plan.steps().len(),
        ..PlanCheck::default()
    };
    let registry =
        ModuleRegistry::with_external(Some(inv_dir.unwrap_or_else(|| std::path::Path::new("."))));
    if let Err(e) = registry.validate_plan(&plan) {
        check.problems.push(e.to_string());
    }
    check
        .problems
        .extend(config::checks::missing_file_sources(&plan, plan_dir));
    check
        .problems
        .extend(config::checks::template_scope_problems(&plan, plan_dir));
    check.warnings = config::checks::literal_reference_warnings(&plan, plan_dir, |name| {
        plan.vars.contains_key(name)
            || plan.prompts.iter().any(|p| p.name == name)
            || known_vars.contains(name)
            || is_builtin_var(name)
    });
    check.warnings.extend(shadows(&plan));
    check
}

/// `validate`'s warnings for `plan`'s variables that the inventory or the secrets file
/// also sets for one of `hosts`.
fn shadow_warnings(
    plan: &config::types::Plan,
    inventory: Option<&Inventory>,
    secret_names: &std::collections::HashSet<String>,
    hosts: &[String],
) -> Vec<String> {
    config::shadow::shadowed(
        plan,
        inventory,
        secret_names,
        hosts.iter().map(String::as_str),
    )
    .iter()
    .map(config::shadow::Shadow::warning)
    .collect()
}

/// A name in a namespace glidesh injects at run time, whatever the host.
fn is_builtin_var(name: &str) -> bool {
    [
        "@host.",
        "@os.",
        "@fact.",
        "@inventory.",
        "@item.",
        "@error.",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
        || name == "@item"
}

/// Print a plan's result on the line `validate` opened for it; true when it passed.
fn report_plan(check: &PlanCheck) -> bool {
    match check.problems.as_slice() {
        [] => println!("OK ({} steps)", check.steps),
        [one] => println!("FAILED: {}", one),
        many => {
            println!("FAILED:");
            for problem in many {
                println!("  - {}", problem);
            }
        }
    }
    for warning in &check.warnings {
        println!("  warning: {}", warning);
    }
    check.problems.is_empty()
}

fn cmd_validate(args: cli::ValidateArgs) -> Result<(), GlideshError> {
    let mut valid = true;

    // Read leniently here — a broken file is reported by its own check below.
    let inventory = args
        .inventory
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|c| config::parse_inventory(&c).ok());
    let inv_dir = args.inventory.as_ref().and_then(|p| p.parent());

    // Names a run could define outside the plan: the secrets file's, plus inventory
    // variables — any host's for `-p`, the plan's own hosts' for an inventory `plan=`.
    let mut secret_vars = std::collections::HashSet::new();
    if let Some(secrets) = secrets_config::discover_secrets_path(None, inv_dir)
        .and_then(|p| secret_store::read(&p).ok())
        .and_then(|c| secrets_config::parse_secrets_file(&c).ok())
    {
        secret_vars.extend(secrets.vars.into_keys());
    }

    if let Some(ref fp_path) = args.plan {
        let mut known_vars = secret_vars.clone();
        for host in inventory.iter().flat_map(|inv| inv.resolve_targets(None)) {
            known_vars.extend(host.vars.into_keys());
        }
        // `run -p` may target any of the inventory's hosts.
        let hosts: Vec<String> = inventory
            .iter()
            .flat_map(|inv| inv.resolve_targets(None))
            .map(|h| h.name)
            .collect();
        let shadows = |plan: &config::types::Plan| {
            shadow_warnings(plan, inventory.as_ref(), &secret_vars, &hosts)
        };
        print!("Validating plan '{}'... ", fp_path.display());
        valid &= report_plan(&validate_plan_file(fp_path, inv_dir, &known_vars, shadows));
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

    // Without `-p`, `run -i` runs the plans the inventory names, so those are what to check.
    if let (None, Some(inventory)) = (&args.plan, &inventory) {
        let base = inv_dir.unwrap_or_else(|| std::path::Path::new("."));
        let mut plans: Vec<(PathBuf, Vec<config::types::ResolvedHost>)> = Vec::new();
        for (_, plan_path, hosts) in inventory.resolve_group_plans() {
            let path = base.join(plan_path);
            match plans.iter_mut().find(|(p, _)| *p == path) {
                Some((_, known)) => known.extend(hosts),
                None => plans.push((path, hosts)),
            }
        }
        for (path, hosts) in plans {
            let mut known_vars = secret_vars.clone();
            for host in &hosts {
                known_vars.extend(host.vars.keys().cloned());
            }
            print!(
                "Validating plan '{}' ({} host{})... ",
                path.display(),
                hosts.len(),
                if hosts.len() == 1 { "" } else { "s" }
            );
            let hosts: Vec<String> = hosts.into_iter().map(|h| h.name).collect();
            let shadows = |plan: &config::types::Plan| {
                shadow_warnings(plan, Some(inventory), &secret_vars, &hosts)
            };
            valid &= report_plan(&validate_plan_file(&path, inv_dir, &known_vars, shadows));
        }
    }

    // A secrets file beside the inventory is part of the configuration a run will load, so
    // check it here rather than letting a malformed provider block surface mid-deploy. Only
    // the file is parsed — validating never needs the passphrase.
    if let Some(path) = secrets_config::discover_secrets_path(None, inv_dir) {
        print!("Validating secrets '{}'... ", path.display());
        match secret_store::read(&path).and_then(|c| secrets_config::parse_secrets_file(&c)) {
            Ok(secrets_file) => {
                let count = secrets_file.vars.len() + secrets_file.structured.len();
                match secrets_file.config {
                    Some(cfg) => println!(
                        "OK ({count} values, provider {}{})",
                        cfg.provider.as_str(),
                        provider_detail(&cfg)
                    ),
                    None => println!("OK ({count} values, no provider block)"),
                }
            }
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

/// Build `TemplateData` from an inventory for `@inventory.*` and `@group.*` template references.
fn build_inventory_template_data(inventory: &Inventory) -> TemplateData {
    let mut data = TemplateData::default();

    // @inventory.<host>.* flat vars for direct host lookups
    let all_hosts = inventory.resolve_targets(None);
    for rh in &all_hosts {
        let prefix = format!("@inventory.{}", rh.name);
        data.extra_vars
            .insert(format!("{}.address", prefix), rh.address.clone());
        data.extra_vars
            .insert(format!("{}.user", prefix), rh.user.clone());
        data.extra_vars
            .insert(format!("{}.port", prefix), rh.port.to_string());
        for (k, v) in &rh.vars {
            data.extra_vars
                .insert(format!("{}.vars.{}", prefix, k), v.clone());
        }
    }

    // Build name→ResolvedHost map for group lookups (avoids resolve_targets
    // which matches groups before hosts and could return wrong results)
    let host_map: HashMap<&str, &glidesh::config::types::ResolvedHost> =
        all_hosts.iter().map(|rh| (rh.name.as_str(), rh)).collect();

    // @group.<name> collections for loop iteration
    for group in &inventory.groups {
        let group_hosts: Vec<HashMap<String, String>> = group
            .hosts
            .iter()
            .filter_map(|h| host_map.get(h.name.as_str()))
            .map(|rh| {
                HashMap::from([
                    ("name".to_string(), rh.name.clone()),
                    ("address".to_string(), rh.address.clone()),
                    ("user".to_string(), rh.user.clone()),
                    ("port".to_string(), rh.port.to_string()),
                ])
            })
            .collect();
        data.collections
            .insert(format!("@group.{}", group.name), group_hosts);
    }

    data
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
    load_key_from_path(&key_path)
}

fn load_key_from_path(
    key_path: &std::path::Path,
) -> Result<russh_keys::key::PrivateKeyWithHashAlg, GlideshError> {
    tracing::debug!("Using SSH key: {}", key_path.display());
    let key_pair = russh_keys::load_secret_key(key_path, None)?;
    let hash_alg = match key_pair.algorithm() {
        ssh_key::Algorithm::Rsa { .. } => Some(ssh_key::HashAlg::Sha256),
        _ => None,
    };
    Ok(russh_keys::key::PrivateKeyWithHashAlg::new(
        Arc::new(key_pair),
        hash_alg,
    )?)
}

async fn cmd_console(args: cli::ConsoleArgs) -> Result<(), GlideshError> {
    let inv_path = match args.inventory {
        Some(p) => p,
        None => {
            let default = PathBuf::from("inventory.kdl");
            if default.exists() {
                default
            } else {
                return Err(GlideshError::Other(
                    "No inventory file found. Create ./inventory.kdl or pass --inventory <path>."
                        .to_string(),
                ));
            }
        }
    };

    let inv_content = std::fs::read_to_string(&inv_path).map_err(|e| {
        GlideshError::Other(format!(
            "Failed to read inventory '{}': {}",
            inv_path.display(),
            e
        ))
    })?;
    let mut inventory = config::parse_inventory(&inv_content)?;

    // Only `--vars` needs the secrets file. Without it nothing is read or prompted for, and
    // the redaction below is a pass-through.
    let secrets = if args.vars {
        let inv_dir = inv_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let parsed = parse_secrets(&args.secrets, inv_dir)?;
        add_secret_vars(&mut inventory, &parsed.vars);
        open_secrets(&args.secrets, args.key.as_deref(), parsed.config.as_ref())?
    } else {
        glidesh::secrets::Secrets::locked()
    };

    let host_key_policy = HostKeyPolicy {
        verify: !args.no_host_key_check,
        accept_new: args.accept_new_host_key,
    };

    let shell_mode = args.target.is_some() || args.command.is_some();

    if shell_mode {
        let hosts = inventory.resolve_targets(args.target.as_deref());
        if hosts.is_empty() {
            return Err(GlideshError::NoTargets);
        }
        let key = resolve_key(args.key.as_deref(), hosts.first().map(|h| &h.vars))?;

        match (hosts.len(), &args.command) {
            (1, Some(command)) => {
                let host = &hosts[0];
                let command = console_command(command, host, &secrets, args.vars)?;
                let registry = secrets.registry();
                let session = connect_host(host, &key, host_key_policy).await?;
                let output = session.exec(&command).await?;
                let stdout = registry.redact(&output.stdout);
                let stderr = registry.redact(&output.stderr);
                if !stdout.is_empty() {
                    print!("{}", stdout);
                }
                if !stderr.is_empty() {
                    eprint!("{}", stderr);
                }
                let exit_code = output.exit_code;
                session.close().await?;
                if exit_code != 0 {
                    return Err(GlideshError::SshCommand {
                        exit_code,
                        stdout,
                        stderr,
                    });
                }
            }
            (1, None) => {
                let host = &hosts[0];
                eprintln!(
                    "Opening shell to {}@{}:{}",
                    host.user, host.address, host.port
                );
                let session = connect_host(host, &key, host_key_policy).await?;
                let exit_code = session.interactive_shell().await?;
                session.close().await?;
                if exit_code != 0 {
                    std::process::exit(exit_code as i32);
                }
            }
            (_, Some(command)) => {
                let jobs = hosts
                    .iter()
                    .map(|host| {
                        let command = console_command(command, host, &secrets, args.vars)
                            .map_err(|e| e.to_string());
                        (host.clone(), command)
                    })
                    .collect();
                run_command_on_hosts(
                    jobs,
                    &key,
                    host_key_policy,
                    args.concurrency,
                    secrets.registry(),
                )
                .await?;
            }
            (_, None) => {
                tui::run_shell_tui(&hosts, &key, host_key_policy, args.concurrency).await?;
            }
        }
        return Ok(());
    }

    if !tui::is_tty() {
        return Err(GlideshError::Other(
            "`glidesh console` requires a TTY. Use `glidesh run` or pass --target/--command for scripted execution."
                .to_string(),
        ));
    }

    let all_hosts = inventory.resolve_targets(None);
    if all_hosts.is_empty() {
        return Err(GlideshError::NoTargets);
    }
    let key_path = resolve_key_path(args.key.as_deref(), all_hosts.first().map(|h| &h.vars));
    let key = load_key_from_path(&key_path)?;

    tui::console::run(&inv_path, &inventory, key, key_path, host_key_policy)
        .await
        .map_err(|e| GlideshError::Other(format!("Console TUI error: {}", e)))?;
    Ok(())
}

/// The command `console -c` runs on `host`.
///
/// Without `--vars` it is the command as typed, so a shell's own `${VAR}` reaches the host
/// untouched. With it, `${name}` references are filled from the host's merged inventory
/// variables (the secrets file's included) and `@host.*`, then any secret token — from a
/// variable or written inline — is decrypted. An undefined name fails that host rather than
/// sending a half-substituted command.
fn console_command(
    command: &str,
    host: &config::types::ResolvedHost,
    secrets: &glidesh::secrets::Secrets,
    interpolate: bool,
) -> Result<String, GlideshError> {
    if !interpolate {
        return Ok(command.to_string());
    }
    let mut vars = host.vars.clone();
    vars.extend(executor::node_runner::host_builtin_vars(host));
    secrets.decrypt_vars(&mut vars)?;
    let resolved = config::template::interpolate(command, &vars)?;
    if glidesh::secrets::token::contains_secret_token(&resolved) {
        secrets.decrypt_inline(&resolved)
    } else {
        Ok(resolved)
    }
}

fn resolve_key_path(
    cli_key: Option<&std::path::Path>,
    inv_vars: Option<&HashMap<String, String>>,
) -> PathBuf {
    if let Some(k) = cli_key {
        expand_tilde(k)
    } else if let Some(inv_key) = inv_vars.and_then(|v| v.get("ssh-key")) {
        expand_tilde(&PathBuf::from(inv_key))
    } else {
        expand_tilde(&default_ssh_key())
    }
}

fn resolve_key(
    cli_key: Option<&std::path::Path>,
    inv_vars: Option<&HashMap<String, String>>,
) -> Result<russh_keys::key::PrivateKeyWithHashAlg, GlideshError> {
    load_key_from_path(&resolve_key_path(cli_key, inv_vars))
}

async fn connect_host(
    host: &config::types::ResolvedHost,
    key: &russh_keys::key::PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
) -> Result<SshSession, GlideshError> {
    match &host.jump {
        Some(jump) => {
            SshSession::connect_via_jump(&host.address, host.port, &host.user, key, policy, jump)
                .await
        }
        None => SshSession::connect(&host.address, host.port, &host.user, key, policy).await,
    }
}

/// Redacts the whole stream before splitting it, so a secret that itself contains a newline is
/// still found.
fn redacted_lines(output: &str, registry: &glidesh::secrets::SecretRegistry) -> Vec<String> {
    registry
        .redact(output)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Run each host's command concurrently and stream `[host]`-prefixed output. A host whose
/// command could not be built is reported and counted as failed without connecting.
async fn run_command_on_hosts(
    jobs: Vec<(config::types::ResolvedHost, Result<String, String>)>,
    key: &russh_keys::key::PrivateKeyWithHashAlg,
    policy: HostKeyPolicy,
    concurrency: usize,
    registry: Arc<glidesh::secrets::SecretRegistry>,
) -> Result<(), GlideshError> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String, bool)>();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));

    let mut handles = Vec::new();
    for (host, command) in jobs {
        let tx = tx.clone();
        let key = key.clone();
        let sem = semaphore.clone();
        let registry = registry.clone();

        handles.push(tokio::spawn(async move {
            let name = host.name.clone();
            let command = match command {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send((name, e, true));
                    return true;
                }
            };
            let _permit = sem.acquire().await;
            let session = match connect_host(&host, &key, policy).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send((name, format!("Connection failed: {}", e), true));
                    return true; // failed
                }
            };
            let failed = match session.exec(&command).await {
                Ok(output) => {
                    for line in redacted_lines(&output.stdout, &registry) {
                        let _ = tx.send((name.clone(), line, false));
                    }
                    for line in redacted_lines(&output.stderr, &registry) {
                        let _ = tx.send((name.clone(), line, true));
                    }
                    if output.exit_code != 0 {
                        let _ = tx.send((
                            name.clone(),
                            format!("(exit code {})", output.exit_code),
                            true,
                        ));
                        true
                    } else {
                        false
                    }
                }
                Err(e) => {
                    let _ = tx.send((name, format!("Command failed: {}", e), true));
                    true
                }
            };
            let _ = session.close().await;
            failed
        }));
    }
    drop(tx);

    // Command output was redacted whole before it was split; this catches a secret quoted in
    // an error message.
    while let Some((host, line, is_stderr)) = rx.recv().await {
        let line = registry.redact(&line);
        if is_stderr {
            eprintln!("[{}] {}", host, line);
        } else {
            println!("[{}] {}", host, line);
        }
    }

    let mut failed_count = 0;
    for handle in handles {
        if let Ok(true) = handle.await {
            failed_count += 1;
        }
    }

    if failed_count > 0 {
        return Err(GlideshError::Executor {
            message: format!("{} host(s) failed", failed_count),
        });
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_with_ui(
    group_plans: Vec<executor::GroupPlan>,
    registry: Arc<ModuleRegistry>,
    key: russh_keys::key::PrivateKeyWithHashAlg,
    secrets: Arc<glidesh::secrets::Secrets>,
    tags: Arc<TagFilter>,
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
    let diff = args.diff;
    let host_key_policy = HostKeyPolicy {
        verify: !args.no_host_key_check,
        accept_new: args.accept_new_host_key,
    };

    if tui::is_tty() && !args.no_tui {
        let connection_info: Vec<tui::state::HostConnectionInfo> = group_plans
            .iter()
            .flat_map(|gp| &gp.targets)
            .map(|h| tui::state::HostConnectionInfo {
                address: h.address.clone(),
                user: h.user.clone(),
                port: h.port,
                jump: h.jump.clone(),
            })
            .collect();

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

        let tui_key = key.clone();
        let host_names_owned = host_names.to_vec();
        let engine_handle = tokio::spawn(async move {
            executor::run(
                group_plans,
                registry,
                key,
                concurrency,
                dry_run,
                diff,
                tags,
                host_key_policy,
                secrets,
                combined_tx,
            )
            .await
        });

        let abort_handle = engine_handle.abort_handle();
        let aborted = tui::run_tui(
            tui_rx,
            &host_names_owned,
            abort_handle,
            connection_info,
            tui_key,
            host_key_policy,
            dry_run,
        )
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
            if let Some(err) = run_failure(&summary) {
                return Err(err);
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
            diff,
            tags,
            host_key_policy,
            secrets,
            event_tx,
        )
        .await?;
        let _ = consumer.await;

        if let Some(err) = run_failure(&summary) {
            return Err(err);
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

#[cfg(test)]
mod tests {
    use super::*;
    use executor::result::Section;

    #[test]
    fn the_directory_of_a_bare_plan_name_is_the_absolute_current_one() {
        let bare = std::path::Path::new("plan.kdl").parent().unwrap();
        let dir = absolute_dir(bare);
        assert!(dir.is_absolute(), "{}", dir.display());
        assert_eq!(dir, std::fs::canonicalize(".").unwrap());
    }

    #[test]
    fn a_plan_directory_that_does_not_resolve_is_still_absolute() {
        let dir = absolute_dir(std::path::Path::new("no-such-dir/plans"));
        assert!(dir.is_absolute(), "{}", dir.display());
    }

    fn no_display_ids() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::new()
    }

    fn module_result(changed: bool, dry_run: bool, stdout: &str) -> ExecutorEvent {
        ExecutorEvent::ModuleResult {
            host: "web-1".to_string(),
            module: "container".to_string(),
            resource: "lmcache".to_string(),
            changed,
            dry_run,
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    #[test]
    fn plain_output_shows_the_reason_only_for_a_preview() {
        let reason = "Recreate container lmcache (configuration changed)";
        let (stream, lines) = event_lines(&module_result(true, true, reason), &no_display_ids());
        assert_eq!(stream, OutStream::Out);
        assert!(lines[0].contains("would change"), "got: {:?}", lines[0]);
        assert!(
            lines.iter().any(|l| l.contains(reason)),
            "the reason must be printed: {lines:?}"
        );

        let (_, lines) = event_lines(
            &module_result(true, false, "some output"),
            &no_display_ids(),
        );
        assert!(lines[0].contains("changed"));
        assert!(!lines[0].contains("would change"));
        assert!(
            !lines.iter().any(|l| l.contains("some output")),
            "a real run must not dump stdout: {lines:?}"
        );
    }

    #[test]
    fn plain_output_calls_a_satisfied_task_ok_in_either_mode() {
        for dry_run in [true, false] {
            let (_, lines) = event_lines(&module_result(false, dry_run, ""), &no_display_ids());
            assert_eq!(lines.len(), 1);
            assert!(lines[0].ends_with("ok"), "got: {:?}", lines[0]);
        }
    }

    #[test]
    fn the_plain_summary_says_nothing_was_applied_in_a_preview() {
        let event = |dry_run| ExecutorEvent::RunComplete {
            summary: executor::result::RunSummary {
                total_hosts: 2,
                succeeded: 2,
                failed: 0,
                total_changed: 3,
                total_skipped: 0,
                aborted: 0,
                dry_run,
            },
        };

        let (_, lines) = event_lines(&event(true), &no_display_ids());
        assert!(lines[0].contains("Dry Run Complete (nothing applied)"));
        assert!(lines[1].ends_with("3 would change"), "got: {:?}", lines[1]);

        let (_, lines) = event_lines(&event(false), &no_display_ids());
        assert!(lines[0].contains("Run Complete"));
        assert!(!lines[0].contains("Dry Run"));
        assert!(lines[1].ends_with("3 changed"), "got: {:?}", lines[1]);
    }

    /// The per-host line must agree with the task lines above it and the summary below.
    #[test]
    fn the_per_host_count_is_worded_like_the_rest_of_the_run() {
        let event = |dry_run| ExecutorEvent::NodeComplete {
            host: "web-1".to_string(),
            success: true,
            changed: 1,
            skipped: 0,
            dry_run,
        };

        let (_, lines) = event_lines(&event(true), &no_display_ids());
        assert!(
            lines[0].ends_with("OK (1 would change)"),
            "got: {:?}",
            lines[0]
        );

        let (_, lines) = event_lines(&event(false), &no_display_ids());
        assert!(lines[0].ends_with("OK (1 changed)"), "got: {:?}", lines[0]);
    }

    #[test]
    fn a_skipped_task_names_itself_and_its_condition() {
        let (stream, lines) = event_lines(
            &ExecutorEvent::TaskSkipped {
                host: "web-1".to_string(),
                module: "package".to_string(),
                resource: "nginx".to_string(),
                reason: "when: ${@os.family} == redhat".to_string(),
            },
            &no_display_ids(),
        );
        assert_eq!(stream, OutStream::Out);
        assert_eq!(
            lines,
            ["[web-1]   package 'nginx': skipped (when: ${@os.family} == redhat)"]
        );
    }

    #[test]
    fn a_skipped_step_gives_its_reason() {
        let (_, lines) = event_lines(
            &ExecutorEvent::StepSkipped {
                host: "web-1".to_string(),
                step: "Install".to_string(),
                tasks: 2,
                reason: "when: ${x}".to_string(),
            },
            &no_display_ids(),
        );
        assert_eq!(lines, ["[web-1]   skipped (when: ${x})"]);
    }

    #[test]
    fn rescue_and_always_are_announced_before_their_tasks() {
        for (section, expected) in [
            (Section::Rescue, "[web-1]   RESCUE step 'Deploy'"),
            (Section::Always, "[web-1]   ALWAYS step 'Deploy'"),
        ] {
            let (stream, lines) = event_lines(
                &ExecutorEvent::SectionStarted {
                    host: "web-1".to_string(),
                    step: "Deploy".to_string(),
                    section,
                },
                &no_display_ids(),
            );
            assert_eq!(stream, OutStream::Out);
            assert_eq!(lines, [expected]);
        }
    }

    #[test]
    fn a_waiting_step_shows_its_gate() {
        let (_, lines) = event_lines(
            &ExecutorEvent::StepWaiting {
                host: "web-1".to_string(),
                step: "Wait".to_string(),
                command: "curl -sf localhost".to_string(),
                elapsed_secs: 0,
                timeout_secs: 300,
                first: true,
                preview: false,
            },
            &no_display_ids(),
        );
        assert_eq!(
            lines,
            ["[web-1]   waiting until: curl -sf localhost (up to 300s)"]
        );
    }

    #[test]
    fn skips_are_counted_in_both_summaries() {
        let (_, lines) = event_lines(
            &ExecutorEvent::NodeComplete {
                host: "web-1".to_string(),
                success: true,
                changed: 1,
                skipped: 2,
                dry_run: false,
            },
            &no_display_ids(),
        );
        assert!(lines[0].ends_with("OK (1 changed, 2 skipped)"), "{lines:?}");

        let (_, lines) = event_lines(
            &ExecutorEvent::RunComplete {
                summary: executor::result::RunSummary {
                    total_hosts: 1,
                    succeeded: 1,
                    failed: 0,
                    total_changed: 1,
                    total_skipped: 2,
                    aborted: 0,
                    dry_run: true,
                },
            },
            &no_display_ids(),
        );
        assert!(lines[1].ends_with("1 would change, 2 skipped"), "{lines:?}");
    }

    fn console_host(vars: &[(&str, &str)]) -> config::types::ResolvedHost {
        config::types::ResolvedHost {
            name: "web-1".to_string(),
            address: "10.0.0.1".to_string(),
            user: "deploy".to_string(),
            port: 22,
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            jump: None,
            run_as: Default::default(),
        }
    }

    /// A `Secrets` unlocked by a passphrase, and a token encrypting `plaintext` under it.
    fn unlocked_secrets(plaintext: &str) -> (Arc<glidesh::secrets::Secrets>, String) {
        use glidesh::secrets::config::{Provider, SecretsConfig};
        use glidesh::secrets::passphrase::{PassphraseProvider, generate_dek};
        let dek = generate_dek();
        let wrapped = PassphraseProvider::new("pw".into()).wrap_dek(&dek).unwrap();
        let cfg = SecretsConfig {
            provider: Provider::Passphrase,
            encryptedkey: wrapped,
            recipients: Vec::new(),
        };
        let secrets = glidesh::secrets::Secrets::open(
            Some(&cfg),
            Some(&glidesh::secrets::Identity::Passphrase("pw".into())),
        )
        .unwrap();
        let token = glidesh::secrets::token::encrypt_value(&dek, plaintext.as_bytes()).unwrap();
        (secrets, token)
    }

    /// Off by default: a shell's own `${VAR}` must reach the host untouched.
    #[test]
    fn a_console_command_is_sent_as_typed_without_vars() {
        let host = console_host(&[("HOME", "not-this")]);
        let locked = glidesh::secrets::Secrets::locked();
        let cmd = console_command("echo ${HOME}", &host, &locked, false).unwrap();
        assert_eq!(cmd, "echo ${HOME}");
    }

    #[test]
    fn with_vars_a_console_command_uses_the_host_variables() {
        let host = console_host(&[("app-dir", "/opt/app")]);
        let locked = glidesh::secrets::Secrets::locked();
        let cmd =
            console_command("ls ${app-dir} # on ${@host.name}", &host, &locked, true).unwrap();
        assert_eq!(cmd, "ls /opt/app # on web-1");
    }

    #[test]
    fn with_vars_an_undefined_name_fails_rather_than_sending_half_a_command() {
        let host = console_host(&[]);
        let locked = glidesh::secrets::Secrets::locked();
        let err = console_command("echo ${nope}", &host, &locked, true).unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    /// The field ask: use a secret in a one-off command. The value is decrypted into the
    /// command, and — because decrypting registers it — scrubbed from anything printed.
    #[test]
    fn with_vars_a_secret_is_decrypted_into_the_command_and_redacted_from_output() {
        let (secrets, token) = unlocked_secrets("hunter2-token");
        let host = console_host(&[("api-token", token.as_str())]);
        let cmd = console_command(
            "curl -H 'Authorization: ${api-token}' https://x",
            &host,
            &secrets,
            true,
        )
        .unwrap();
        assert_eq!(cmd, "curl -H 'Authorization: hunter2-token' https://x");
        assert_eq!(
            secrets.registry().redact("echoed: hunter2-token"),
            "echoed: ***"
        );
    }

    #[test]
    fn a_multi_line_secret_is_redacted_before_output_is_split() {
        let (secrets, token) = unlocked_secrets("first-half\nsecond-half");
        let host = console_host(&[("pair", token.as_str())]);
        console_command("echo ${pair}", &host, &secrets, true).unwrap();

        let lines = redacted_lines(
            "before\nfirst-half\nsecond-half\nafter\n",
            &secrets.registry(),
        );
        assert_eq!(lines, ["before", "***", "after"]);
    }

    #[test]
    fn with_vars_an_inline_token_is_decrypted_too() {
        let (secrets, token) = unlocked_secrets("s3cret-value");
        let host = console_host(&[]);
        let cmd = console_command(&format!("echo {token}"), &host, &secrets, true).unwrap();
        assert_eq!(cmd, "echo s3cret-value");
    }

    #[test]
    fn failures_go_to_stderr() {
        let (stream, _) = event_lines(
            &ExecutorEvent::StepFailed {
                host: "web-1".to_string(),
                step: "Deploy".to_string(),
                error: "boom".to_string(),
            },
            &no_display_ids(),
        );
        assert_eq!(stream, OutStream::Err);
    }

    /// A wrapped-key blob with a chosen cost byte. `wrap_cost` reads only that byte, so the
    /// rest need not be real ciphertext.
    fn blob_at(cost: u8) -> String {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        format!("v1:{}", URL_SAFE_NO_PAD.encode([cost, 0, 0, 0]))
    }

    fn passphrase_cfg(encryptedkey: String) -> secrets_config::SecretsConfig {
        secrets_config::SecretsConfig {
            provider: secrets_config::Provider::Passphrase,
            encryptedkey,
            recipients: Vec::new(),
        }
    }

    #[test]
    fn validate_nudges_a_key_wrapped_below_the_current_cost() {
        let current = secret_passphrase::current_cost();
        let weak = provider_detail(&passphrase_cfg(blob_at(current - 1)));
        assert!(weak.contains("secret rekey"), "no nudge: {weak}");
        assert!(weak.contains(&format!("2^{}", current - 1)), "{weak}");

        // At the current cost there is nothing to say beyond the cost itself.
        let fine = provider_detail(&passphrase_cfg(blob_at(current)));
        assert!(fine.contains(&format!("2^{current}")), "{fine}");
        assert!(!fine.contains("rekey"), "spurious nudge: {fine}");
    }

    #[test]
    fn validate_reports_recipient_count_for_age_files() {
        let cfg = secrets_config::SecretsConfig {
            provider: secrets_config::Provider::Age,
            encryptedkey: "agev1:AAAA".to_string(),
            recipients: vec![glidesh::secrets::config::SecretRecipient {
                name: "alice".to_string(),
                key: "ssh-ed25519 AAAA".to_string(),
            }],
        };
        let detail = provider_detail(&cfg);
        assert!(detail.contains("1 recipient(s)"), "{detail}");
        // An age blob has no scrypt cost to report.
        assert!(!detail.contains("scrypt"), "{detail}");
    }

    #[test]
    fn pass_file_flag_outranks_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pass");
        std::fs::write(&path, "from-file\n").unwrap();
        // Parsed the way the CLI parses it, so this also pins the flag's spelling.
        let args =
            cli::RunArgs::try_parse_from(["run", "--secret-pass-file", path.to_str().unwrap()])
                .unwrap();
        // Holds whether or not GLIDESH_SECRET_PASS is set in this environment.
        assert_eq!(
            source_secret_pass(&args.secrets).unwrap().as_deref(),
            Some("from-file")
        );
    }

    /// The pass file does not exist, so reading it would fail the command.
    #[test]
    fn no_secrets_file_means_no_passphrase_is_sourced() {
        let dir = tempfile::tempdir().unwrap();
        let flags = cli::SecretSourceArgs {
            secret_pass_file: Some(dir.path().join("does-not-exist")),
            ..Default::default()
        };
        let parsed = parse_secrets(&flags, dir.path()).unwrap();
        assert!(parsed.vars.is_empty());
        open_secrets(&flags, None, parsed.config.as_ref()).unwrap();
    }

    #[test]
    fn the_mode_flag_overrides_the_plan_in_both_directions() {
        let plan =
            |mode: &str| config::parse_plan(&format!("plan \"p\" {{ mode \"{mode}\" }}")).unwrap();

        let mut p = plan("async");
        apply_mode_override(&mut p, Some("sync"));
        assert_eq!(p.mode, ExecutionMode::Sync);

        let mut p = plan("sync");
        apply_mode_override(&mut p, Some("async"));
        assert_eq!(p.mode, ExecutionMode::Async);

        let mut p = plan("async");
        apply_mode_override(&mut p, None);
        assert_eq!(
            p.mode,
            ExecutionMode::Async,
            "no flag keeps the plan's mode"
        );
    }

    #[test]
    fn a_batch_names_its_hosts() {
        let (stream, lines) = event_lines(
            &ExecutorEvent::BatchStarted {
                index: 1,
                total: 3,
                hosts: vec!["web-3".into(), "web-4".into()],
            },
            &no_display_ids(),
        );
        assert_eq!(stream, OutStream::Out);
        assert_eq!(lines, ["--- Batch 2/3: web-3, web-4 ---"]);
    }

    #[test]
    fn a_stopped_rollout_gives_its_reason_and_every_host_left_out() {
        let (stream, lines) = event_lines(
            &ExecutorEvent::HostsAborted {
                hosts: vec!["web-5".into(), "web-6".into()],
                reason: "3 of 6 hosts failed, more than max-fail 1".into(),
            },
            &no_display_ids(),
        );
        assert_eq!(stream, OutStream::Err);
        assert_eq!(
            lines,
            [
                "--- Rollout stopped: 3 of 6 hosts failed, more than max-fail 1 ---",
                "[web-5] ABORTED (not started)",
                "[web-6] ABORTED (not started)",
            ]
        );
    }

    fn summary_with(failed: usize, aborted: usize) -> executor::result::RunSummary {
        executor::result::RunSummary {
            total_hosts: 6,
            succeeded: 6 - failed - aborted,
            failed,
            total_changed: 1,
            total_skipped: 0,
            aborted,
            dry_run: false,
        }
    }

    #[test]
    fn the_summary_counts_aborted_hosts_only_when_there_are_some() {
        let line = |s| {
            event_lines(
                &ExecutorEvent::RunComplete { summary: s },
                &no_display_ids(),
            )
            .1[1]
                .clone()
        };
        assert_eq!(
            line(summary_with(1, 0)),
            "Hosts: 6 total, 5 ok, 1 failed, 1 changed"
        );
        assert_eq!(
            line(summary_with(2, 3)),
            "Hosts: 6 total, 1 ok, 2 failed, 3 aborted, 1 changed"
        );
    }

    #[test]
    fn aborted_hosts_fail_the_run() {
        assert!(run_failure(&summary_with(0, 0)).is_none());
        let err = run_failure(&summary_with(2, 3)).unwrap().to_string();
        assert!(err.contains("2 host(s) failed, 3 not started"), "{err}");
    }

    #[test]
    fn the_rollout_flags_override_the_plan() {
        let mut plan = config::parse_plan("plan \"p\" {\n serial 5\n max-fail 0\n}").unwrap();
        apply_rollout_override(&mut plan, Some("1, 25%"), Some("10%")).unwrap();
        assert_eq!(
            plan.serial,
            [
                glidesh::config::types::Amount::Count(1),
                glidesh::config::types::Amount::Percent(25)
            ]
        );
        assert_eq!(
            plan.max_fail,
            Some(glidesh::config::types::Amount::Percent(10))
        );

        let mut plan = config::parse_plan("plan \"p\" {\n serial 5\n}").unwrap();
        apply_rollout_override(&mut plan, None, None).unwrap();
        assert_eq!(plan.serial, [glidesh::config::types::Amount::Count(5)]);
    }

    #[test]
    fn a_bad_rollout_flag_is_rejected_like_the_plan_setting() {
        let mut plan = config::parse_plan("plan \"p\" { }").unwrap();
        for sizes in ["0", "", ",", "1,,2"] {
            let err = apply_rollout_override(&mut plan, Some(sizes), None).unwrap_err();
            assert!(
                err.to_string().contains("--serial must be"),
                "{sizes:?}: {err}"
            );
            assert!(
                !matches!(err, GlideshError::ConfigParse { .. }),
                "a flag is not a plan file: {err}"
            );
        }
        let err = apply_rollout_override(&mut plan, None, Some("150%")).unwrap_err();
        assert!(err.to_string().contains("--max-fail must be"), "{err}");
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        assert!(cli::RunArgs::try_parse_from(["run", "-m", "asinc"]).is_err());
    }

    /// `console` takes the same secrets flags as `run`, and `--vars` only with a command.
    #[test]
    fn console_accepts_the_secrets_flags_and_vars() {
        let args = cli::ConsoleArgs::try_parse_from([
            "console",
            "-c",
            "echo ${token}",
            "--vars",
            "--secrets",
            "s.kdl",
            "--secret-pass-file",
            "p",
            "--secret-identity",
            "k",
        ])
        .unwrap();
        assert!(args.vars);
        assert_eq!(
            args.secrets.secrets.as_deref(),
            Some(std::path::Path::new("s.kdl"))
        );
        assert!(cli::ConsoleArgs::try_parse_from(["console", "--vars"]).is_err());
    }
}
