---
title: Variables
description: Variable interpolation, merge order, and built-in host variables.
---

glidesh supports variable interpolation using `${var-name}` syntax. Variables can be defined in inventory files and plans, and are available in all module parameters and templates.

## Defining Variables

### In the inventory

```kdl
// Global variables
vars {
    deploy-user "deploy"
    app-dir "/opt/myapp"
}

group "web" {
    // Group-level variables
    vars {
        http-port 8080
    }
    host "web-1" "10.0.0.1" user="deploy"
}
```

### In the plan

```kdl
plan "deploy" {
    target "web"

    vars {
        app-image "registry.example.com/myapp:v2"
    }

    step "Deploy" {
        container "myapp" {
            image "${app-image}"
        }
    }
}
```

## Merge Order

When the same variable is defined at multiple levels, the most specific value wins:

```
Inventory global vars → Group vars → Host properties → Plan vars
```

Built-in host variables are injected last and cannot be overridden.

## Built-in Host Variables

These variables are automatically available in all interpolations — no need to define them:

| Variable | Description |
|---|---|
| `${host.name}` | Host name from inventory |
| `${host.address}` | Host address (IP or hostname) |
| `${host.user}` | SSH user for this host |
| `${host.port}` | SSH port for this host |

### Example

```kdl
step "Tag host" {
    shell "echo 'Configuring ${host.name} at ${host.address}'"
}

step "Fetch backup" {
    file "backups/${host.name}-dump.sql" {
        src "/var/backups/db.sql"
        fetch #true
    }
}
```

## Interpolation

Variable substitution is simple string replacement — no expressions, conditionals, or filters. The syntax is `${var-name}` where `var-name` matches a key from any vars block or a built-in host variable.

Undefined variables are left as-is (the literal `${var-name}` string remains).
