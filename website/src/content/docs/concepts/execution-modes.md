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
  hosts change at once — use [`serial`](#rolling-deploys) for that.

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

## Rolling Deploys

`serial` runs a plan on a few hosts at a time. **A batch starts only after every host in the
previous batch has finished the whole plan**, so a broken change reaches only one batch before
you see it fail.

```kdl
plan "deploy" {
    serial 1 "25%"
    max-fail "10%"

    step "Deploy binary" {
        file "/opt/myapp/bin/server" { src "build/server" }
    }

    step "Restart service" {
        systemd "myapp" { state "restarted" }
    }

    step "Health check" {
        shell "curl -fsS http://localhost:8080/health" { retries 10; delay 3; }
    }
}
```

- **`serial`** gives batch sizes: a host count (`2`) or a percentage of the plan's hosts
  (`"25%"`). The values are used in order and the last one repeats, so `serial 1 "25%"` runs
  one host first — a canary — then a quarter of the fleet at a time. A percentage is rounded
  down, but a batch always has at least one host.
- **`max-fail`** stops the rollout once more hosts than this have failed: a count, or a
  percentage of all the plan's hosts. It is checked after each batch; the batch already running
  always finishes. `max-fail 0` stops at the first failure.
- **Without `max-fail`**, the rollout stops only if every host in a batch fails — almost always
  a broken change rather than a bad host. Set `max-fail` for anything stricter.

The health gate is an ordinary step. A host whose check fails — here after 10 tries — fails,
and counts towards `max-fail`.

When a rollout stops, the hosts it never reached are reported as **aborted**, and the run exits
with an error, as it does for failures. On eight hosts the plan above runs batches of 1, 2, 2,
2 and 1; if one host of the second batch fails its health check, one failure is already more
than 10% of eight:

```
--- Batch 1/5: web-1 ---
...
--- Batch 2/5: web-2, web-3 ---
...
--- Rollout stopped: 1 of 8 hosts failed, more than max-fail 10% ---
[web-4] ABORTED (not started)
[web-5] ABORTED (not started)
[web-6] ABORTED (not started)
[web-7] ABORTED (not started)
[web-8] ABORTED (not started)

--- Run Complete ---
Hosts: 8 total, 2 ok, 1 failed, 5 aborted, 8 changed
```

Within a batch, the mode still applies: in sync mode the batch's hosts move through the steps
together; in async mode each runs at its own pace. A [`host`](/modules/host/) task runs once
for the whole rollout, in the first batch that reaches it; later batches reuse its result.

Try a plan on a single host before rolling it out, without editing it, by overriding both
settings on the command line:

```bash
glidesh run -i inventory.kdl -p deploy.kdl --serial 1 --max-fail 0
```

`serial` and `max-fail` belong to the plan being run. In an [included](/advanced/plan-includes/)
plan they are ignored, as `mode` is.

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
