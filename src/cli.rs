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
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Execute a plan against target hosts
    Run(RunArgs),

    /// View logs from past runs
    Logs(LogsArgs),

    /// Validate configuration files
    Validate(ValidateArgs),
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

    /// Max concurrent hosts
    #[arg(long, default_value = "10")]
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

    /// Additional directories to search for external modules
    #[arg(long = "module-path", value_name = "DIR")]
    pub module_paths: Vec<std::path::PathBuf>,
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
pub struct ValidateArgs {
    /// Path to the plan file
    #[arg(short, long)]
    pub plan: Option<PathBuf>,

    /// Path to the inventory file
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,
}
