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
