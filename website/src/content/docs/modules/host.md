---
title: host
description: Run a command once on the controller (or a chosen inventory host) and broadcast the result to every target.
---

The `host` module runs a command **once per task** and shares the captured
output with every target host. It's the right primitive whenever every host
needs to agree on the same value — a generated token, a timestamp, a build
tag, the output of a leader host.

By default the command runs on the **controller** (the machine running
`glidesh`). With `on="<name>"` it runs on that single inventory host's SSH
session instead. Either way, the result is broadcast: every host's
`${register}` var receives the same string.

## Usage

```kdl
step "Generate one deploy token for the whole fleet" {
    host "deploy token" cmd="openssl rand -hex 16" register="deploy_token"
}

step "Write the same token everywhere" {
    shell "echo ${deploy_token} > /etc/app/token"
}
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Descriptive label for logs |
| `cmd` | string or list | Single command, or list joined with `&&` |
| `on` | string | Inventory host name to run on; default runs on the controller |
| `login` | boolean | Wrap command in `sh -l -c '…'` on the remote target (requires `on=`; rejected for controller-local execution) |

## Semantics

- **Runs exactly once.** The first target host to reach the task triggers
  execution; every other host blocks briefly and then reads the cached
  result. This is true even if the plan `mode` is `async`.
- **Register is broadcast.** If `register="var"` is set, every host's local
  `vars` map receives the same trimmed stdout.
- **Failure fails every host.** If the single execution returns a non-zero
  exit code, every host fails the step with the same error message.
- **Local by default.** Without `on=`, the command runs via `sh -c` on the
  controller (or `cmd /C` on Windows). It has access to the controller's
  environment, not the target's.

## Controller vs. target

```kdl
// Runs on the machine running glidesh
host "local time" cmd="date -u +%s" register="start_epoch"

// Runs once on node-1 via SSH; every host still reads the same value
host "leader hostname" cmd="hostname" on="node-1" register="leader"
```

## Command list

Like `shell`, `cmd` can be a list — items are joined with ` && `:

```kdl
step "Produce a build tag once" {
    host "build tag" register="build_tag" {
        cmd {
            - "git -C /src rev-parse --short HEAD"
        }
    }
}
```

## When to prefer `host` over `shell`

Use `shell` when each host should compute its own value (its hostname, its
uptime, its package list). Use `host` when every host must share a single
value — otherwise a `shell "openssl rand -hex 16" register="token"` would
write a different token on every node.
