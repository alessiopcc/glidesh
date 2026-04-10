---
title: Subscribe
description: Restart services or recreate containers when upstream steps change.
---

The `subscribe` attribute lets a step react to changes made by an earlier step. When the referenced step applies changes, the subscribing step forces a re-apply even if its own check reports the resource is already satisfied.

This is useful for restarting services after a config file changes, or recreating containers after volume data is updated.

## Usage

Add `subscribe="Step Name"` to any step, referencing an earlier step by its exact name:

```kdl
plan "web-server" {
    step "Deploy nginx config" {
        file "/etc/nginx/sites-available/default" {
            src "files/default.conf"
            template #true
        }
    }

    step "Restart nginx" subscribe="Deploy nginx config" {
        systemd "nginx" state="restarted"
    }
}
```

If "Deploy nginx config" uploads a new file, "Restart nginx" runs `systemctl restart nginx`. If the config is unchanged, the restart is skipped entirely.

## Multiple Subscriptions

Subscribe to multiple steps with a comma-separated list:

```kdl
plan "deploy" {
    step "Upload app binary" {
        file "/opt/myapp/bin/server" src="build/server" mode="0755"
    }

    step "Deploy config" {
        file "/etc/myapp/config.toml" src="templates/config.toml" template=#true
    }

    step "Restart app" subscribe="Upload app binary, Deploy config" {
        systemd "myapp" state="restarted"
    }
}
```

The subscribing step fires if **any** of the referenced steps made changes.

## Chaining

Subscriptions chain naturally. If step B subscribes to step A, and step C subscribes to step B, then a change in A triggers B, which triggers C:

```kdl
plan "stack" {
    step "Deploy config" {
        file "/etc/myapp/config.toml" src="templates/config.toml" template=#true
    }

    step "Restart app" subscribe="Deploy config" {
        systemd "myapp" state="restarted"
    }

    step "Health check" subscribe="Restart app" {
        shell "curl -sf http://localhost:8080/health"
    }
}
```

## Rules

- **Step names must be unique** within a plan (including steps from [included plans](/advanced/plan-includes/)). Duplicate names are rejected at parse time.
- Referenced steps must appear **before** the subscribing step in the plan. Forward references are rejected at parse time.
- Step names must match exactly (case-sensitive).
- A force-applied step always propagates as changed to its own subscribers, enabling reliable chaining.
- When combined with `loop`, the subscribe fires on every iteration if the referenced step changed.
- Subscribe works with all modules. It is most useful with `systemd state="restarted"` and `container` steps, but any module can be a subscriber or a dependency.
