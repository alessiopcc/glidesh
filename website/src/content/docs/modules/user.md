---
title: user
description: Manage system users and group membership.
---

The `user` module creates, modifies, or deletes system users.

## Usage

```kdl
user "appuser" {
    uid 1001
    groups "docker" "www-data"
    shell "/bin/bash"
    state "present"
}

user "olduser" {
    state "absent"
}
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Username |
| `uid` | integer | Numeric user ID |
| `groups` | string or list | Supplementary groups: `groups "docker" "sudo"`, a `-` list block, or `groups="docker,sudo"` |
| `shell` | string | Login shell |
| `state` | string | `"present"` (default) or `"absent"` |

## Idempotency

The module queries user properties (`id`, `getent`) and compares them against the desired state. Only mismatched properties are modified. If the user already exists with the correct uid, shell, and groups, no action is taken.

Every module's rules side by side: [Idempotency & Drift](/concepts/idempotency/).

## Example

```kdl
step "Create deploy users" {
    user "deploy" {
        uid 1000
        groups "docker" "sudo"
        shell "/bin/bash"
        state "present"
    }

    user "monitoring" {
        uid 1001
        groups "docker"
        shell "/bin/bash"
        state "present"
    }
}
```
