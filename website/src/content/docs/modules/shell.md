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
| *(positional)* | string | The command to execute |
| `retries` | integer | Number of retry attempts on failure |
| `delay` | integer | Seconds between retries |

## Idempotency

The shell module always reports `Pending` — it has no way to know if the command needs to run. Use it for tasks where idempotency is handled by the command itself, or where the command is safe to repeat.

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

See [Loops & Register](/advanced/loops-register/) for more on capturing command output.
