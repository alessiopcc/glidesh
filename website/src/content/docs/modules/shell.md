---
title: shell
description: Run arbitrary shell commands on target hosts.
---

The `shell` module executes commands on the remote host via SSH.

## Usage

```kdl
shell "echo 'hello world'"

shell "curl -sf http://localhost:8080/health" {
    retries 5           // retry until curl exits 0
    delay 3
}
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | The command to execute (alternative to `cmd`) |
| `cmd` | string or list | Single command string, or list of commands joined with `&&` (alternative to positional) |
| `check` | string | Gate command — if it exits 0 the step is skipped (already satisfied) |
| `retries` | integer | Number of retry attempts on failure |
| `delay` | integer | Seconds between retries |
| `timeout` | integer | Abort the command after this many seconds and treat the attempt as failed (feeds `retries`). Default: no limit |
| `success_codes` | string / integer / list | Exit codes treated as success (e.g. `"0,2"`). Default: only `0` |
| `login` | boolean | Run the command (and `check` gate) inside a POSIX login shell so `/etc/profile` and `~/.profile` are sourced |

## Exit codes (`success_codes`)

By **default only exit code `0` counts as success** — any non-zero exit fails the attempt (and triggers `retries` if configured). Set `success_codes` to widen the accepted set when a tool uses non-zero codes to mean something other than failure.

```kdl
// cloud-init returns 2 when it finished but with recoverable errors ("degraded
// done"). Accept both 0 and 2; only a real error (exit 1) fails and retries.
shell "incus exec -T web -- cloud-init status --wait" {
    success_codes "0,2"
    retries 30
    delay 5
}
```

`success_codes` accepts a comma/space-separated string (`"0,2"`), a single integer (`2`), or a list.

> Because the default already accepts only `0`, a plain `retries`/`delay` loop retries until the command exits `0` with no extra configuration. Use `success_codes` only when a non-zero exit should *also* count as success.

## Timeouts (`timeout`)

A remote command with no `timeout` runs until it completes. Some tools hang when driven non-interactively (no TTY) — set `timeout` (seconds) to bound each attempt. On timeout the in-flight command is abandoned (its SSH channel is closed; the remote process may keep running) and the attempt is treated as a failure, so `retries`/`delay` apply.

```kdl
shell "incus exec -T web -- cloud-init status --wait" {
    timeout 60      // give up on a stuck attempt after 60s
    retries 30
    delay 5
    success_codes "0,2"
}
```

## Conditional execution with `check`

By default the shell module always runs. The optional `check` parameter runs a gate command first to decide whether the step is needed:

- **Exit 0** — the step is already **satisfied** and is **skipped**
- **Non-zero exit** — the step is **pending** and will **run**

```kdl
step "Install package" {
    shell "apt-get install -y nginx" check="dpkg -l nginx | grep -q ^ii"
}
```

## Using `cmd` instead of positional

The `cmd` parameter can be used as an alternative to the positional command string. It accepts either a single string or a list of commands (joined with ` && `).

### Single command

When combined with `check`, this provides a clean block syntax with no positional argument needed:

```kdl
step "Start valkey" {
    shell {
        check "docker ps --filter name=prophet-valkey --filter status=running -q | grep -q ."
        cmd "docker run -d --name prophet-valkey --network prophet --restart always -p 6379:6379 -v prophet_valkey:/data valkey/valkey:8-alpine"
    }
}
```

### Command list (multiline)

For long command sequences, use a list. The commands are joined with ` && `:

```kdl
step "Add deadsnakes PPA" {
    shell {
        check "test -f /etc/apt/sources.list.d/deadsnakes-*"
        cmd {
            - "apt-get update -qq"
            - "apt-get install -y software-properties-common"
            - "add-apt-repository -y ppa:deadsnakes/ppa"
            - "apt-get update -qq"
        }
    }
}
```

This is equivalent to:

```kdl
shell "apt-get update -qq && apt-get install -y software-properties-common && add-apt-repository -y ppa:deadsnakes/ppa && apt-get update -qq"
```

## Login shell environment (`login=#true`)

SSH non-interactive sessions start with a minimal environment. Profile scripts in `/etc/profile`, `/etc/profile.d/*.sh`, and `~/.profile` — which is where **Nix**, **asdf**, **nvm**, **rustup**, and similar tools inject their `PATH` entries — are **not** sourced by default. That means a command like `shell "rg foo"` will often fail with `command not found` even though the tool is installed.

Set `login=#true` to wrap the command (and the `check` gate) in `sh -l -c '…'`, which forces the remote to read those profile scripts:

```kdl
// Nix-installed tool
shell "rg TODO ./src" login=#true

// With a check gate
shell {
    cmd "mytool --refresh"
    check "command -v mytool"
    login #true
}
```

Use this whenever the tool lives in a user profile or uses shims (`~/.nix-profile/bin`, `~/.asdf/shims`, `~/.nvm/versions/...`). You do **not** need it for tools in system paths like `/usr/bin` or `/usr/local/bin`.

See also the [nix module](/modules/nix/) for higher-level package/shell/build operations that set up their own Nix environment.

## Idempotency

Without a `check` parameter, the shell module always reports `Pending` — it has no way to know if the command needs to run. Use `check` to make shell steps idempotent, or use the module for commands that are safe to repeat.

## Examples

### Simple command

```kdl
step "Check connectivity" {
    shell "ping -c 1 google.com"
}
```

### Health check with retries

```kdl
step "Wait for app" {
    shell "curl -sf http://localhost:8080/health" {
        retries 10          // keep retrying until the health check exits 0
        delay 5
    }
}
```

### Capture output with register

```kdl
step "Get hostname" {
    shell "hostname" register="node_hostname"
}

step "Log it" {
    shell "echo 'Running on ${node_hostname}'"
}
```

### Skip if already done

```kdl
step "Initialize database" {
    shell "pg_isready && createdb myapp" check="psql -lqt | grep -q myapp"
}
```

See [Loops & Register](/advanced/loops-register/) for more on capturing command output.
