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
// `Run` carries far more flags than the other subcommands, but clap's derive needs each
// variant to hold its `Args` struct directly — `Box<RunArgs>` does not implement `Args`,
// so the size spread cannot be boxed away.
#[allow(clippy::large_enum_variant)]
pub enum Commands {
    /// Execute a plan against target hosts
    Run(RunArgs),

    /// View logs from past runs
    Logs(LogsArgs),

    /// Validate configuration files
    Validate(ValidateArgs),

    /// Connection console: TUI when no target/command, otherwise shell or one-shot exec
    Console(ConsoleArgs),

    /// Manage encrypted secrets (create, read, edit)
    Secret(SecretArgs),
}

#[derive(Parser, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,
}

#[derive(Subcommand, Debug)]
pub enum SecretCommand {
    /// Initialize a secrets file: generate and wrap a data key
    Init(SecretInitArgs),

    /// Encrypt a value and store it under a key (prompts for the value if omitted)
    Set(SecretSetArgs),

    /// Decrypt and print a stored value, or a `secret:v1:…` token given directly
    Get(SecretKeyArgs),

    /// Decrypt and print a stored value or a token (alias for `get`)
    Decrypt(SecretKeyArgs),

    /// List the names in a secrets file (no passphrase needed, never prints values)
    List(SecretFileArgs),

    /// Delete a value from a secrets file
    #[command(alias = "remove")]
    Rm(SecretKeyArgs),

    /// Read plaintext on stdin and print a `secret:v1:…` token for pasting inline
    Encrypt(SecretFileArgs),

    /// Re-wrap the data key under a new passphrase (value tokens are unchanged)
    Rekey(SecretRekeyArgs),

    /// Open the secrets file in $VISUAL/$EDITOR with values transiently decrypted
    Edit(SecretFileArgs),

    /// Manage who can unlock an age-wrapped secrets file
    Recipients(RecipientsArgs),
}

/// Key-wrapping provider for a new secrets file.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderArg {
    /// One shared passphrase unlocks the file
    Passphrase,
    /// The data key is wrapped to SSH public keys; each person unlocks with their own
    Age,
}

#[derive(Parser, Debug)]
pub struct SecretInitArgs {
    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,

    /// How the data key is wrapped
    #[arg(long, value_enum, default_value = "passphrase")]
    pub provider: ProviderArg,

    /// SSH public key, or path to a .pub file, that may unlock this file. Repeatable;
    /// required by --provider age
    #[arg(long = "recipient", value_name = "KEY_OR_PATH")]
    pub recipients: Vec<String>,
}

#[derive(Parser, Debug)]
pub struct RecipientsArgs {
    #[command(subcommand)]
    pub command: RecipientsCommand,
}

#[derive(Subcommand, Debug)]
pub enum RecipientsCommand {
    /// List who can unlock the file
    List(SecretFileArgs),

    /// Grant access to another SSH public key
    Add(RecipientAddArgs),

    /// Revoke a recipient, rotating the data key so their old copy is useless
    #[command(alias = "remove")]
    Rm(RecipientRmArgs),
}

#[derive(Parser, Debug)]
pub struct RecipientAddArgs {
    /// SSH public key, or path to a .pub file
    pub recipient: String,

    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,
}

#[derive(Parser, Debug)]
pub struct RecipientRmArgs {
    /// Name of the recipient to remove, as shown by `recipients list`
    pub name: String,

    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,

    /// Keep the existing data key. Faster and a smaller diff, but the removed recipient
    /// can still decrypt every value with the copy they already have
    #[arg(long)]
    pub keep_data_key: bool,
}

#[derive(Parser, Debug)]
pub struct SecretFileArgs {
    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,
}

#[derive(Parser, Debug)]
pub struct SecretRekeyArgs {
    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,

    /// Also generate a new data key and re-encrypt every value under it
    #[arg(long)]
    pub rotate_data_key: bool,

    /// Read the new passphrase from the first line of a file (otherwise prompt)
    #[arg(long, value_name = "PATH")]
    pub new_pass_file: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct SecretSetArgs {
    /// Variable name to store the secret under
    pub key: String,

    /// The secret value (omit to be prompted without echo)
    pub value: Option<String>,

    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,
}

#[derive(Parser, Debug)]
pub struct SecretKeyArgs {
    /// Variable name, or a `secret:v1:…` token to decrypt in place
    pub key: String,

    /// Path to the secrets file
    #[arg(short, long, default_value = "secrets.kdl")]
    pub file: PathBuf,
}

#[derive(Parser, Debug, Default)]
pub struct ConsoleArgs {
    /// Path to the inventory file (defaults to ./inventory.kdl)
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,

    /// Target filter: group name, host name, or group:hostname
    #[arg(short, long)]
    pub target: Option<String>,

    /// Command to run (if set, skips TUI and executes on resolved targets)
    #[arg(short, long)]
    pub command: Option<String>,

    /// SSH private key path
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Max concurrent hosts when running a command (minimum 1)
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
    #[arg(short = 'T', long)]
    pub no_tui: bool,

    /// Skip SSH host key verification
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Accept and save new host keys to known_hosts
    #[arg(long)]
    pub accept_new_host_key: bool,

    /// Default escalation target user (e.g. root). Inventory/plan can override.
    #[arg(long)]
    pub run_as: Option<String>,

    /// Default escalation method: sudo (default), doas, or su
    #[arg(long)]
    pub run_as_method: Option<String>,

    /// Prompt for the escalation password (otherwise read from GLIDESH_RUNAS_PASS)
    #[arg(long)]
    pub ask_pass: bool,

    /// Path to the secrets file (defaults to secrets.kdl next to the inventory)
    #[arg(long)]
    pub secrets: Option<PathBuf>,

    /// Prompt for the secrets passphrase (otherwise read from GLIDESH_SECRET_PASS)
    #[arg(long)]
    pub ask_secret_pass: bool,

    /// Read the secrets passphrase from the first line of a file (for CI)
    #[arg(long, value_name = "PATH")]
    pub secret_pass_file: Option<PathBuf>,

    /// SSH private key that unlocks an age-wrapped secrets file (defaults to --key)
    #[arg(long, value_name = "PATH")]
    pub secret_identity: Option<PathBuf>,
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

fn parse_concurrency(s: &str) -> Result<usize, String> {
    let n: usize = s.parse().map_err(|e| format!("{}", e))?;
    if n == 0 {
        return Err("concurrency must be at least 1".to_string());
    }
    Ok(n)
}
