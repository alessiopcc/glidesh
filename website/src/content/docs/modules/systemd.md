---
title: systemd
description: Manage systemd services — start, stop, enable, disable, restart, and create unit files.
---

The `systemd` module controls systemd service units. It can manage existing services or create new ones from scratch when a `command` parameter is provided.

## Usage

### Managing existing services

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

### Creating services

When `command` is provided, the module generates a systemd unit file, uploads it to `/etc/systemd/system/`, and runs `daemon-reload` before managing the service state.

```kdl
systemd "my-app" {
    command "/usr/bin/my-app --port 8080"
    description "My Application"
    user "www-data"
    group "www-data"
    working-dir "/opt/my-app"
    restart-policy "always"
    type "simple"
    after "network.target"
    wanted-by "multi-user.target"
    environment {
        PORT "8080"
        NODE_ENV "production"
    }
    state "started"
    enabled #true
}
```

Minimal form — only `command` is required for service creation:

```kdl
systemd "my-script" {
    command "/usr/local/bin/my-script.sh"
    state "started"
}
```

## Parameters

### Core parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Unit name |
| `state` | string | `"started"`, `"stopped"`, or `"restarted"` |
| `enabled` | boolean | `true` or `false` — controls boot-time start |

### Service creation parameters

These parameters are only used when `command` is present.

| Parameter | Type | Default | Maps to |
|-----------|------|---------|---------|
| `command` | string | *(required)* | `ExecStart=` |
| `description` | string | `"{name} service"` | `Description=` |
| `user` | string | *(omitted)* | `User=` |
| `group` | string | *(omitted)* | `Group=` |
| `working-dir` | string | *(omitted)* | `WorkingDirectory=` |
| `restart-policy` | string | `"on-failure"` | `Restart=` |
| `type` | string | `"simple"` | `Type=` |
| `after` | string | `"network.target"` | `After=` |
| `wanted-by` | string | `"multi-user.target"` | `WantedBy=` |
| `environment` | map | *(omitted)* | `Environment="K=V"` lines |

## Idempotency

For existing services, the module checks `systemctl is-active` and `systemctl is-enabled` before acting. If the service is already in the desired state, no action is taken.

For service creation, the module computes a SHA256 hash of the generated unit file and compares it with the remote file. The unit file is only uploaded when the content differs. After uploading, `systemctl daemon-reload` runs automatically.

The `restarted` state always triggers a restart regardless of current state.

## Examples

### Deploy a web application

```kdl
step "Deploy app service" {
    systemd "webapp" {
        command "/opt/webapp/bin/server"
        description "Web Application Server"
        user "webapp"
        working-dir "/opt/webapp"
        restart-policy "always"
        environment {
            DATABASE_URL "postgres://localhost/mydb"
            PORT "3000"
        }
        state "started"
        enabled #true
    }
}
```

### Manage infrastructure services

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
