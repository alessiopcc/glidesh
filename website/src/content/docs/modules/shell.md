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

`retries` is for a command that can fail transiently. To wait for something to become ready
before a step, use the step's [`until=`](/advanced/until/) instead: a polling `shell` task
counts as a change on every run, and triggers every step that subscribes to it.

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | The command to execute (alternative to `cmd`) |
| `cmd` | string or list | Single command string, or list of commands joined with `&&` (alternative to positional) |
| `check` | string | Gate command — if it exits 0 the task is already satisfied and the command does not run |
| `retries` | integer | Number of retry attempts on failure |
| `delay` | integer | Seconds between retries |
| `timeout` | integer | Abort the command after this many seconds and treat the attempt as failed (feeds `retries`). Default: no limit |
| `success_codes` | string / integer / list | Exit codes treated as success (e.g. `"0,2"`). Default: only `0` |
| `login` | boolean | Run the command (and `check` gate) inside a POSIX login shell so `/etc/profile` and `~/.profile` are sourced |
| `changed-when` | boolean / string | Whether a successful run counts as a change: `#false` never, `#true` always (the default), or a command run afterwards whose exit 0 means it did — see [Reporting changes](#reporting-changes-with-changed-when) |

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

## Idempotency with `check`

By default the shell module always runs. The optional `check` parameter runs a gate command first to decide whether the command is needed:

- **Exit 0** — the task is already **satisfied**: the command does not run and the task reports `ok`
- **Non-zero exit** — the task is **pending** and the command **runs**

```kdl
step "Install package" {
    shell "apt-get install -y nginx" check="dpkg -l nginx | grep -q ^ii"
}
```

`check` asks the host whether the work is already done. To decide whether a task applies to a host at all — by OS, inventory variable, or feature flag, without running anything — use [`when=`](/advanced/conditionals/); see [`when` or `check`?](/advanced/conditionals/#when-or-check).

In a step [triggered by `subscribe`](/advanced/subscribe/#what-a-triggered-task-does), the gate is skipped and the command runs: being triggered means the work must be done again. `check="true"` therefore makes a command that runs only when triggered.

## Reporting changes with `changed-when`

A shell command that runs counts as a change — glidesh cannot tell what it did. `changed-when` says otherwise.

**Read-only commands** that only gather information should never count. Otherwise a plan that runs `lsblk` or a status query reports a change on every run, and a step that [subscribes](/advanced/subscribe/) to it fires every time:

```kdl
step "List disks" {
    shell "lsblk -dn -o NAME" register="disks" changed-when=#false
}
```

**Commands whose effect varies** can be followed by a probe that decides: exit `0` means the run changed something, any other exit means it did not.

```kdl
step "Upgrade packages" {
    shell "apt-get upgrade -y" changed-when="test -f /var/run/reboot-required"
}
```

- The probe runs only after the command succeeds, with the same `login` and `timeout` settings. A probe that times out counts as a change, since it could not show that nothing changed.
- A task that does not count as a change reports `ok` and does not trigger subscribers.
- Under `--dry-run`, `changed-when=#false` is honoured — the task is not counted. A probe cannot run in a preview, so a task with one is reported as `would change` whenever its `check` says it would run.

`check` and `changed-when` combine: `check` decides whether the command runs at all, `changed-when` whether running it changed anything.

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

## Output limit

glidesh keeps at most 8 MiB of each output stream (stdout and stderr) of a command it runs
on a host: its first 4 MiB and its last 4 MiB, joined by a line that says how much was
dropped:

```text
[glidesh: 12582912 bytes of output dropped here]
```

The command itself runs to the end and its exit code is unaffected. The limit applies to
every command glidesh runs on a host — tasks, `check` guards, [`until=`](/advanced/until/)
gates, container probes, a [`host`](/modules/host/) task with `on=` — so a command that
prints without end cannot exhaust the controller's memory. A `host` task without `on=` runs
on the controller itself and keeps all of its output. Output shown in the run and kept in
the [run log](/concepts/logs/) is cut much shorter still.

A task whose output was cut cannot be [registered](/advanced/loops-register/#register): the
task fails instead. Send large output to a file on the host and register something smaller.

## Idempotency

Without a `check` parameter, the shell module always reports `Pending` — it has no way to know if the command needs to run. Use `check` to make shell steps idempotent, or use the module for commands that are safe to repeat.

Every module's rules side by side: [Idempotency & Drift](/concepts/idempotency/).

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
