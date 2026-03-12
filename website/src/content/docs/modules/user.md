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
| `groups` | string(s) | Supplementary groups |
| `shell` | string | Login shell |
| `state` | string | `"present"` or `"absent"` |

## Idempotency

The module queries user properties (`id`, `getent`) and compares them against the desired state. Only mismatched properties are modified. If the user already exists with the correct uid, shell, and groups, no action is taken.

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
