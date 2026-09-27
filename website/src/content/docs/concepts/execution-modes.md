---
title: Execution Modes
description: Understand sync vs async execution and concurrency control.
---

glidesh supports two execution modes that control how steps are coordinated across hosts.
Within a single host, steps always run one after another in either mode.

## Sync Mode (default)

Hosts move through the plan together: **no host starts step N+1 until every host still
running has finished step N**. A host that skips a step with [`when=`](/advanced/conditionals/)
has finished it too.

```kdl
plan "deploy" {
    mode "sync"

    step "Run migrations" {
        shell "/opt/myapp/bin/migrate" when="${@host.name} == app-1"
    }

    step "Deploy binary" {
        file "/opt/myapp/bin/server" { src "build/server" }
    }

    step "Restart service" {
        systemd "myapp" { state "restarted" }
    }
}
```

Here no host receives the new binary until the migrations have finished on `app-1`.

Use sync mode when cross-host ordering matters. It has costs:

- **The slowest host sets the pace.** Every host waits at each step for the last one to
  finish it. A command that hangs holds the whole fleet — give long commands a
  [`timeout`](/modules/shell/#timeouts-timeout).
- **Every host does the same step at the same time.** In the example above, every host
  restarts the service together. Sync mode is not a rolling deploy: it does not limit how many
  hosts change at once.

If a host fails, the others carry on without it: a failed host stops, and the rest are no
longer held for it.

## Async Mode

Each host runs the entire plan independently at its own pace. Hosts never wait for each
other.

```kdl
plan "update-packages" {
    mode "async"

    step "Update system" {
        shell "apt-get update && apt-get upgrade -y"
    }
}
```

Use async mode when steps are independent across hosts — typically faster for large fleets,
since a slow host holds back only itself.

## Setting the Mode

In the plan file:

```kdl
plan "example" {
    mode "async"
    // ...
}
```

Or on the command line, which overrides the plan in either direction:

```bash
glidesh run -i inventory.kdl -p plan.kdl -m async
glidesh run -i inventory.kdl -p plan.kdl -m sync
```

Without `-m`, the plan's `mode` applies, and a plan without one runs in sync mode.

Groups that run their own [inventory-linked plans](/concepts/inventory/#inline-plans) are
independent of each other: sync mode holds together the hosts running the same plan.

## Concurrency

`--concurrency` (default 10) limits how many hosts work at the same time:

```bash
glidesh run -i inventory.kdl -p plan.kdl --concurrency 50
```

- **In async mode** it limits how many hosts run the plan at once. A host takes a slot when it
  starts and keeps it until it has finished.
- **In sync mode** it limits how many hosts connect, or run a step, at once. A host gives its
  slot back while it waits for the others, so a fleet larger than `--concurrency` still
  moves through each step together — it just takes each step in several waves. Every host
  keeps its SSH connection open for the whole run.
