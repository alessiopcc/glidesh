mod help;

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "glidesh",
    version,
    about = "Fast, stateless, SSH-only infrastructure automation",
    after_help = "An overview with example files and a workflow: glidesh --help. Plan syntax \
                  and every module: glidesh run --help.",
    after_long_help = help::TOP_LEVEL
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
    /// Apply a plan to the hosts of an inventory, or run one command on a host
    Run(RunArgs),

    /// Browse past runs and their per-host logs
    Logs(LogsArgs),

    /// Check a plan, an inventory and its secrets file without connecting to any host
    Validate(ValidateArgs),

    /// Browse hosts in a TUI, open a shell on some, or run one command across them
    Console(ConsoleArgs),

    /// Manage encrypted secrets (create, read, edit)
    Secret(SecretArgs),
}

#[derive(Parser, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    pub command: SecretCommand,

    /// SSH private key that unlocks an age-wrapped secrets file
    /// (defaults to $GLIDESH_SECRET_IDENTITY, then ~/.ssh/id_ed25519)
    // Global so it may be given before or after the subcommand: every `secret` command
    // that touches an age-wrapped file needs the same key, and `run` spells it the same way.
    #[arg(long, value_name = "PATH", global = true)]
    pub secret_identity: Option<PathBuf>,
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
    Rm(SecretRmArgs),

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

#[derive(Parser, Debug)]
pub struct SecretRmArgs {
    /// Variable name of the value to delete
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

    /// A group, a host, or group:host; comma-separate several. One host opens a shell,
    /// several a broadcast shell
    #[arg(short, long)]
    pub target: Option<String>,

    /// Run this command on every target and print each host's output, instead of a shell
    #[arg(short, long)]
    pub command: Option<String>,

    /// SSH private key [default: the inventory's `ssh-key` variable, else ~/.ssh/id_ed25519]
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Hosts running --command at once (minimum 1)
    #[arg(long, default_value = "10", value_parser = parse_concurrency)]
    pub concurrency: usize,

    /// Do not verify host keys against ~/.ssh/known_hosts
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Trust and save the key of a host not yet in ~/.ssh/known_hosts (a changed key still
    /// fails)
    #[arg(long)]
    pub accept_new_host_key: bool,

    /// Substitute ${var} references in --command from each host's variables and the secrets
    /// file before running it. Off by default, so a shell's own ${VAR} reaches the host
    /// untouched.
    #[arg(long, requires = "command")]
    pub vars: bool,

    #[command(flatten)]
    pub secrets: SecretSourceArgs,
}

#[derive(Parser, Debug)]
#[command(
    after_help = "Plan syntax, variables, conditions and every module's parameters: \
                  glidesh run --help",
    after_long_help = help::RUN
)]
pub struct RunArgs {
    /// Plan to apply. Without it, each host runs the `plan=` its inventory entry or group names
    #[arg(short, long)]
    pub plan: Option<PathBuf>,

    /// Inventory listing the hosts to run on
    #[arg(short, long)]
    pub inventory: Option<PathBuf>,

    /// Limit the run to a group, a host, or group:host; comma-separate several
    #[arg(short, long)]
    pub target: Option<String>,

    /// Run on this one address instead of an inventory, with --plan or --command
    #[arg(long, value_parser = parse_host)]
    pub host: Option<String>,

    /// SSH user for --host (inventory hosts set their own) [default: root]
    #[arg(short, long)]
    pub user: Option<String>,

    /// SSH port for --host (inventory hosts set their own)
    #[arg(short = 'P', long, default_value = "22")]
    pub port: u16,

    /// SSH private key [default: the inventory's `ssh-key` variable, else ~/.ssh/id_ed25519]
    #[arg(short, long)]
    pub key: Option<PathBuf>,

    /// Run this one command on --host instead of a plan. For inventory hosts, use
    /// `glidesh console -t <target> -c <command>`
    #[arg(short, long, requires = "host", conflicts_with = "plan")]
    pub command: Option<String>,

    /// Execution mode, overriding the plan's `mode`: sync (hosts move through the steps
    /// together) or async (each host runs at its own pace). Default: the plan's, else sync
    #[arg(short, long, value_parser = ["sync", "async"])]
    pub mode: Option<String>,

    /// Run only the steps tagged with one of these (comma-separated), plus steps tagged
    /// `always`. A tag no step carries is an error
    #[arg(long, value_name = "TAGS", conflicts_with = "command")]
    pub tags: Option<String>,

