---
title: Execution Modes
description: Understand sync vs async execution and concurrency control.
---

glidesh supports two execution modes that control how steps are coordinated across hosts.

## Sync Mode (default)

All hosts execute the same step together. A barrier between steps ensures every host finishes step N before any host starts step N+1.

```kdl
plan "rolling-deploy" {
    target "web"
    mode "sync"

    step "Stop service" {
        systemd "myapp" { state "stopped" }
    }

    step "Deploy binary" {
        file "/opt/myapp/bin" { src "build/myapp" }
    }

    step "Start service" {
        systemd "myapp" { state "started" }
    }
}
```

Use sync mode when cross-host ordering matters — for example, database migrations before application deploys.

## Async Mode

Each host runs the entire plan independently at its own pace. There are no barriers between steps across hosts.

```kdl
plan "update-packages" {
    target "all"
    mode "async"

    step "Update system" {
        shell "apt-get update && apt-get upgrade -y"
    }
}
```

Use async mode when steps are independent across hosts — typically faster for large fleets.

## Setting the Mode

In the plan file:

```kdl
plan "example" {
    mode "async"
    // ...
}
```

Or override on the CLI:

```bash
glidesh run -i inventory.kdl -p plan.kdl -m async
```

The CLI flag takes precedence over the plan file.

## Concurrency

By default, glidesh runs up to 10 hosts concurrently. Adjust with `--concurrency`:

```bash
glidesh run -i inventory.kdl -p plan.kdl --concurrency 50
```

Each host gets its own async task, bounded by a semaphore. Within a single host, steps always run sequentially.
