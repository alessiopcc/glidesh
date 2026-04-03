---
title: Inventory
description: Define your target machines using the KDL inventory format.
---

An inventory file defines the machines glidesh connects to. It uses [KDL](https://kdl.dev) format.

## Structure

```kdl
// Global variables — inherited by all groups and hosts
vars {
    deploy-user "deploy"
    ssh-key "~/.ssh/id_ed25519"
}

group "web" {
    vars {
        http-port 8080
    }
    host "web-1" "10.0.0.1" user="deploy" port=22
    host "web-2" "10.0.0.2" user="deploy"
    host "web-3" "web3.example.com"
}

group "db" {
    host "db-1" "10.0.1.1" user="root" port=2222
    host "db-2" "10.0.1.2" user="root"
}

// Ungrouped host
host "monitoring" "10.0.2.1" user="admin"
```

## Hosts

Each `host` node takes two positional arguments:

1. **Name** — a human-readable identifier (shown in the TUI)
2. **Address** — IP or hostname to connect to

Optional properties:
- `user` — SSH username (overrides group/global vars)
- `port` — SSH port (default: 22)
- `plan` — path to a plan file to run on this host (see [Inline Plans](#inline-plans))

## Groups

A `group` node collects hosts under a name that can be targeted in plans or via `--target` on the CLI. Groups can define their own `vars` block and an optional `plan=` attribute to link a plan file (see [Inline Plans](#inline-plans)).

## Jump Hosts

A `jump` node configures an SSH bastion (jump host) that glidesh connects through before reaching the target. Jump hosts can be set at group level (inherited by all hosts) or per-host.

```kdl
group "internal" {
    jump "bastion.example.com" user="jumpuser" port=2222

    host "app-1" "10.0.1.10" user="deploy"
    host "app-2" "10.0.1.11" user="deploy"

    host "app-3" "10.0.1.12" user="deploy" {
        jump "bastion-us.example.com"
    }
}

host "db-backup" "10.0.2.50" user="root" {
    jump "bastion.example.com"
}
```

Optional properties on `jump`:
- `user` — SSH username for the bastion (defaults to the target host's user)
- `port` — SSH port on the bastion (default: 22)

**Resolution order:** a host-level `jump` overrides the group-level `jump`. If no `user` is set on the jump node, it inherits the resolved user of the target host.

## Inline Plans

Instead of passing `--plan` on the CLI, you can link a plan file directly to a group or host using the `plan=` attribute. When you run `glidesh run -i inventory.kdl` without `--plan`, glidesh automatically runs each linked plan against its associated hosts.

```kdl
group "web" plan="plans/web.kdl" {
    host "web-1" "10.0.0.1" user="deploy"
    host "web-2" "10.0.0.2" user="deploy"
}

group "db" plan="plans/db.kdl" {
    host "db-1" "10.0.1.1" user="root"
}

// Ungrouped hosts can also have a plan
host "monitoring" "10.0.2.1" user="admin" plan="plans/monitoring.kdl"
```

This lets different groups run different plans in a single invocation — useful when your infrastructure has distinct roles that each need their own configuration.

Plan paths are resolved relative to the inventory file's directory.

:::note
When `--plan` is provided on the CLI, it overrides all `plan=` attributes in the inventory.
:::

## Variables

Variables are defined in `vars` blocks and follow a scoping hierarchy:

```
Global vars → Group vars → Host properties
```

The most specific value wins. Variables can be referenced in plans using `${var-name}` syntax.

See [Variables](/concepts/variables/) for full details on variable interpolation and merge order.
