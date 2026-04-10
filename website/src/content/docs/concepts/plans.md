---
title: Plans
description: Describe the desired state of your infrastructure with plans.
---

A plan describes the desired state of your infrastructure. It is a declarative configuration that defines what steps to execute on your target hosts.

## Structure

```kdl
plan "deploy-app" {
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

- **mode** — `"sync"` (default) or `"async"` (can be overridden with `--mode` on CLI)
- **vars** — plan-scoped variables, merged with inventory vars (supports both scalar and [structured variables](/concepts/variables/#structured-variables))
- **vars-file** — load variables from an external KDL file (see below)

## External Vars Files

Use `vars-file` to load variables from a separate KDL file. This keeps large or reusable variable sets organized:

```kdl
plan "setup-proxy" {
    vars {
        domain "example.com"
    }

    // Load additional vars (scalar + structured) from an external file
    vars-file "keys.kdl"

    step "Deploy config" {
        file "/etc/proxy/config.conf" src="templates/config.conf" template=#true
    }
}
```

The external file contains raw var nodes — no `vars` wrapper needed:

```kdl
// keys.kdl
region "us-east-1"

api-keys {
    - name="alice" key="sk-aaa"
    - name="bob" key="sk-bbb"
}
```

Both scalar and [structured variables](/concepts/variables/#structured-variables) are supported. Inline `vars` take precedence over `vars-file` when the same key is defined in both.

Paths are resolved relative to the plan file's directory. You can use multiple `vars-file` directives.

## Steps

Each `step` node has a human-readable name (displayed in the TUI) and contains one or more module invocations. Steps always execute sequentially within each host. Module invocations within a step also execute sequentially.

## Path Resolution

All relative paths in a plan are resolved **relative to the plan file's directory**. This applies to:

- **`include`** directives — paths to other plan files
- **`vars-file`** — paths to external variable files
- **`file` module `src`** — local files to upload or use as templates

Given this layout:

```
project/
├── inventory.kdl
├── plans/
│   ├── deploy.kdl
│   ├── common/
│   │   └── security.kdl
│   ├── vars/
│   │   └── keys.kdl
│   └── files/
│       └── nginx.conf
```

Inside `plans/deploy.kdl`:

```kdl
plan "deploy" {
    vars-file "vars/keys.kdl"       // → plans/vars/keys.kdl
    include "common/security.kdl"   // → plans/common/security.kdl

    step "Upload config" {
        file "/etc/nginx/nginx.conf" src="files/nginx.conf"
        // src → plans/files/nginx.conf
    }
}
```

Absolute paths are used as-is. You can run glidesh from any directory — paths always resolve from the plan file's location.

## Including Other Plans

Plans can include other plan files using the `include` directive:

```kdl
plan "main" {
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

## Subscribe

Steps can react to changes made by earlier steps using the `subscribe` attribute. When the referenced step applies changes, the subscribing step forces a re-apply:

```kdl
plan "web-server" {
    step "Deploy config" {
        file "/etc/nginx/nginx.conf" src="files/nginx.conf" template=#true
    }

    step "Restart nginx" subscribe="Deploy config" {
        systemd "nginx" state="restarted"
    }
}
```

See [Subscribe](/advanced/subscribe/) for details.
