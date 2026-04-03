---
title: Variables
description: Variable interpolation, merge order, structured vars, and inventory references.
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

### From external files

Use `vars-file` to load variables from a separate KDL file for better organization:

```kdl
plan "setup" {
    vars-file "keys.kdl"

    step "Deploy" {
        file "/etc/app/config" src="templates/config" template=#true
    }
}
```

The external file contains raw var nodes (no wrapper):

```kdl
// keys.kdl
region "us-east-1"

api-keys {
    - name="alice" key="sk-aaa"
    - name="bob" key="sk-bbb"
}
```

Inline `vars` take precedence over `vars-file` when the same key appears in both. See [External Vars Files](/concepts/plans/#external-vars-files) for details.

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

## Inventory References

You can reference any host from the inventory in templates using the `@inventory` prefix. This is useful when a template on one host needs the address or port of another host.

| Variable | Description |
|---|---|
| `${@inventory.<host>.address}` | Address of the named host |
| `${@inventory.<host>.user}` | SSH user of the named host |
| `${@inventory.<host>.port}` | SSH port of the named host |
| `${@inventory.<host>.vars.<key>}` | A resolved variable for the named host, after applying inventory merge order |

Host names are unique across the entire inventory, so the lookup is unambiguous regardless of which group the host belongs to.

`@inventory.<host>.vars` exposes the host's effective merged variables, so values may come from global, group, or host `vars` blocks.

### Example

Given this inventory:

```kdl
group "services" {
    host "caddy" "10.0.1.1" user="deploy"
    host "bifrost" "10.0.1.5" user="app" {
        vars {
            api-port "8080"
        }
    }
}
```

A template file deployed to the `caddy` host can reference `bifrost`:

```
reverse_proxy ${@inventory.bifrost.address}:${@inventory.bifrost.vars.api-port}
```

This renders to:

```
reverse_proxy 10.0.1.5:8080
```

## Structured Variables

Variables can also be lists of named fields, used for [template loops](/advanced/loops-register/#template-loops). Define them in a `vars` block using `-` nodes with named properties:

```kdl
plan "setup" {
    vars {
        // Simple scalar variable
        domain "example.com"

        // Structured variable (list of maps)
        api-keys {
            - name="alice" key="sk-aaa"
            - name="bob" key="sk-bbb"
            - name="charlie" key="sk-ccc"
        }
    }
}
```

Structured variables are available in `${for}` loops inside template files. See [Template Loops](/advanced/loops-register/#template-loops) for usage.

## Interpolation

The `${var-name}` syntax performs string replacement. It works in all module parameters and in template files (when `template=#true` on the file module).

Undefined variables cause an error — glidesh does not silently pass through unresolved references.
