---
title: Idempotency & Drift
description: How each module decides whether a host already matches the plan, and how to see what a run would change.
---

glidesh keeps no state between runs. Every task asks the host whether it already matches
the plan, and changes only what differs — so running a plan twice is safe, and the second
run reports nothing to change. A host edited by hand since the last run (drift) is found the
same way and put back.

Each task has two phases: **check** compares the host with the plan, and **apply** makes the
change when check found one. A task that was already in place reports `ok`; one that was
not reports `changed`. See [How Modules Work](/modules/#how-modules-work).

## What each module compares

| Module | Already in place when… | Details |
|--------|------------------------|---------|
| [`file`](/modules/file/) | the SHA256 checksum of the host's file matches the local (rendered) one, and `owner`/`group`/`mode` match. A change to attributes alone is fixed without re-uploading | [Idempotency](/modules/file/#idempotency) |
| [`container`](/modules/container/) | the container's `sh.glide.param-hash` label matches the hash of every parameter in the plan. Any difference recreates it; a stopped container that still matches is started | [Idempotency](/modules/container/#idempotency) |
| [`package`](/modules/package/) | the package is already installed (or already absent) | [Idempotency](/modules/package/#idempotency) |
| [`user`](/modules/user/) | the user exists with the requested uid, shell and groups | [Idempotency](/modules/user/#idempotency) |
| [`systemd`](/modules/systemd/) | the service is already active/enabled as requested, and a generated unit file's checksum matches | [Idempotency](/modules/systemd/#idempotency) |
| [`disk`](/modules/disk/) | the filesystem type, fstab entry and mount already match | [Idempotency](/modules/disk/#idempotency) |
| [`nix`](/modules/nix/) | the package or channel is already installed; `shell`, `build`, `flake-update` and `gc` always run | [Idempotency](/modules/nix/#idempotency) |
| [`shell`](#shell-commands) | never, unless you say how — see below | [Idempotency with `check`](/modules/shell/#idempotency-with-check) |
| [`host`](/modules/host/) | never: it runs once per task, on every run | [Semantics](/modules/host/#semantics) |
| [external](/modules/external/) | whatever the plugin's `check` answers | [Writing Plugins](/advanced/writing-plugins/) |

## Shell commands

glidesh cannot tell what an arbitrary command does, so a `shell` task runs on every run and
counts as a change. Three parameters make it behave like the other modules:

- **`check="…"`** — a command run first; exit 0 means the work is already done and the task
  reports `ok` without running. This is what other tools call `creates` or `unless`.
- **`changed-when`** — whether a successful run counts as a change: `#false` for read-only
  commands, or a probe run afterwards.
- **`retries`, `delay`, `timeout`, `success_codes`** — for commands that fail transiently or
  use non-zero exit codes for success.

```kdl
step "Install the CLI" {
    shell "curl -fsSL https://example.com/install.sh | sh" check="command -v example" retries=3 delay=5
}
```

To decide whether a task applies to a host at all — by OS or a variable, without running
anything — use [`when=`](/advanced/conditionals/#when-or-check) instead.

## Containers and drift

Every parameter that reaches the container runtime — image, ports, volumes, environment,
`extra-args`, everything — is folded into the `sh.glide.param-hash` label when glidesh
creates the container. A later run recomputes the hash from the plan and recreates the
container only when it differs, whether the plan changed or someone recreated the container
by hand. Readiness settings (`wait`, `ready-cmd`, …) are excluded, so tuning a probe never
recreates a healthy container.

A second label, `sh.glide.field-hashes`, records a short hash per parameter, so
[`--diff`](/modules/container/#--diff) can name *which* parameters drifted. Neither label
holds a value, so no secret is written to the container's metadata.

## Seeing what a run would change

- **[`--dry-run`](/cli/#previewing-a-run)** runs every check and no apply. Each task reports
  `would change` or `ok`, with the reason check gave:
  `Recreate container web (configuration changed)`.
- **`--diff`** adds the detail behind each change: a unified diff of a `file`'s content,
  the parameters of a `container` that changed. It works on a real run too, where the
  detail goes to the [run log](/concepts/logs/).
- **[`glidesh validate`](/cli/#glidesh-validate)** checks a plan without connecting to any
  host: syntax, includes, missing `file` sources.

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run --diff
```
