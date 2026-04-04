---
title: CLI Reference
description: Complete command reference for glidesh.
---

## `glidesh run`

Execute a plan against target hosts.

```
glidesh run [OPTIONS]
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--plan <PATH>` | `-p` | Path to the plan file | — |
| `--inventory <PATH>` | `-i` | Path to the inventory file | — |
| `--target <NAME>` | `-t` | Target filter: group name, host name, or group:hostname | — |
| `--host <ADDR>` | — | Single host for ad-hoc mode | — |
| `--user <USER>` | `-u` | SSH user | — |
| `--port <PORT>` | `-P` | SSH port | `22` |
| `--key <PATH>` | `-k` | SSH private key path | — |
| `--command <CMD>` | `-c` | Ad-hoc command to run | — |
| `--mode <MODE>` | `-m` | Execution mode: `sync` or `async` | `sync` |
| `--concurrency <N>` | — | Max concurrent hosts | `10` |
| `--dry-run` | — | Check only, no changes applied | `false` |
| `--no-tui` | — | Disable TUI, use plain text output | `false` |
| `--no-host-key-check` | — | Skip SSH host key verification | `false` |
| `--accept-new-host-key` | — | Accept and save unknown host keys to known_hosts | `false` |

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

## `glidesh shell`

Open an interactive shell or run commands directly on inventory hosts — no plan file needed.

```
glidesh shell [OPTIONS] --inventory <PATH>
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--inventory <PATH>` | `-i` | Path to the inventory file | *(required)* |
| `--target <NAME>` | `-t` | Target filter: group name, host name, or group:hostname | — |
| `--command <CMD>` | `-c` | Command to run (skip interactive mode) | — |
| `--key <PATH>` | `-k` | SSH private key path | — |
| `--concurrency <N>` | — | Max concurrent hosts | `10` |
| `--no-host-key-check` | — | Skip SSH host key verification | `false` |
| `--accept-new-host-key` | — | Accept and save unknown host keys to known_hosts | `false` |

### Interactive shell (single host)

Target a single host to open a full PTY shell session:

```bash
glidesh shell -i inventory.kdl -t web-1
```

This works exactly like SSH — you get a remote terminal and can run commands interactively. Type `exit` to disconnect.

### Run a command across hosts

Use `-c` to run a command on one or more hosts. Output is streamed with `[hostname]` prefixes:

```bash
glidesh shell -i inventory.kdl -t web -c "df -h /"
```

```
[web-1] /dev/sda1  50G  40G  10G  80% /
[web-2] /dev/sda1  50G  25G  25G  50% /
[web-3] /dev/sda1  50G  45G   5G  90% /
```

Run on every host in the inventory:

```bash
glidesh shell -i inventory.kdl -c "uptime"
```

### Interactive group shell (TUI)

When targeting multiple hosts without `-c`, glidesh opens a TUI with a command input bar:

```bash
glidesh shell -i inventory.kdl -t web
```

Type a command and press Enter — it runs on all targeted hosts concurrently and streams `[hostname]`-prefixed results in real time. Press Ctrl+D to exit.

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
