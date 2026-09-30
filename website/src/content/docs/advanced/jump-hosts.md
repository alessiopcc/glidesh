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

### Every host

A `jump` node at the top level of the inventory applies to every host, grouped or not.

```kdl
jump "bastion.example.com" user="jumpuser"

group "internal" {
    host "app-1" "10.0.1.10" user="deploy"
    host "app-2" "10.0.1.11" user="deploy"
}

host "db-backup" "10.0.2.50" user="root"
```

### Group-level

Add a `jump` node inside a group. All hosts in the group inherit it, instead of the top-level
one.

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

### Reaching a host directly

`jump #false` on a group or host connects to it directly, although a wider scope names a
bastion — a host in a DMZ while the rest sit behind one, or the bastion itself when it is in
the inventory.

```kdl
jump "bastion.example.com" user="jumpuser"

group "dmz" {
    jump #false
    host "edge-1" "203.0.113.10"
}

host "bastion" "bastion.example.com" user="jumpuser" {
    jump #false
}
```

`jump #false` takes nothing else, and at the top level it is an error: leave `jump` out
instead. A scope may have one `jump` only.

### Properties

| Property | Default | Description |
|----------|---------|-------------|
| *(positional)* | *(required)* | Address of the bastion host |
| `user` | target host's user | SSH username on the bastion |
| `port` | `22` | SSH port on the bastion |

Anything else on a `jump` node — a second argument, another property, a child node — is an
error, as is a `port` that is not an integer from 1 to 65535.

## Inheritance Rules

- **Most specific wins**: a host's `jump` beats its group's, which beats the top-level one;
  each replaces the wider one entirely, its `user` and `port` included
- **Opt out**: `jump #false` on a group or host means no bastion for it; a host in such a group
  can still name its own
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
