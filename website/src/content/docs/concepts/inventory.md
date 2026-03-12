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

## Groups

A `group` node collects hosts under a name that can be targeted in plans or via `--target` on the CLI. Groups can define their own `vars` block.

## Variables

Variables are defined in `vars` blocks and follow a scoping hierarchy:

```
Global vars → Group vars → Host properties
```

The most specific value wins. Variables can be referenced in plans using `${var-name}` syntax.

See [Variables](/concepts/variables/) for full details on variable interpolation and merge order.
