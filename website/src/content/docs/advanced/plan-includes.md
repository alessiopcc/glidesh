---
title: Plan Includes
description: Compose plans from reusable plan files.
---

Plans can include other plan files using the `include` directive, enabling modular and reusable infrastructure definitions.

## Usage

```kdl
plan "full-setup" {
    target "web"

    step "Base packages" {
        package "curl" state="present"
        package "vim" state="present"
    }

    include "common/security.kdl"
    include "common/monitoring.kdl"

    step "Deploy app" {
        shell "/opt/deploy.sh"
    }
}
```

## Path Resolution

Include paths are resolved relative to the directory of the including plan file. Given this file structure:

```
plans/
├── main.kdl
└── common/
    ├── security.kdl
    └── monitoring.kdl
```

The `include "common/security.kdl"` in `main.kdl` resolves to `plans/common/security.kdl`.

## How It Works

Included plan steps are **inlined** at parse time. The `include` directive is replaced with the steps from the included plan. The result is a flat sequence of steps, as if they were written directly in the parent plan.

## Variable Merging

Included plans can define their own `vars` block. These are merged with the parent plan's variables, with the **parent's values taking precedence** on conflicts.

```kdl
// common/security.kdl
plan "security" {
    vars {
        ssh-port 22
        fail2ban-maxretry 5
    }

    step "Install fail2ban" {
        package "fail2ban" state="present"
    }
}
```

If the parent plan also defines `ssh-port`, the parent's value wins.

## Circular Include Detection

glidesh detects circular includes and reports an error. If `a.kdl` includes `b.kdl` and `b.kdl` includes `a.kdl`, the parser will fail with a clear error message.