    /// Skip the steps tagged with any of these (comma-separated), even `always` ones or ones
    /// --tags selects. A tag no step carries is an error
    #[arg(long, value_name = "TAGS", conflicts_with = "command")]
    pub skip_tags: Option<String>,

    /// Roll out in batches, overriding the plan's `serial`: comma-separated host counts or
    /// percentages, used in order with the last repeating (e.g. "1,25%")
    #[arg(long, value_name = "SIZES")]
    pub serial: Option<String>,

    /// Stop starting batches once more hosts than this have failed, overriding the plan's
    /// `max-fail`: a count or a percentage of all hosts (e.g. "10%")
    #[arg(long, value_name = "N|N%")]
    pub max_fail: Option<String>,

    /// Answer the plan's `vars-prompt` for NAME instead of being asked. Repeatable. Only
    /// names the plan prompts for are accepted. Needed for each prompt without a default when
    /// stdin is not a terminal
    #[arg(long = "var", value_name = "NAME=VALUE", conflicts_with = "command")]
    pub vars: Vec<String>,

    /// Hosts worked on at once (minimum 1)
    #[arg(long, default_value = "10", value_parser = parse_concurrency)]
    pub concurrency: usize,

    /// Report what would change without applying it. Note that read-only probes still
    /// run on the target: `shell check=` guards, container readiness gates, and runtime
    /// detection all execute, since that is how the preview is computed.
    #[arg(long)]
    pub dry_run: bool,

    /// Show the detail behind each pending change: a content diff for `file`, the drifted
    /// parameters for `container`. Works with or without --dry-run; on a real run the detail
    /// goes to the run log. May cost extra round trips.
    #[arg(long)]
    pub diff: bool,

    /// Plain text output instead of the TUI (automatic when output is not a terminal)
    #[arg(short = 'T', long)]
    pub no_tui: bool,

    /// Do not verify host keys against ~/.ssh/known_hosts
    #[arg(long)]
    pub no_host_key_check: bool,

    /// Trust and save the key of a host not yet in ~/.ssh/known_hosts (a changed key still
    /// fails)
    #[arg(long)]
    pub accept_new_host_key: bool,

    /// Run tasks as this user, e.g. root. A `run-as` in the inventory or plan overrides it
    #[arg(long)]
    pub run_as: Option<String>,

    /// How to become the --run-as user: sudo (default), doas, or su
    #[arg(long)]
    pub run_as_method: Option<String>,

    /// Prompt for the escalation password (otherwise read from GLIDESH_RUNAS_PASS)
    #[arg(long)]
    pub ask_pass: bool,

    #[command(flatten)]
    pub secrets: SecretSourceArgs,
}

/// Where the secrets file is and how to unlock it. Shared by `run` and `console`.
#[derive(Args, Debug, Default)]
pub struct SecretSourceArgs {
    /// Secrets file [default: $GLIDESH_SECRETS, else secrets.kdl next to the inventory, else in
    /// the current directory]
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
#[command(
    after_help = "Without --last or --run, opens a browser on a terminal and lists recent runs \
                  otherwise. Runs are kept in ~/.glidesh/runs/."
)]
pub struct LogsArgs {
    /// Print the most recent run
    #[arg(long)]
    pub last: bool,

    /// Print only this host's log
    #[arg(long, value_name = "HOST")]
    pub node: Option<String>,

    /// Print the run whose directory name contains this text, such as a timestamp or plan name
    #[arg(long, value_name = "TEXT")]
    pub run: Option<String>,
}

#[derive(Parser, Debug)]
#[command(
    after_help = "Checks only the files given: --plan, --inventory, or both. With --inventory \
                  and no --plan, the plans it names with plan= are checked as `run -i` would \
                  load them. A secrets file is checked too: $GLIDESH_SECRETS, else secrets.kdl next to the inventory, else in \
                  the current directory. Reports every problem found, not only the first, and \
                  exits non-zero if any. To check a plan against real hosts without changing \
                  them, use `glidesh run --dry-run`.\n\n\
                  Docs: https://glidesh.netlify.app/cli/#glidesh-validate"
)]
pub struct ValidateArgs {
    /// Plan to check: syntax, includes, modules, `file` sources and unexpanded `${var}`s
    #[arg(short, long)]
    pub plan: Option<PathBuf>,

