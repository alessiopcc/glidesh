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

## Prompted Variables

Some values should be chosen per run rather than written into the plan — the release to
deploy, a password nobody wants in a file. Declare them in `vars-prompt`:

```kdl
plan "deploy" {
    vars-prompt {
        release "Release to deploy" default="main"
        db-password "Database password" secret=#true
    }

    step "Deploy" {
        shell "deploy.sh --ref ${release} --db-pass ${db-password}"
    }
}
```

Each child is a variable name (not starting with `-`, and without `=` or whitespace, so
`--var name=value` can always answer it), the question to ask, and optionally:

- `default="…"` — taken when the answer is empty, and when there is no terminal to ask on
- `secret=#true` — read without echo, and shown as `***` in the TUI, plain output and run
  logs, exactly like a [decrypted secret](#secret-variables). A non-empty answer must be at
  least 4 bytes (4 plain ASCII characters): a shorter one could not be masked, so it is refused (asked again at a
  terminal)

`glidesh run` asks each question once, before connecting to any host, and the answers
become plan variables (`${release}`). A non-secret prompt shows its default in brackets:

```
Release to deploy [main]: v1.4.2
Database password:
```

Answer on the command line instead with `--var name=value` (repeatable):

```bash
glidesh run -i inventory.kdl -p deploy.kdl --var release=v1.4.2 --var db-password="$DB_PASSWORD"
```

- `--var` only answers a declared prompt; a name the plan does not ask for is an error.
  Errors about a `--var` name it by position (`--var #2`) and never repeat what you typed,
  which may be a password; a close declared name is suggested instead.
- When stdin is not a terminal, a prompt without `--var` takes its default. One with no
  default fails the run before it connects, and the error names every missing variable
  with the `--var` that fixes it — glidesh never waits for input it cannot get.
- `--dry-run` asks too: a preview needs the values. `glidesh validate` never asks; it
  treats prompted names as defined.
- Only the plan you run may prompt. `vars-prompt` in an [included](/concepts/plans/#including-other-plans)
  plan is an error rather than ignored, since skipping the question would leave the variable
  undefined. A name that is both prompted for and set in `vars` (or a `vars-file`) is an
  error too.
- With [inventory `plan=`](/concepts/inventory/#inline-plans) runs, several plans may
  prompt; a name any of them declares is asked once and the answer shared. It is secret if
  any plan marks it so, and keeps a `default` only when every plan gives the same one —
  otherwise it must be answered, so the result never depends on the order of the groups.

## Merge Order

When the same variable is defined at multiple levels, the most specific value wins:

```
Inventory global vars → Group vars → Host vars → Plan vars
```

[Prompted variables](#prompted-variables) take the plan-vars slot.

Built-in variables live in reserved `@`-prefixed namespaces (`@host`, `@os`, `@fact`, `@item`, `@inventory`, `@group`, `@error`) that user variables cannot collide with — a variable name may not begin with `@`. `${@error.msg}` and `${@error.task}` describe a step's failure to its [`rescue` and `always`](/advanced/rescue/#reading-the-failure) tasks.

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
under `@os`. Detection happens on every run anyway, so these cost nothing extra. Branch on
them with [`when=`](/advanced/conditionals/) — for example
`when="${@os.family} == debian"`.

| Variable | Description | Values |
|---|---|---|
| `${@os.id}` | `ID` from `/etc/os-release` | e.g. `ubuntu`, `rocky`, `alpine` |
| `${@os.version}` | `VERSION_ID` from `/etc/os-release` | e.g. `22.04`, `9.3` |
| `${@os.family}` | Distribution family | `debian`, `redhat`, `arch`, `alpine`, `suse`, `nixos` — or the raw `ID` for an OS glidesh does not recognise |
| `${@os.pkg-manager}` | Package manager the `package` module will use | `apt`, `dnf`, `yum`, `pacman`, `apk`, `zypper`, `nix` — `apt` on an OS glidesh does not recognise, since that is what `package` falls back to |
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

### Host facts

The same exec that reads `/etc/os-release` also asks the host a few questions about its
hardware and network, exposed under `@fact` — no extra round trip, and nothing to install on
the host (plain POSIX `sh`, so busybox hosts work too).

| Variable | Description | Source |
|---|---|---|
| `${@fact.hostname}` | Host name as the host reports it | `hostname` (or `uname -n`) |
| `${@fact.kernel}` | Kernel release, e.g. `6.1.0-18-amd64` | `uname -r` |
| `${@fact.arch}` | Machine architecture, e.g. `x86_64`, `aarch64` | `uname -m` |
| `${@fact.cpu.count}` | Online CPUs, e.g. `8` | `nproc` (or `getconf _NPROCESSORS_ONLN`) |
| `${@fact.mem.total-mb}` | Total memory in MiB, rounded down to an integer, e.g. `15935` | `MemTotal` in `/proc/meminfo` |
| `${@fact.ip.default}` | Source address of the default route, e.g. `10.0.0.12` | the `src` field of `ip route get 1.1.1.1` |

Every fact is always defined. A fact the host cannot report — `ip` not installed, no default
route, no `/proc/meminfo` — expands to an empty string; it never fails the connection or the
task. Guard a fact that may be missing with `when="${@fact.ip.default}"`.

```kdl
step "Size the worker pool" {
    file "/etc/app/workers.conf" src="templates/workers.conf" template=#true
}

step "Listen on the primary address" when="${@fact.ip.default}" {
    shell "app-ctl bind ${@fact.ip.default}"
}
```

with `templates/workers.conf` holding `workers = ${@fact.cpu.count}`.

OS and host facts are per host and only exist once glidesh has connected, so they resolve wherever
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

### Group members

`@group.<name>` is the list of hosts in an inventory group, for
[template loops](/advanced/loops-register/#looping-over-inventory-groups). Each host has
`name`, `address`, `user`, and `port`:

```
${for h in @group.backend}
server ${h.address}:8080;
${endfor}
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

Structured variables can be consumed two ways: in `${for}` loops inside template files (see [Template Loops](/advanced/loops-register/#template-loops)), and as the source of a step `loop=`, where each row binds `${@item.<field>}` (see [Looping over structured variables](/advanced/loops-register/#looping-over-structured-variables)). A [`when=`](/advanced/conditionals/) can test whether one exists with `defined ${name}`, but has no single value to compare.

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
