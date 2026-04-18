use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "glidesh",
    version,
    about = "Fast, stateless, SSH-only infrastructure automation"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Execute a plan against target hosts
    Run(RunArgs),

    /// View logs from past runs
    Logs(LogsArgs),

    /// Validate configuration files
    Validate(ValidateArgs),

    /// Open an interactive shell or run a command on target hosts
    Shell(ShellArgs),

    /// Open the interactive connection console (groups, shells, tunnels)
    Console(ConsoleArgs),
}

#[derive(Parser, Debug, Default)]
pub struct ConsoleArgs {
    /// Path to the inventory file (defaults to ./inventory.kdl)
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,

    /// SSH private key path
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Skip SSH host key verification
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Accept and save new host keys to known_hosts
    #[arg(long)]
    pub accept_new_host_key: bool,
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Path to the plan file
    #[arg(short, long)]
    pub plan: Option<PathBuf>,

    /// Path to the inventory file
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,

    /// Target filter: group name, host name, or group:hostname
    #[arg(short, long)]
    pub target: Option<String>,

    /// Single host to connect to (ad-hoc mode)
    #[arg(long)]
    pub host: Option<String>,

    /// SSH user
    #[arg(short, long)]
    pub user: Option<String>,

    /// SSH port
    #[arg(short = 'P', long, default_value = "22")]
    pub port: u16,

    /// SSH private key path
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Ad-hoc command to run
    #[arg(short, long)]
    pub command: Option<String>,

    /// Execution mode: sync or async
    #[arg(short, long, default_value = "sync")]
    pub mode: String,

    /// Max concurrent hosts (minimum 1)
    #[arg(long, default_value = "10", value_parser = parse_concurrency)]
    pub concurrency: usize,

    /// Dry run (check only, no changes)
    #[arg(long)]
    pub dry_run: bool,

    /// Disable TUI and use plain text output
    #[arg(long)]
    pub no_tui: bool,

    /// Skip SSH host key verification
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Accept and save new host keys to known_hosts
    #[arg(long)]
    pub accept_new_host_key: bool,
}

#[derive(Parser, Debug)]
pub struct LogsArgs {
    /// Show the last run
    #[arg(long)]
    pub last: bool,

    /// Filter by node name
    #[arg(long)]
    pub node: Option<String>,

    /// Specific run directory
    #[arg(long)]
    pub run: Option<String>,
}

#[derive(Parser, Debug)]
pub struct ShellArgs {
    /// Path to the inventory file
    #[arg(short, long)]
    pub inventory: PathBuf,

    /// Target filter: group name, host name, or group:hostname
    #[arg(short, long)]
    pub target: Option<String>,

    /// Command to run (if omitted, opens interactive shell for single host)
    #[arg(short, long)]
    pub command: Option<String>,

    /// SSH private key path
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Max concurrent hosts (minimum 1)
    #[arg(long, default_value = "10", value_parser = parse_concurrency)]
    pub concurrency: usize,

    /// Skip SSH host key verification
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Accept and save new host keys to known_hosts
    #[arg(long)]
    pub accept_new_host_key: bool,
}

#[derive(Parser, Debug)]
pub struct ValidateArgs {
    /// Path to the plan file
    #[arg(short, long)]
    pub plan: Option<PathBuf>,

    /// Path to the inventory file
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,
}

fn parse_concurrency(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|e| format!("{}", e))?;
    if n == 0 {
        return Err("concurrency must be at least 1".to_string());
    }
    Ok(n)
}
