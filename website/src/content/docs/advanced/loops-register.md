---
title: Loops & Register
description: Capture command output and iterate over dynamic values.
---

glidesh supports capturing command output into variables with `register` and iterating over values with `loop`.

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

## Loop

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

## Chaining Register and Loop

Register and loop compose naturally for dynamic infrastructure tasks:

```kdl
step "Find config files" {
    shell "ls /etc/myapp/conf.d/*.conf" register="config_files"
}

step "Validate each config" loop="${config_files}" {
    shell "myapp validate --config ${item}"
}
```

## Complete Example

```kdl
plan "disk-setup" {
    target "storage"

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
