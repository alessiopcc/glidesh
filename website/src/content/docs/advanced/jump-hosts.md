---
title: Jump Hosts
description: Connect to targets through SSH bastion hosts with group and per-host configuration.
---

Many production environments place internal machines behind a bastion (jump host). Glidesh supports this natively — no external tooling or SSH config required.

## How It Works

When a jump host is configured, glidesh:

1. Connects and authenticates to the bastion via SSH
2. Opens a `direct-tcpip` tunnel through the bastion to the target host
3. Runs the SSH protocol over the tunnel to authenticate with the target

All modules (shell, file, package, etc.) work transparently over the tunneled connection.

## Configuration

### Group-level

Add a `jump` node inside a group. All hosts in the group inherit it.

```kdl
group "internal" {
    jump "bastion.example.com" user="jumpuser" port=2222

    host "app-1" "10.0.1.10" user="deploy"
    host "app-2" "10.0.1.11" user="deploy"
}
```

### Per-host

Add a `jump` child node inside a host to set or override the jump host for that specific machine.

```kdl
group "internal" {
    jump "bastion-eu.example.com"

    host "eu-app" "10.0.1.10" user="deploy"

    host "us-app" "10.0.2.10" user="deploy" {
        jump "bastion-us.example.com" port=2222
    }
}
```

Ungrouped hosts can also have a jump host:

```kdl
host "db-backup" "10.0.2.50" user="root" {
    jump "bastion.example.com"
}
```

### Properties

| Property | Default | Description |
|----------|---------|-------------|
| *(positional)* | *(required)* | Address of the bastion host |
| `user` | target host's user | SSH username on the bastion |
| `port` | `22` | SSH port on the bastion |

## Inheritance Rules

- **Group → host**: all hosts in a group inherit the group's `jump` node
- **Host override**: a `jump` inside a host replaces the group's jump entirely
- **User fallback**: if `user` is omitted on the jump node, it defaults to the resolved user of the target host
- **Same SSH key**: the same key is used for both the bastion and the target

## Complete Example

```kdl
vars {
    deploy-user "deploy"
}

group "production" {
    jump "bastion.prod.example.com" user="jumpuser"

    host "web-1" "10.0.1.10"
    host "web-2" "10.0.1.11"
    host "api-1" "10.0.1.20" user="api" {
        jump "bastion-api.prod.example.com" user="admin" port=2222
    }
}
```

In this setup:
- `web-1` and `web-2` connect through `bastion.prod.example.com` as `jumpuser`
- `api-1` connects through `bastion-api.prod.example.com` as `admin` on port 2222

See the [jump-host example](https://github.com/alessiopcc/glidesh/tree/main/examples/jump-host) for a runnable demo.
