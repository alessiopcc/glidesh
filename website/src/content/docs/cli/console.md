---
title: Console
description: Interactive TUI for browsing inventory hosts, opening shells, and managing SSH tunnels.
---

`glidesh console` opens an interactive terminal UI that acts as the connection center for an inventory: browse groups and hosts, open interactive or broadcast shells, and create / manage SSH local (-L) and reverse (-R) port forwards. Tunnel specs can be saved to disk so they auto-reopen on the next launch.

Run it explicitly:

```bash
glidesh console -i inventory.kdl
```

Or just type `glidesh` with no subcommand — when no subcommand is given, the console opens against `./inventory.kdl` if it exists in the current directory:

```bash
cd my-fleet/
glidesh
```

## Layout

```
┌─ glidesh console — inventory.kdl  [4 hosts, 2 tunnels] ──────────┐
│ ▾ web    (3)                                                     │
│    [✓] web-1   deploy@10.0.1.10:22                               │
│    [ ] web-2   deploy@10.0.1.11:22                               │
│    [ ] web-3   deploy@10.0.1.12:22                               │
│ ▾ db     (1)                                                     │
│    [ ] db-1    postgres@10.0.2.20:22                             │
├──────────────────────────────────────────────────────────────────┤
│ Dir Local           Via      Remote          Accepts Saved Status│
│ L   127.0.0.1:8080  web-1    localhost:80    14      ✓     active│
│ R   127.0.0.1:5432  db-1     127.0.0.1:5432  3             active│
└──────────────────────────────────────────────────────────────────┘
 ↑↓ nav  Space select  Enter/s shell  t tunnel  Tab focus  d kill  q quit
```

- **Top half**: collapsible group → host tree, with multi-select markers
- **Bottom half**: live table of active tunnels (direction, local port, via-host, remote target, accept count, saved flag, status)
- **Footer**: keybindings + transient flash messages (errors, status updates)

## Keybindings

### Tree (top panel — default focus)

| Key | Action |
|-----|--------|
| `↑` / `↓` or `j` / `k` | Move cursor |
| `←` / `→` | Collapse / expand the focused group |
| `Space` | Toggle selection of host (or every host in a group) |
| `Esc` | Clear all selections |
| `Enter` or `s` | Open shell — single host = interactive PTY, group / multi-select = broadcast TUI |
| `t` | Open the tunnel-creation dialog (cursor must be on a host) |
| `Tab` | Switch focus to the tunnel table |
| `q` or `Ctrl+C` | Quit (confirms if any tunnels are active) |

### Tunnel table (bottom panel)

| Key | Action |
|-----|--------|
| `↑` / `↓` or `j` / `k` | Move tunnel cursor |
| `d` / `x` / `Delete` / `Backspace` | Kill the focused tunnel |
| `Tab` | Switch focus back to the host tree |

When killing a **saved** tunnel, the console asks whether to also delete the saved spec:

- `y` — kill and delete the saved spec
- `n` — kill but keep the saved spec (it will reopen next launch)
- `Esc` — cancel

### Tunnel dialog

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Cycle between fields |
| Type | Edit the focused text field (digits only for ports) |
| `Backspace` | Delete a character |
| `Space` | Toggle the focused checkbox (`Reverse` or `Save`) |
| `Enter` | Submit |
| `Esc` | Cancel |

## Shells

### Single host

When the cursor is on a single host (or exactly one host is selected), pressing `Enter`/`s` opens a full PTY shell — same as `glidesh shell -t <host>`. The console TUI is suspended while the shell runs; type `exit` (or Ctrl+D) to return.

### Group or multi-select

When a group is focused or multiple hosts are selected, the console launches the broadcast multi-host TUI (the same UI as `glidesh shell -t <group>` without `-c`). You type a command once and it runs concurrently on every selected host with `[hostname]`-prefixed output.

### Tunnels keep running

Background tunnel tasks are independent of the foreground shell. Tunnels stay open and continue to accept connections while you are inside a shell session.

## Tunnels

### Opening a local forward (`-L`)

Standard local port forwarding: incoming TCP connections to `127.0.0.1:<local-port>` on your machine are routed through the SSH session to `<remote-host>:<remote-port>` on the remote network.

1. Move the cursor to the host you want to tunnel through
2. Press `t` to open the dialog
3. Fill in `Local port`, `Remote host`, `Remote port`
4. Leave `Reverse` unchecked
5. Press `Enter`

Equivalent to:

```
ssh -L <local>:<remote-host>:<remote-port> user@<via-host>
```

### Opening a reverse forward (`-R`)

The remote sshd listens on `0.0.0.0:<remote-port>` and forwards incoming connections back through the SSH session to `<remote-host>:<local-port>` on your local machine (typically `127.0.0.1:<local-port>`).

1. Move the cursor to the host
2. Press `t`
3. Fill in `Local port` (where to forward back to locally), `Remote host` (usually `127.0.0.1`), `Remote port` (the port sshd binds remotely)
4. Check `Reverse`
5. Press `Enter`

Equivalent to:

```
ssh -R <remote-port>:<remote-host>:<local-port> user@<via-host>
```

> **Note**: reverse forwards require the remote sshd to allow them — check `GatewayPorts` and `AllowTcpForwarding` in the server's `sshd_config`.

### Tunnels and multi-select

Tunnels are 1:1 with a via-host. The `t` key is disabled when more than one host is selected — clear the selection with `Esc` first, then move the cursor to your chosen host.

## Saving tunnels

Check the `Save` box in the dialog to persist the spec. Saved tunnels live in a sidecar file next to the inventory:

```
<inventory-dir>/.glidesh-tunnels.kdl
```

Format:

```kdl
tunnel via="web-1" direction="L" local-port=8080 remote-host="localhost" remote-port=80
tunnel via="db-1" direction="R" local-port=5432 remote-host="127.0.0.1" remote-port=5433
```

On every console launch, glidesh reads this file and re-opens each saved tunnel against the inventory. Any spec whose `via` host is missing or whose connection fails appears in the table with an `Error` status — the rest still come up. Add the file to `.gitignore` if you want the specs to stay personal.

## Connection reuse (session pool)

The console keeps one live SSH session per host and shares it across tunnels and shells. Opening multiple tunnels through the same host does not open additional SSH connections, and entering a shell on a host you already have a tunnel through reuses the same session.

## Jump hosts

The console respects jump host configuration in the inventory. If a host is behind a bastion, both shells and tunnels through that host transparently use the bastion — no extra setup needed. See [Jump Hosts](/advanced/jump-hosts/) for inventory syntax.

## Flags

```
glidesh console [OPTIONS]
```

| Flag | Short | Description | Default |
|------|-------|-------------|---------|
| `--inventory <PATH>` | `-i` | Path to the inventory file | `./inventory.kdl` |
| `--key <PATH>` | `-k` | SSH private key path | `~/.ssh/id_ed25519` |
| `--no-host-key-check` | — | Skip SSH host key verification | `false` |
| `--accept-new-host-key` | — | Accept and save unknown host keys to `known_hosts` | `false` |

SSH key resolution follows the same order as the other commands — see [SSH Key Resolution](/cli/#ssh-key-resolution).

## Non-TTY behavior

The console requires a TTY. If stdout is piped or redirected, `glidesh console` (or bare `glidesh`) exits with an error:

```
`glidesh console` requires a TTY. Use `glidesh run` for scripted execution.
```

For automation, use `glidesh run` or `glidesh shell -c "..."` instead.
