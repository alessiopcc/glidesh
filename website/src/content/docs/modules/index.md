---
title: Modules
description: Overview of the glidesh module system.
---

Modules are the units of work in glidesh. Each module follows a two-phase **check/apply** pattern for idempotency.

## How Modules Work

Every module implements two operations:

1. **check** — inspects the current state of the target and returns one of:
   - `Satisfied` — the desired state already matches, no action needed
   - `Pending` — changes are required
   - `Unknown` — state cannot be determined

2. **apply** — performs the actual change on the target

When you run with `--dry-run`, only the check phase runs. This lets you preview what would change without modifying anything.

## Idempotency

Because modules check before acting, plans are safe to run repeatedly. If a package is already installed, the package module reports `Satisfied` and skips it. If a service is already running, systemd reports `Satisfied`. Only the delta is applied.

## Available Modules

### Built-in

| Module | Description |
|--------|-------------|
| [shell](/modules/shell/) | Run arbitrary shell commands |
| [package](/modules/package/) | Install or remove system packages |
| [user](/modules/user/) | Manage system users and groups |
| [systemd](/modules/systemd/) | Control systemd services |
| [container](/modules/container/) | Manage containers (Docker/Podman) |
| [file](/modules/file/) | Transfer files, templates, and fetch |
| [disk](/modules/disk/) | Format and mount block devices |

### External

Community and custom modules can extend glidesh via the [external module](/modules/external/) plugin system. External modules are standalone executables that communicate over a JSON-over-stdio protocol. They use the `external` keyword in plans:

```kdl
step "Configure nginx" {
    external "acme/nginx-vhost" "mysite" server_name="example.com"
}
```

See [Writing Plugins](/advanced/writing-plugins/) for how to build your own.
