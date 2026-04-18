---
title: TUI
description: Terminal user interfaces for plan execution, post-run debugging, the connection console, and interactive group shells.
---

glidesh ships several terminal UIs, each suited to a different workflow:

- **[Console](/cli/console/)** — opened by `glidesh` with no subcommand (or `glidesh console`) when no `--target`/`--command` is given. Browse groups/hosts, open shells, manage SSH local and reverse port forwards. Saved tunnels auto-reopen across sessions.
- **Plan Execution TUI** — opens during `glidesh run` to show live progress, per-node logs, and a post-run shell.
- **Interactive Group Shell** — opens via `glidesh console -t <group>` (no `-c`) to broadcast commands across multiple hosts.

## Plan Execution TUI

When you run a plan, the TUI shows a split-screen view:

- **Top**: progress bar with host count, changes, elapsed time
- **Middle**: node table with host status, current step, and timing
- **Bottom**: scrollable log panel (combined or per-node)

```bash
glidesh run -i inventory.kdl -p deploy.kdl
```

### Keybindings

| Key | Action |
|-----|--------|
| Up/Down or j/k | Select node / scroll logs (depending on focus) |
| Enter | View selected node's logs |
| Esc | Back to combined view |
| Tab | Switch focus between nodes and logs |
| PgUp/PgDn | Scroll logs by page |
| g / G | Jump to top / bottom of logs |
| q | Quit (confirms if still running) |

When viewing a single node's logs (after pressing Enter), Tab switches focus between the node list and the log panel so you can scroll through the log. Press Esc to return to the combined view.

## Post-Run Shell Access

After a plan completes — whether it succeeded or failed — you can press **`s`** on any host in the node table to open an interactive shell directly to that host.

This is the fastest way to debug a failed step: select the failed host, press `s`, and you are in a remote terminal on that exact machine with no extra commands or windows needed.

```
Plan completes -> select failed host -> press s -> investigate -> exit -> back to TUI
```

The TUI suspends while the shell is active. When you type `exit` (or Ctrl+D), you return to the TUI exactly where you left off.

The footer keybinding hints update after the run completes to show the `s shell` option.

### Jump hosts

Shell access respects jump host configuration. If a host was reached through a bastion during the plan, pressing `s` tunnels through the same bastion automatically.

## Interactive Group Shell (TUI)

The `glidesh console` command opens a dedicated broadcast TUI when targeting multiple hosts without `-c`:

```bash
glidesh console -i inventory.kdl -t web
```

This TUI has two panels:

- **Output panel**: scrollable area showing `[hostname]`-prefixed results
- **Input bar**: type a command and press Enter to run it on all hosts

Commands run concurrently on all targeted hosts (bounded by `--concurrency`), and output streams in real time. You can scroll through results while a command is running.

| Key | Action |
|-----|--------|
| Enter | Run command on all hosts |
| Up/Down | Scroll output |
| PgUp/PgDn | Scroll output by page |
| Ctrl+C / Ctrl+D | Exit |

This is useful for ad-hoc investigation across a fleet — checking disk usage, tailing logs, or verifying a deploy without writing a plan.

## Disabling the TUI

### `--no-tui` flag

For CI pipelines, cron jobs, or when piping output, use `--no-tui` to get plain text:

```bash
glidesh run -i inventory.kdl -p deploy.kdl --no-tui
```

Output is printed line by line with `[group:host]` prefixes:

```
[web:web-1] Connecting...
[web:web-1] Connected (ubuntu-22.04)
[web:web-1] Step 1/3: Deploy binary
[web:web-1]   file '/opt/app/bin': changed
[web:web-1] Step 2/3: Restart service
[web:web-1]   systemd 'myapp': changed
[web:web-1] OK (2 changed)

--- Run Complete ---
Hosts: 2 total, 2 ok, 0 failed, 4 changed
```

### Automatic detection

The TUI is disabled automatically when stdout is not a TTY (e.g., when piping to a file or running in a non-interactive shell). You do not need `--no-tui` in most CI environments.

### `glidesh console -c` in CI

For running commands across hosts in CI, use the `-c` flag — it always produces plain text output regardless of TTY:

```bash
glidesh console -i inventory.kdl -t web -c "systemctl status myapp" --no-host-key-check
```
