---
title: Loops & Register
description: Capture command output, iterate over dynamic values, and use template loops.
---

glidesh has two loop mechanisms: **step loops** that repeat an entire step, and **template loops** that generate repeated blocks inside template files.

## Register

Use `register="var_name"` on any step to capture the module's output into a variable for later use:

```kdl
step "Get disk list" {
    shell "lsblk -dn -o NAME" register="disk_names"
}

step "Show disks" {
    shell "echo 'Found disks: ${disk_names}'"
}
```

The output is trimmed of leading/trailing whitespace before being stored.

## Step Loops

Use `loop="${var_name}"` on a step to iterate over newline-separated values. Each iteration injects the current value as `${item}`:

```kdl
step "List disks" {
    shell "lsblk -dn -o NAME" register="disks"
}

step "Format each disk" loop="${disks}" {
    disk "/dev/${item}" {
        fs "ext4"
        mount "/mnt/${item}"
    }
}
```

### Chaining Register and Loop

Register and loop compose naturally for dynamic infrastructure tasks:

```kdl
step "Find config files" {
    shell "ls /etc/myapp/conf.d/*.conf" register="config_files"
}

step "Validate each config" loop="${config_files}" {
    shell "myapp validate --config ${item}"
}
```

## Template Loops

Template loops generate repeated content **inside a template file**. This is different from step loops — instead of repeating an entire step, you repeat lines within a file.

Use `${for <binding> in <collection>}...${endfor}` syntax in any file with `template=#true`.

### Defining the data

Define [structured variables](/concepts/variables/#structured-variables) in your plan's `vars` block:

```kdl
plan "setup-proxy" {
    vars {
        api-keys {
            - name="alice" key="sk-aaa"
            - name="bob" key="sk-bbb"
            - name="charlie" key="sk-ccc"
        }
    }

    step "Deploy config" {
        file "/etc/proxy/config.conf" src="templates/config.conf" template=#true
    }
}
```

### Using the loop in a template

Inside `templates/config.conf`:

```
# API key mapping
${for k in api-keys}
key "${k.name}" = "${k.key}"
${endfor}
```

This renders to:

```
# API key mapping

key "alice" = "sk-aaa"

key "bob" = "sk-bbb"

key "charlie" = "sk-ccc"

```

Each item in the collection is a map of named fields. Access fields with dot notation: `${k.name}`, `${k.key}`, etc. The binding name (`k` in this example) is your choice.

### Looping over inventory groups

You can also loop over all hosts in an inventory group using `@group.<name>`:

```kdl
// inventory.kdl
group "backend" {
    host "api-1" "10.0.1.10" user="deploy"
    host "api-2" "10.0.1.11" user="deploy"
    host "api-3" "10.0.1.12" user="deploy"
}
```

Inside a template file:

```
upstream backend {
    ${for h in @group.backend}
    server ${h.address}:8080;
    ${endfor}
}
```

Renders to:

```
upstream backend {

    server 10.0.1.10:8080;

    server 10.0.1.11:8080;

    server 10.0.1.12:8080;

}
```

Each host in the group exposes these fields: `name`, `address`, `user`, `port`.

### Combining loops with inventory references

Template loops and [inventory references](/concepts/variables/#inventory-references) work together. Here's a complete example — a Caddyfile that maps API keys to developers and proxies to another host:

```kdl
// inventory.kdl
group "services" {
    host "caddy" "10.0.1.1" user="deploy"
    host "bifrost" "10.0.1.5" user="app" {
        vars { api-port "8080" }
    }
}
```

```kdl
// plan.kdl
plan "setup-caddy" {
    vars {
        api-keys {
            - name="k1" value="sk-k-asd"
            - name="k2" value="sk-k-das"
        }
    }

    step "Deploy Caddyfile" {
        file "/etc/caddy/Caddyfile" src="templates/Caddyfile" template=#true
    }
}
```

`templates/Caddyfile`:

```
llm.example.com {
    map {http.request.header.Authorization} {developer_from_bearer} {
        ${for key in api-keys}
        ~Bearer\s+${key.value}   "${key.name}"
        ${endfor}
        default               ""
    }

    handle {
        reverse_proxy ${@inventory.bifrost.address}:${@inventory.bifrost.vars.api-port}
    }
}
```

## Step Loop Example

```kdl
plan "disk-setup" {
    step "Discover available disks" {
        shell "lsblk -dn -o NAME | grep -v sda" register="available_disks"
    }

    step "Format and mount each disk" loop="${available_disks}" {
        disk "/dev/${item}" {
            fs "ext4"
            mount "/mnt/${item}"
            opts "defaults,noatime"
        }
    }

    step "Verify mounts" {
        shell "df -h | grep /mnt"
    }
}
```

See the [disk-management example](/examples/#disk-management) for a complete working example.