    /// Inventory to check, with the secrets file beside it. With --plan, its variables
    /// count as defined; without --plan, the plans it names with plan= are checked too
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

/// The address is also the label on every output line, so whitespace or a control
/// character (which no address holds) would let it break a `[host]` line in two.
fn parse_host(s: &str) -> Result<String, String> {
    if s.is_empty() {
        return Err("the address is empty".to_string());
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("an address cannot contain whitespace or control characters".to_string());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn a_host_address_with_a_line_break_or_space_is_refused() {
        assert_eq!(parse_host("10.0.0.5").unwrap(), "10.0.0.5");
        assert_eq!(
            parse_host("web-1.example.com").unwrap(),
            "web-1.example.com"
        );
        for bad in ["", "web-1\n[db-1] OK", "web 1", "web\r"] {
            assert!(parse_host(bad).is_err(), "{bad:?}");
        }
    }

    fn long_help(path: &[&str]) -> String {
        let mut cmd = Cli::command();
        let mut sub = &mut cmd;
        for name in path {
            sub = sub.find_subcommand_mut(name).unwrap();
        }
        sub.render_long_help().to_string()
    }

    #[test]
    fn the_cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// A user reading `--help` asked for drift detection, `creates`/`unless` and retries,
    /// all of which already existed. Keep them named where that user looked.
    #[test]
    fn run_help_names_the_features_users_could_not_find() {
        let help = long_help(&["run"]);
        for needle in [
            "sh.glide.param-hash",
            "check=",
            "Assertion: the condition in check=",
            "retries",
            "timeout",
            "success_codes",
            "changed-when",
            "--diff",
            "serial",
            "PLAN SYNTAX",
            "CONDITIONS",
            "${@os.family}",
            "${@fact.cpu.count}",
            "TAGS",
            "tags=",
            "--skip-tags",
            "SUBSCRIBE",
            "until=",
            "until-timeout",
            "RESCUE AND ALWAYS",
            "${@error.msg}",
            "vars-prompt",
            "secret=#true",
            "--var NAME=VALUE",
            "keeps its owner/group/mode",
            "refuses a destination others could redirect",
            "sudo and doas work for any user",
            "resolve from the plan file's directory",
            "Plan vars are defaults",
            "$${NAME} writes a literal ${NAME}",
            "Any other parameter on a built-in module is an error",
            "`jump #false`",
            "prune=#true removes host paths",
            "dir-mode= file-mode=",
            "`[<host>]`, the inventory host name",
        ] {
            assert!(help.contains(needle), "run --help lacks {needle}:\n{help}");
        }
    }

    /// `--help` must stand alone: agents read it and never open the docs site.
    #[test]
    fn top_level_help_explains_the_files_and_the_workflow() {
        let help = Cli::command().render_long_help().to_string();
        for needle in [
            "inventory.kdl:",
            "plan.kdl:",
            "--dry-run --diff",
            "--accept-new-host-key",
            "GLIDESH_SECRET_PASS",
            "--var name=value",
            "exits non-zero",
            "glidesh run --help",
        ] {
            assert!(
                help.contains(needle),
                "glidesh --help lacks {needle}:\n{help}"
            );
        }
    }

    /// Every built-in module is named in `run --help`, so none can be added without it.
    #[test]
    fn run_help_names_every_module() {
        let help = long_help(&["run"]);
        let registry = glidesh::modules::ModuleRegistry::new();
        for module in registry.builtin_names().chain(["host"]) {
            assert!(
                help.contains(&format!("  {module} \"")),
                "run --help lacks {module}"
            );
        }
    }

    /// Doc comments become help text, so an implementation note must not be one.
    #[test]
    fn secret_help_carries_no_implementation_notes() {
        assert!(!long_help(&["secret"]).contains("Global so"));
    }

    #[test]
    fn secret_rm_does_not_offer_to_decrypt() {
        assert!(!long_help(&["secret", "rm"]).contains("decrypt"));
    }

    /// `-c` used to be silently ignored without `--host`, and silently won over `--plan`.
    #[test]
    fn tags_do_not_apply_to_an_adhoc_command() {
        let parsed = Cli::try_parse_from([
            "glidesh", "run", "--host", "h", "-c", "uptime", "--tags", "web",
        ]);
        assert!(parsed.is_err());
    }

    #[test]
    fn an_adhoc_command_needs_a_host_and_no_plan() {
        let parse = |args: &[&str]| Cli::try_parse_from([&["glidesh", "run"], args].concat());
        assert!(parse(&["-i", "inv.kdl", "-c", "uptime"]).is_err());
        assert!(parse(&["--host", "h", "-p", "plan.kdl", "-c", "uptime"]).is_err());
        assert!(parse(&["--host", "h", "-c", "uptime"]).is_ok());
    }
}
