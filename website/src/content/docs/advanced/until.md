---
title: Waiting (until)
description: Hold a step until a command on the host succeeds, without reporting a change.
---

`until=` holds a step until the world is ready for it: a service that has registered, a
cluster that has reached quorum, a file another system writes. It runs a command on the host
before the step's tasks, again and again, until the command exits 0.

```kdl
plan "bench" {
    step "Wait for the cache to register" until="curl -sf localhost:8000/metrics | grep -q kv_pool_bytes" until-timeout=600 until-interval=5

    step "Run the benchmark" {
        shell "vllm bench serve --model ${model}"
    }
}
```

A step with `until=` and no tasks, like the first one above, is a pure wait. A step with both
waits first, then runs its tasks:

```kdl
step "Warm the cache" until="curl -sf http://localhost:8000/health" until-timeout=900 {
    shell "curl -s http://localhost:8000/v1/completions -d @/opt/warmup.json"
}
```

| Attribute | Type | Default | Meaning |
|-----------|------|---------|---------|
| `until` | string | — | Command run on the host; the step goes on once it exits 0 |
| `until-timeout` | integer | `300` | Seconds to keep trying before the step fails |
| `until-interval` | integer | `3` | Seconds between attempts |

Both are whole seconds from 1 to 604800 (7 days); anything else is rejected when the plan is
parsed, so `glidesh validate` catches it.

## Why not `shell` with `retries`?

A polling `shell` task works, but it runs its command as work: every run reports it as a
change, and every step that [subscribes](/advanced/subscribe/) to it is triggered every run.
Adding `check=` fixes the count but stops the waiting — `check` asks once. `until=` separates
the two: **waiting is not work**, so the gate never counts as a change and never triggers a
subscriber.

`until=` also differs from a container's [readiness gate](/modules/container/#readiness)
(`wait`, `ready-cmd`): that one belongs to a container task and asks whether *that container*
is ready. `until=` asks whether the host is ready for a whole step.

## Rules

- The gate runs on **every** run, before the step's tasks, once per step — a step with a
  [`loop`](/advanced/loops-register/#step-loops) waits once, then iterates. It cannot use
  `${@item}`.
- It runs after the step's [`when=`](/advanced/conditionals/) and [tags](/advanced/tags/): a
  skipped step does not wait.
- **Timing out fails the host**, like any failing step: the host stops and the error names the
  command, its last exit code and the tail of its last output. Nothing after it runs on that
  host, unless the step's [`rescue`](/advanced/rescue/) handles the failure. An attempt that is still running at the deadline is cut off and does not count, even
  if it would have succeeded — so a command that hangs cannot hold the step past
  `until-timeout`. glidesh stops trying once another attempt could not start before the
  deadline.
- While it waits, the run says so — `waiting until: <command> (up to 600s)` once the first
  attempt fails (or after 30 seconds of a slow one), then every 30 seconds
  `still waiting (90s of 600s)`, even while an attempt is running — in the plain output, the
  TUI and the [run log](/concepts/logs/).
- The command is interpolated like any parameter, so it can use `${@host.address}` and other
  variables, and runs with the step's [`run-as`](/advanced/run-as/).
- The gate is not a task: it has no `register`, and it does not count toward the summary.
- **`--dry-run`** runs the command once and never waits between attempts (that one attempt
  is still cut off at `until-timeout`). A gate that is not open yet is
  reported — `until not met yet: … (a run would wait up to 600s)` — and the preview goes on to
  the step's tasks.

## Across hosts

Each host runs its own gate. In [sync mode](/concepts/execution-modes/#sync-mode-default), the
default, hosts also wait for each other at every step, so no host starts the step after a gate
until every host's gate has opened and its tasks have run. In async mode, each host goes on as
soon as its own gate opens.
