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
| `--target <NAME>` | `-t` | Target filter: group name, host name, or group:hostname | — |
| `--command <CMD>` | `-c` | Command to run (skips the TUI; runs on resolved targets) | — |
| `--key <PATH>` | `-k` | SSH private key path | `~/.ssh/id_ed25519` |
| `--concurrency <N>` | — | Max concurrent hosts when running a command (minimum 1) | `10` |
| `--no-host-key-check` | — | Skip SSH host key verification | `false` |
| `--accept-new-host-key` | — | Accept and save unknown host keys | `false` |

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

Execute a plan against target hosts.

```
glidesh run [OPTIONS]
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--plan <PATH>` | `-p` | Path to the plan file | — |
| `--inventory <PATH>` | `-i` | Path to the inventory file | — |
| `--target <NAME>` | `-t` | Target filter: group name, host name, `group:hostname`, or a comma-separated list of any of these | — |
| `--host <ADDR>` | — | Single host for ad-hoc mode | — |
| `--user <USER>` | `-u` | SSH user (ad-hoc mode only) | `root` |
| `--port <PORT>` | `-P` | SSH port | `22` |
| `--key <PATH>` | `-k` | SSH private key path | `~/.ssh/id_ed25519` |
| `--command <CMD>` | `-c` | Ad-hoc command to run | — |
| `--mode <MODE>` | `-m` | Execution mode: `sync` or `async` | `sync` |
| `--concurrency <N>` | — | Max concurrent hosts | `10` |
| `--dry-run` | — | Check only, no changes applied | `false` |
| `--no-tui` | — | Disable TUI, use plain text output | `false` |
| `--no-host-key-check` | — | Skip SSH host key verification | `false` |
| `--accept-new-host-key` | — | Accept and save unknown host keys to known_hosts | `false` |

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

View logs from past runs. Logs are stored in `~/.glidesh/runs/`.

```
glidesh logs [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `--last` | Show the last run |
| `--node <NAME>` | Filter by node name |
| `--run <DIR>` | Specific run directory |

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

## Environment Variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Control log verbosity. Default is `glidesh=info`. Set to `glidesh=debug` or `glidesh=trace` for troubleshooting. |

```bash
RUST_LOG=glidesh=debug glidesh run -i inventory.kdl -p plan.kdl
```
