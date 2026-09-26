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
    host "web-2" "10.0.0.2" user="deploy" {
        // Host-level variables — override group and global vars
        vars {
            http-port 9090
            app-env "staging"
        }
    }
}
```

### In the plan

```kdl
plan "deploy" {
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
Inventory global vars → Group vars → Host vars → Plan vars
```

Built-in variables live in reserved `@`-prefixed namespaces (`@host`, `@os`, `@item`, `@inventory`, `@group`) that user variables cannot collide with — a variable name may not begin with `@`.

## Built-in Host Variables

These variables are automatically available in all interpolations — no need to define them:

| Variable | Description |
|---|---|
| `${@host.name}` | Host name from inventory |
| `${@host.address}` | Host address (IP or hostname) |
| `${@host.user}` | SSH user for this host |
| `${@host.port}` | SSH port for this host |

### Example

```kdl
step "Tag host" {
    shell "echo 'Configuring ${@host.name} at ${@host.address}'"
}

step "Fetch backup" {
    file "backups/${@host.name}-dump.sql" {
        src "/var/backups/db.sql"
        fetch #true
    }
}
```

## Built-in OS Facts

glidesh detects each host's operating system when it connects, and exposes what it found
under `@os`. Detection happens on every run anyway, so these cost nothing extra.

| Variable | Description | Values |
|---|---|---|
| `${@os.id}` | `ID` from `/etc/os-release` | e.g. `ubuntu`, `rocky`, `alpine` |
| `${@os.version}` | `VERSION_ID` from `/etc/os-release` | e.g. `22.04`, `9.3` |
| `${@os.family}` | Distribution family | `debian`, `redhat`, `arch`, `alpine`, `suse`, `nixos` — or the raw `ID` for an OS glidesh does not recognise |
| `${@os.pkg-manager}` | Package manager the `package` module will use | `apt`, `dnf`, `yum`, `pacman`, `apk`, `zypper`, `nix` |
| `${@os.init}` | Init system | `systemd`, `openrc`, `unknown` |
| `${@os.container-runtime}` | Container runtime found on the host | `podman`, `docker`, or empty if neither is installed |
| `${@os.nix-installed}` | Whether Nix is available | `true`, `false` |

`${@os.container-runtime}` is always defined — a host with no runtime expands it to an empty
string rather than failing the task on an undefined variable.

### Example

```kdl
step "Report platform" {
    shell "echo ${@os.id} ${@os.version} uses ${@os.pkg-manager}"
}

step "Render config" {
    file "/etc/app/platform.conf" src="templates/platform.conf" template=#true
}
```

Inside `templates/platform.conf`, `${@os.family}` and the rest resolve like any other
variable.

OS facts are per host and only exist once glidesh has connected, so they resolve wherever
`${@host.*}` does — module arguments and `file` templates — but not in `include` or
`vars-file` paths, which are read before any host is contacted.

## Inventory References

You can reference any host from the inventory in templates using the `@inventory` prefix. This is useful when a template on one host needs the address or port of another host.

| Variable | Description |
|---|---|
| `${@inventory.<host>.address}` | Address of the named host |
| `${@inventory.<host>.user}` | SSH user of the named host |
| `${@inventory.<host>.port}` | SSH port of the named host |
| `${@inventory.<host>.vars.<key>}` | A resolved variable for the named host, after applying inventory merge order |

Host names are unique across the entire inventory, so the lookup is unambiguous regardless of which group the host belongs to.

`@inventory.<host>.vars` exposes the host's effective merged variables — values may come from global, group, or host-level `vars` blocks, with the most specific level winning.

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

Variables can also be lists of named fields, used for [template loops](/advanced/loops-register/#template-loops) and [structured step loops](/advanced/loops-register/#looping-over-structured-variables). Define them in a `vars` block using `-` nodes with named properties:

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

Structured variables can be consumed two ways: in `${for}` loops inside template files (see [Template Loops](/advanced/loops-register/#template-loops)), and as the source of a step `loop=`, where each row binds `${@item.<field>}` (see [Looping over structured variables](/advanced/loops-register/#looping-over-structured-variables)).

## Secret Variables

Any variable value can be an encrypted `secret:v1:…` token. glidesh decrypts it in memory at run
time and scrubs the plaintext from all output. Secrets live in a committed `secrets.kdl` and merge
in at the inventory-global tier. See [Secrets](/concepts/secrets/) for the full workflow.

```kdl
// secrets.kdl
db-password "secret:v1:k6Ge72IL-UQ1lGtRm962…"
```

```kdl
step "Configure" {
    shell "app --db-pass ${db-password}"
}
```

## Interpolation

The `${var-name}` syntax performs string replacement. It works in all module parameters and in template files (when `template=#true` on the file module).

Undefined variables cause an error — glidesh does not silently pass through unresolved references.
