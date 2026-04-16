---
title: shell
description: Run arbitrary shell commands on target hosts.
---

The `shell` module executes commands on the remote host via SSH.

## Usage

```kdl
shell "echo 'hello world'"

shell "curl -sf http://localhost:8080/health" {
    retries 5
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
        retries 10
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
