---
title: CLI Reference
description: Complete command reference for glidesh.
---

## `glidesh` (no subcommand)

Running `glidesh` with no subcommand opens the [interactive console](/cli/console/) against `./inventory.kdl` if one exists in the current directory. Equivalent to `glidesh console`.

```bash
cd my-fleet/
glidesh                       # opens the console TUI
```

If no inventory is present in the working directory, glidesh exits with an error suggesting `--inventory <path>`.

## `glidesh console`

Connection console: opens the interactive TUI when invoked with no `--target` and no `--command`; otherwise behaves like a shell — interactive PTY for a single host, broadcast TUI for multiple hosts, or one-shot exec when `--command` is set. See the dedicated [Console](/cli/console/) page for full details on the TUI.

```
glidesh console [OPTIONS]
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--inventory <PATH>` | `-i` | Path to the inventory file | `./inventory.kdl` |
| `--target <NAME>` | `-t` | A group, a host, or `group:host`; comma-separate several | — |
| `--command <CMD>` | `-c` | Run this command on every target and print each host's output, instead of a shell | — |
| `--key <PATH>` | `-k` | SSH private key — see [SSH Key Resolution](#ssh-key-resolution) | `~/.ssh/id_ed25519` |
| `--concurrency <N>` | — | Max concurrent hosts when running a command (minimum 1) | `10` |
| `--no-host-key-check` | — | Do not verify host keys against `~/.ssh/known_hosts` | `false` |
| `--accept-new-host-key` | — | Trust and save the key of a host not yet in `~/.ssh/known_hosts`; a changed key still fails | `false` |
| `--vars` | — | Substitute `${var}` references in `--command` from host variables and secrets (see [Variables and secrets](/cli/console/#variables-and-secrets---vars)) | `false` |
| `--secrets <PATH>` | — | Path to the secrets file (used with `--vars`) | `GLIDESH_SECRETS`, else `secrets.kdl` next to the inventory, else in the current directory |
| `--ask-secret-pass` | — | Prompt for the secrets passphrase | `false` |
| `--secret-pass-file <PATH>` | — | Read the secrets passphrase from the first line of a file | — |
| `--secret-identity <PATH>` | — | SSH private key that unlocks an age-wrapped secrets file | `--key` |

### Mode selection

| `--target` | `--command` | Behavior |
|------------|-------------|----------|
| —          | —           | Console TUI (requires a TTY) |
| single host resolved | — | Interactive PTY shell |
| multiple hosts resolved | — | Broadcast group shell TUI |
| any        | set         | Run command, stream `[hostname]`-prefixed output |

### Examples

Interactive PTY on a single host:

```bash
glidesh console -i inventory.kdl -t web-1
```

Run a command across a group, stream prefixed output:

```bash
glidesh console -i inventory.kdl -t web -c "df -h /"
```

```
[web-1] /dev/sda1  50G  40G  10G  80% /
[web-2] /dev/sda1  50G  25G  25G  50% /
[web-3] /dev/sda1  50G  45G   5G  90% /
```

Broadcast TUI across a group (no `-c`):

```bash
glidesh console -i inventory.kdl -t web
```

The console resolves SSH keys using the same [resolution order](#ssh-key-resolution) as `run`.

## `glidesh run`

Apply a plan to the hosts of an inventory, or run one command on a single host. Every
task checks the host before changing it — see [Idempotency & Drift](/concepts/idempotency/)
for what each module compares. `glidesh run --help` lists examples and those rules too.

```
glidesh run [OPTIONS]
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--plan <PATH>` | `-p` | Plan to apply | each host's [`plan=`](#inventory-linked-plans) |
| `--inventory <PATH>` | `-i` | Inventory listing the hosts to run on | — |
| `--target <NAME>` | `-t` | Target filter: group name, host name, `group:hostname`, or a comma-separated list of any of these | — |
| `--host <ADDR>` | — | Run on this one address instead of an inventory, with `--plan` or `--command` | — |
| `--user <USER>` | `-u` | SSH user for `--host` (inventory hosts set their own) | `root` |
| `--port <PORT>` | `-P` | SSH port for `--host` (inventory hosts set their own) | `22` |
| `--key <PATH>` | `-k` | SSH private key — see [SSH Key Resolution](#ssh-key-resolution) | `~/.ssh/id_ed25519` |
| `--command <CMD>` | `-c` | Run this one command on `--host` instead of a plan ([ad-hoc mode](#ad-hoc-mode)) | — |
| `--mode <MODE>` | `-m` | [Execution mode](/concepts/execution-modes/): `sync` or `async`, overriding the plan's `mode` | the plan's `mode`, else `sync` |
| `--serial <SIZES>` | — | [Roll out in batches](/concepts/execution-modes/#rolling-deploys), overriding the plan's `serial`: comma-separated counts or percentages, e.g. `1,25%` | the plan's `serial`, else one batch |
| `--max-fail <N\|N%>` | — | Stop starting batches once more hosts than this have failed, overriding the plan's `max-fail` | the plan's `max-fail`, else stop only when a whole batch fails |
| `--concurrency <N>` | — | Max concurrent hosts | `10` |
| `--dry-run` | — | Report what would change without applying it | `false` |
| `--diff` | — | Show the detail behind each pending change, where the module can describe it | `false` |
| `--no-tui` | `-T` | Plain text output instead of the TUI (automatic when output is not a terminal) | `false` |
| `--no-host-key-check` | — | Do not verify host keys against `~/.ssh/known_hosts` | `false` |
| `--accept-new-host-key` | — | Trust and save the key of a host not yet in `~/.ssh/known_hosts`; a changed key still fails | `false` |
| `--run-as <USER>` | — | Run tasks as this user, e.g. `root`; a [`run-as`](/advanced/run-as/) in the inventory or plan overrides it | — |
| `--run-as-method <METHOD>` | — | How to become the `--run-as` user: `sudo`, `doas`, or `su` | `sudo` |
| `--ask-pass` | — | Prompt for the escalation password (else `GLIDESH_RUNAS_PASS`) | `false` |
| `--secrets <PATH>` | — | Path to the secrets file | `GLIDESH_SECRETS`, else `secrets.kdl` next to the inventory, else in the current directory |
| `--ask-secret-pass` | — | Prompt for the secrets passphrase (else `GLIDESH_SECRET_PASS`) | `false` |
| `--secret-pass-file <PATH>` | — | Read the secrets passphrase from the first line of a file | — |
| `--secret-identity <PATH>` | — | SSH private key that unlocks an age-wrapped secrets file | `--key`, else `~/.ssh/id_ed25519` |

### Previewing a run

`--dry-run` reports what *would* change without applying anything. Each task is labelled
`would change` or `ok`, and a task whose check found work outstanding is preceded by the
reason it gave — `Recreate container lmcache (configuration changed)`, `Upload app.conf ->
/etc/app.conf`. The closing summary counts what would change, not what did.

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run
```

Add `--diff` for the detail behind each pending change, where the module can describe it:

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run --diff
```

```
[web:web-1]   file '/etc/app.conf': would change
[web:web-1]     stdout | Upload app.conf -> /etc/app.conf
[web:web-1]     stdout | --- /etc/app.conf (host)
[web:web-1]     stdout | +++ /etc/app.conf (plan)
[web:web-1]     stdout | @@ -1,2 +1,2 @@
[web:web-1]     stdout |  name=app
[web:web-1]     stdout | -port=80
[web:web-1]     stdout | +port=8080
[web:web-1]     stdout | [dry-run] Would copy app.conf -> /etc/app.conf
```

Two modules describe their changes: [`file`](/modules/file/#--diff) shows a unified diff
of the content, and [`container`](/modules/container/#--diff) names the parameters that
drifted. [External modules](/advanced/writing-plugins/) may return a diff of their own.

`--diff` works on a real run too — the detail behind a change is as useful once the
change is made. There it goes to the [run log](/concepts/logs/) rather than the console,
like the rest of a task's output. It may cost extra round trips: `file` downloads the
destination to diff it.

In [ad-hoc mode](#ad-hoc-mode) there is no desired state to compare against, only a
command, so `--dry-run` prints the command it would have run and connects to nothing.

:::caution
A preview is read-only with respect to *desired state*, but it is not a no-op on the
target: computing it runs the read-only probes a plan asks for. That includes `shell`
`check=` guards, container readiness gates, and container-runtime detection. Nothing is
installed, written, or restarted, but those commands do execute.
:::

Four details worth knowing:

- `register` captures an empty value in a dry run, because the task's own command never
  ran. A later `loop="${var}"` over a registered variable therefore iterates zero times,
  and a step that depends on a registered value cannot be meaningfully previewed.
- A [`when=`](/advanced/conditionals/) that reads a registered value cannot be answered for
  the same reason, so the task is reported as `skipped (undetermined in preview: …)` rather
  than decided against an empty string. Every other condition is evaluated exactly as the
  real run would evaluate it.
- A `shell` task with no `check=` guard has no state to compare against, so it always
  reports `would change`. Give it a guard and it reports `ok` whenever the guard
  succeeds.
- A step using [`subscribe`](/advanced/subscribe/) whose target changed is reported as
  `would change` whatever its own check says, and shows no reason line, because the reason
  it runs is the step it subscribes to rather than its own state. A real run would run it,
  so the preview counts it.

### SSH Key Resolution

The SSH private key is resolved in this order (first match wins):

1. `--key` CLI flag
2. `ssh-key` variable from the inventory (global, group, or host `vars`)
3. `~/.ssh/id_ed25519` (default)

### Ad-hoc mode

Run a single command on a host without a plan or inventory:

```bash
glidesh run --host 192.168.1.10 -u deploy -c "uptime"
```

`--command` needs `--host` and cannot be combined with `--plan`. To run a command on
inventory hosts, use the console: `glidesh console -i inventory.kdl -t web -c "uptime"`.

### Plan mode

Run a plan against an inventory:

```bash
glidesh run -i inventory.kdl -p plan.kdl
```

Filter to a specific group or host:

```bash
glidesh run -i inventory.kdl -p plan.kdl -t web
glidesh run -i inventory.kdl -p plan.kdl -t web-1
```

Run on an arbitrary subset by passing a comma-separated list of targets — each token can be a group name, a host name, or `group:host`:

```bash
glidesh run -i inventory.kdl -p plan.kdl -t web-1,web-3,db-1
```

When `--plan` is omitted, each resolved target uses its own `plan=` (host-level wins over group-level); targets without an associated plan are skipped.

### Ad-hoc host with a plan

Combine `--host` with `--plan` to run a plan against a single host without an inventory file:

```bash
glidesh run --host 192.168.1.10 -u deploy -p plan.kdl
```

The host uses the `--user` (default `root`) and `--port` (default `22`) flags. Plan vars are applied as usual.

### Inventory-linked plans

When `--plan` is omitted but `--inventory` is provided, glidesh runs the `plan=` attributes defined in the inventory (per-group or per-host). See [Inline Plans](/concepts/inventory/#inline-plans).

```bash
glidesh run -i inventory.kdl
```

## `glidesh logs`

Browse past runs and their per-host logs. Logs are stored in `~/.glidesh/runs/`. Without
`--last` or `--run`, glidesh opens a log browser on a terminal and lists the 20 most recent
runs otherwise.

```
glidesh logs [OPTIONS]
```

| Flag | Description | Default |
|------|-------------|---------|
| `--last` | Print the most recent run | `false` |
| `--node <HOST>` | Print only this host's log | every host |
| `--run <TEXT>` | Print the run whose directory name contains this text, such as a timestamp or plan name | — |

```bash
glidesh logs --last
glidesh logs --last --node web-1
glidesh logs --run 20250115_143022_setup
```

## `glidesh validate`

Validate configuration files without executing anything.

```
glidesh validate [OPTIONS]
```

| Flag | Short | Description |
|------|-------|-------------|
| `--plan <PATH>` | `-p` | Validate a plan file |
| `--inventory <PATH>` | `-i` | Validate an inventory file |

```bash
glidesh validate -p plan.kdl
glidesh validate -i inventory.kdl
glidesh validate -p plan.kdl -i inventory.kdl
```

A plan is loaded exactly as `run` loads it, then checked for everything that can be known
without contacting a host:

- **Syntax**, including [`when=`](/advanced/conditionals/) conditions, unknown step attributes,
  and the `mode`, [`serial` and `max-fail`](/concepts/execution-modes/#rolling-deploys) values.
- **Includes and `vars-file`** are resolved, so a missing or broken included plan is reported.
- **Step names** are unique across includes, and every `subscribe` names an earlier step.
- **Modules exist.** A misspelled module name fails here. External modules are looked up next
  to the inventory when `-i` is given, otherwise in `./modules/` and `~/.glidesh/modules/`.
- **Every `file` task has a `src`, and local sources exist.** A `src` is resolved from the
  directory of the plan the task is written in — an [included plan](/advanced/plan-includes/#path-resolution)'s
  own — as a run resolves it. Not checked: a `fetch` source, which is a path on the host, and
  a `src` containing `${…}`, which only a run can resolve.

Every problem is listed, not only the first, and the command exits non-zero if there is any.

With `-i` alone, the inventory and its secrets file are parsed, but the plans its hosts name
with `plan=` are not checked; pass each one with `-p`.

It also **warns**, without failing, when a `file` upload without `template #true` contains
`${name}` for a variable a run would define — a plan variable, any host's inventory variable
when `-i` is given, a secrets-file name, or a built-in such as `${@host.name}`. See
[Forgetting `template`](/modules/file/#forgetting-template).

`validate` never connects, so it cannot tell whether a plan's settings suit a particular host —
whether a package exists in its repositories, a service is installed, or a container would be
recreated. Use [`--dry-run`](#previewing-a-run) for that: it checks each task against the host
without changing anything.

A `secrets.kdl` discovered beside the inventory is parsed too, so a malformed provider block or an
unknown provider is caught here rather than mid-run. Only the file is parsed — validation never
asks for the passphrase and never decrypts a value.

## `glidesh secret`

Manage encrypted secrets. See [Secrets](/concepts/secrets/) for the full workflow and how
values are decrypted and redacted at run time.

```
glidesh secret <COMMAND>
```

| Command | Description |
|---------|-------------|
| `init` | Create a secrets file and generate + wrap a data key. `--provider age --recipient <KEY_OR_PATH>` wraps it to SSH public keys instead of a passphrase (repeatable) |
| `list` | List the names in the file and whether each is encrypted (no passphrase needed) |
| `set <KEY> [VALUE]` | Encrypt a value under a key (prompts for the value if omitted) |
| `get <KEY>` | Decrypt and print a stored value. Pass a `secret:v1:…` token instead of a key to decrypt it directly |
| `decrypt <KEY>` | Alias for `get` |
| `rm <KEY>` | Delete a value from the file (alias: `remove`) |
| `encrypt` | Read plaintext on stdin, print a `secret:v1:…` token |
| `rekey` | Re-wrap the data key under a new passphrase (value tokens unchanged) |
| `rekey --rotate-data-key` | Generate a new data key and re-encrypt every value in the file under it |
| `recipients list` | Show who can unlock an age-wrapped file (no key needed) |
| `recipients add <KEY_OR_PATH>` | Grant access to another SSH public key |
| `recipients rm <NAME>` | Revoke a recipient, rotating the data key unless `--keep-data-key` |
| `edit` | Open the secrets file in `$VISUAL`/`$EDITOR` with values transiently decrypted |

Every subcommand accepts `--file <PATH>` (default `secrets.kdl`). The passphrase comes from
`GLIDESH_SECRET_PASS`, then `GLIDESH_SECRET_PASS_FILE`, then an interactive prompt. `list` and
`rm` need no passphrase at all. `rekey` takes the *new* passphrase from `--new-pass-file <PATH>`
when given, since the ordinary sources already hold the current one. An age-wrapped file takes an
SSH private key instead of a passphrase: `--secret-identity <PATH>` is accepted by every `secret`
subcommand and may be given before or after it, falling back to `GLIDESH_SECRET_IDENTITY` and then
`~/.ssh/id_ed25519`.

```bash
glidesh secret get db-password --secret-identity ~/.ssh/work_ed25519
```

```bash
glidesh secret init
glidesh secret set db-password
GLIDESH_SECRET_PASS=… glidesh secret get db-password
```

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Control log verbosity. Default is `glidesh=info`. Set to `glidesh=debug` or `glidesh=trace` for troubleshooting. |
| `GLIDESH_SECRET_PASS` | Secrets passphrase, for non-interactive `run` / `secret` commands (else `--ask-secret-pass` / prompt). |
| `GLIDESH_SECRET_PASS_FILE` | Path to a file whose first line is the secrets passphrase. Honoured by every subcommand; outranked by `--secret-pass-file` and `GLIDESH_SECRET_PASS`. |
| `GLIDESH_SECRETS` | Path to the secrets file, overriding auto-discovery. |
| `GLIDESH_SECRET_IDENTITY` | SSH private key that unlocks an age-wrapped secrets file. Honoured by every subcommand; outranked by `--secret-identity`. |
| `GLIDESH_RUNAS_PASS` | Privilege-escalation password for `run-as` (else `--ask-pass`). |
| `VISUAL`, `EDITOR` | Editor for `glidesh secret edit` and for opening a log from the logs browser (`VISUAL` first). |

```bash
RUST_LOG=glidesh=debug glidesh run -i inventory.kdl -p plan.kdl
```
