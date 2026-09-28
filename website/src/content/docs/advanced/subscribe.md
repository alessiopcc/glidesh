---
title: Subscribe
description: Restart services or recreate containers when upstream steps change.
---

The `subscribe` attribute lets a step react to changes made by an earlier step. When a step it subscribes to changes something, the subscribing step is **triggered**: each of its tasks redoes its work — a service restarts, a container is recreated, a command runs — even though its own state is already in place.

This is how a service picks up a new configuration file, or a container new data, only when they actually changed.

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

If "Deploy nginx config" uploads a new file, "Restart nginx" runs `systemctl restart nginx`. If the config is unchanged, nginx is only kept running, and the restart does not run.

## What a triggered task does

Each module decides what redoing its work means:

| Module | Triggered | Not triggered |
|--------|-----------|---------------|
| [`systemd`](/modules/systemd/) `state="restarted"` | restarts | kept running, not restarted |
| [`systemd`](/modules/systemd/) `state="started"` | restarts, so a changed configuration is loaded | as usual |
| [`container`](/modules/container/) `state="running"` | recreated | as usual |
| [`container`](/modules/container/) `state="run-once"` | runs again, even when its `check` passes | as usual |
| [`shell`](/modules/shell/) | runs, even when its `check` passes | as usual |
| [external](/advanced/writing-plugins/#triggered-tasks) | whatever the plugin's `check` answers when told it was triggered | as usual |
| `file`, `package`, `user`, `disk`, `nix`, `systemd state="stopped"`, `container state="stopped"`/`"absent"` | nothing to redo: reported `ok` | as usual |

`systemd state="restarted"` depends on where it is written. In a step with `subscribe` it is a
handler, as above. In a step without `subscribe` it restarts on every run.

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

- **Step names must be unique** within a plan (including steps from [included plans](/advanced/plan-includes/)). Duplicate names are rejected when the plan is loaded, by `glidesh validate` as well as `run`.
- Referenced steps must appear **before** the subscribing step in the plan. Forward references and names that match no step are rejected the same way.
- Step names must match exactly (case-sensitive).
- A triggered step counts as changed when its tasks did something — a restart, a recreate, a command — and that is what triggers its own subscribers in turn. A triggered task with nothing to redo reports `ok` and triggers nothing.
- Under `--dry-run`, a triggered task is reported as `would change` exactly when the real run would redo its work, and counts toward the summary the same way.
- When combined with `loop`, the subscribe fires on every iteration if the referenced step changed.
- A step [skipped by `when`](/advanced/conditionals/) or left out by [tags](/advanced/tags/) made no change, so it never fires its subscribers. A subscribing step's own `when` is still checked first: if it does not hold, the step is skipped even though what it subscribes to changed.
- Any step can be subscribed to. A subscriber is most useful with `systemd`, `container` or `shell` tasks — the modules with work to redo.
