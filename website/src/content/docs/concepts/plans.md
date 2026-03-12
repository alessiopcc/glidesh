---
title: Plans
description: Describe the desired state of your infrastructure with plans.
---

A plan describes the desired state of your infrastructure. It is a declarative configuration that defines what steps to execute on your target hosts.

## Structure

```kdl
plan "deploy-app" {
    target "web"
    mode "sync"

    vars {
        app-image "registry.example.com/myapp:latest"
        app-port 8080
    }

    step "Install packages" {
        package "nginx" state="present"
        package "curl" state="present"
    }

    step "Deploy container" {
        container "myapp" {
            image "${app-image}"
            state "running"
            restart "always"
            ports {
                - "${app-port}:80"
            }
            environment {
                DATABASE_URL "postgres://db-1:5432/app"
            }
        }
    }
}
```

## Top-Level Properties

- **target** — which inventory group (or host) this plan applies to
- **mode** — `"sync"` (default) or `"async"` (can be overridden with `--mode` on CLI)
- **vars** — plan-scoped variables, merged with inventory vars

## Steps

Each `step` node has a human-readable name (displayed in the TUI) and contains one or more module invocations. Steps always execute sequentially within each host. Module invocations within a step also execute sequentially.

## Including Other Plans

Plans can include other plan files using the `include` directive:

```kdl
plan "main" {
    target "web"

    step "Setup" {
        package "nginx" state="present"
    }

    include "common/security.kdl"

    step "Deploy" {
        shell "deploy.sh"
    }
}
```

See [Plan Includes](/advanced/plan-includes/) for details.
