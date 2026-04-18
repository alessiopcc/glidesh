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
┌─ glidesh console — inventory.kdl  [4 hosts, 2 tunnels] ────────────────────┐
│ ▾ web    (3)                          ┌─ Plan ───────────────────────┐     │
│    [✓] web-1   deploy@10.0.1.10:22    │ Target: web                  │     │
│    [ ] web-2   deploy@10.0.1.11:22    │ Plan:   plans/deploy.kdl     │     │
│    [ ] web-3   deploy@10.0.1.12:22    │                              │     │
│ ▾ db     (1)                          │ Press r to run               │     │
│    [ ] db-1    postgres@10.0.2.20:22  └──────────────────────────────┘     │
├────────────────────────────────────────────────────────────────────────────┤
│ Dir Listen                  Via    Forwards to     Accepts Saved Status    │
│ L   127.0.0.1:8080          web-1  localhost:80    14      ✓     active    │
│ R   127.0.0.1:5433 (remote) db-1   127.0.0.1:5432  3             active    │
└────────────────────────────────────────────────────────────────────────────┘
 ↑↓ nav  Space select  Enter/s shell  t tunnel  r run  Tab focus  d kill  q quit
```

The **Listen** column is the side that accepts incoming connections (your local box for `-L`, the remote sshd for `-R`). The **Forwards to** column is the destination the bytes get delivered to.

- **Top-left**: collapsible group → host tree, with multi-select markers
- **Top-right**: plan associated with the focused row (group plan, host plan, or none)
- **Bottom**: live table of active tunnels (direction, local port, via-host, remote target, accept count, saved flag, status)
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
| `r` | Run the plan shown in the right panel (`glidesh run` is invoked with the resolved target filter). The console suspends while the plan runs. |
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

Fields cycle in this order: `Local port`, `Remote host`, `Remote port`, `Bind addr R`, `Reverse (-R)`, `Save`. The `Bind addr R` field is pre-filled with `127.0.0.1` and only applies when `Reverse` is checked.

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

When the cursor is on a single host (or exactly one host is selected), pressing `Enter`/`s` opens a full PTY shell — same as `glidesh console -t <host>`. The console TUI is suspended while the shell runs; type `exit` (or Ctrl+D) to return.

### Group or multi-select

When a group is focused or multiple hosts are selected, the console launches the broadcast multi-host TUI (the same UI as `glidesh console -t <group>` without `-c`). You type a command once and it runs concurrently on every selected host with `[hostname]`-prefixed output.

### Tunnels keep running

Background tunnel tasks are independent of the foreground shell. Tunnels stay open and continue to accept connections while you are inside a shell session.

## Plans

The right-side **Plan** panel shows the plan associated with whatever the cursor is on:

| Cursor on | Plan source | Target filter passed to `glidesh run` |
|-----------|-------------|---------------------------------------|
| A group with `plan="..."` | the group's plan | the group name |
| A host inside a group | the host's own `plan="..."` if set, otherwise the group's plan | the host name (own plan) or `group:host` (group plan) |
| An ungrouped host with `plan="..."` | the host's plan | the host name |
| Anything without an associated plan | — | (panel shows "no plan associated") |

Press `r` to run it. The console suspends, `glidesh run -i <inv> -p <plan> -t <target>` is spawned with inherited stdio (so the run TUI takes over the terminal), and you return to the console after pressing a key. Tunnels stay open in the background. SSH key path and host-key flags are forwarded from the console invocation.

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

The remote sshd listens on `<bind-addr>:<remote-port>` and forwards incoming connections back through the SSH session to `<remote-host>:<local-port>` on your local machine (typically `127.0.0.1:<local-port>`).

1. Move the cursor to the host
2. Press `t`
3. Fill in `Local port` (where to forward back to locally), `Remote host` (usually `127.0.0.1`), `Remote port` (the port sshd binds remotely)
4. Set `Bind addr R` if you want a non-default remote bind address. The field is pre-filled with `127.0.0.1` (loopback only — safest); override with `0.0.0.0` to accept connections on every remote interface (and only if the server's `GatewayPorts` allows it).
5. Check `Reverse`
6. Press `Enter`

Equivalent to:

```
ssh -R <bind-addr>:<remote-port>:<remote-host>:<local-port> user@<via-host>
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
tunnel via="db-1" direction="R" local-port=5432 remote-host="127.0.0.1" remote-port=5433 bind-addr="127.0.0.1"
```

`bind-addr` is only emitted for `-R` entries (default `127.0.0.1` when missing — `-L` ignores the field).

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

For automation, use `glidesh run` or `glidesh console -t <name> -c "..."` instead — the latter runs without a TTY when `--command` is set.
