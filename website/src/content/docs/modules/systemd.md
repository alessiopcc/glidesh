---
title: systemd
description: Manage systemd services — start, stop, enable, disable, restart.
---

The `systemd` module controls systemd service units.

## Usage

```kdl
systemd "nginx" {
    state "started"
    enabled #true
}

systemd "old-service" {
    state "stopped"
    enabled #false
}
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Unit name |
| `state` | string | `"started"`, `"stopped"`, or `"restarted"` |
| `enabled` | boolean | `true` or `false` — controls boot-time start |

## Idempotency

The module checks the current service state (`systemctl is-active`) and enabled status (`systemctl is-enabled`) before acting. If the service is already in the desired state, no action is taken.

The `restarted` state always triggers a restart regardless of current state.

## Example

```kdl
step "Configure services" {
    systemd "nginx" {
        state "started"
        enabled #true
    }

    systemd "postgresql" {
        state "started"
        enabled #true
    }
}
```
