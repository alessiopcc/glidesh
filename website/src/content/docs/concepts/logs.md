---
title: Logs
description: Browse, manage, and inspect logs from past runs with the interactive logs explorer.
---

Every `glidesh run` stores per-node logs and a JSON summary in `~/.glidesh/runs/`. The `glidesh logs` command lets you browse them.

## Storage Layout

Each run creates a directory named `<timestamp>_<plan-name>`:

```
~/.glidesh/runs/
├── 2025-04-10T14-30-22_deploy-web/
│   ├── summary.json
│   ├── web-1.log
│   └── web-2.log
├── 2025-04-10T13-15-00_setup-db/
│   ├── summary.json
│   └── db-1.log
```

- **summary.json** — run metadata: plan name, run ID, timestamps, per-node status and change count
- **\<node\>.log** — timestamped log lines for each host

## Interactive Logs Explorer

Running `glidesh logs` in a terminal opens a three-level TUI:

```bash
glidesh logs
```

### Run List

The top-level view lists all runs (newest first) with a summary of node count, successes, and failures.

| Key | Action |
|-----|--------|
| Up/Down | Navigate runs |
| Enter | View run details |
| Space | Toggle selection (multi-select) |
| d / Delete | Delete selected runs (with confirmation) |
| q | Quit |

The title bar shows the selection count when runs are selected. Pressing `d` without any selection deletes the highlighted run.

### Run Detail

Shows the run header (plan, run ID, timestamps) and a table of nodes with status, changed count, and error messages.

| Key | Action |
|-----|--------|
| Up/Down | Navigate nodes |
| Enter | View node log |
| Esc | Back to run list |
| q | Quit |

### Node Log

Displays the full log file with syntax highlighting:

- **Red bold** — failed steps
- **Cyan bold** — step headers
- **Yellow** — changed resources
- **Green** — successful results
- **Gray** — check operations

| Key | Action |
|-----|--------|
| Up/Down or j/k | Scroll line by line |
| PgUp/PgDn | Scroll by page |
| Home/g | Jump to top |
| End/G | Jump to bottom |
| c | Copy log to clipboard |
| e | Open log in `$VISUAL` / `$EDITOR` |
| Esc | Back to run detail |
| q | Quit |

The `c` key copies the entire log content to the system clipboard. On Linux this requires `xclip` to be installed.

The `e` key opens the log file in your preferred editor (`$VISUAL`, then `$EDITOR`, falling back to `notepad` on Windows or `vi` on Unix). The TUI suspends while the editor is open and resumes when you close it.

## CLI Flags

For non-interactive access or scripting:

```bash
glidesh logs --last                        # show last run summary
glidesh logs --last --node web-1           # print a specific node's log
glidesh logs --run 2025-04-10T14-30-22_deploy-web  # show a specific run
```

| Flag | Description |
|------|-------------|
| `--last` | Show the most recent run |
| `--node <NAME>` | Filter output to a specific node |
| `--run <DIR>` | Select a specific run by directory name |

When stdout is not a TTY (piped or in CI), `glidesh logs` prints the 20 most recent runs as plain text instead of launching the TUI.
