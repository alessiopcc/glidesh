---
title: Plan Includes
description: Compose plans from reusable plan files.
---

Plans can include other plan files using the `include` directive, enabling modular and reusable infrastructure definitions.

## Usage

```kdl
plan "full-setup" {
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

Every relative path inside an included plan — its own `include`s, its `vars-file`s, and
`file` sources — resolves from **that plan's** directory, so a reusable plan can keep its
files beside it:

```
plans/
├── main.kdl
└── roles/web/
    ├── plan.kdl
    ├── defaults.kdl
    └── nginx.conf
```

```kdl
// plans/main.kdl
plan "main" {
    include "roles/web/plan.kdl"
}
```

```kdl
// plans/roles/web/plan.kdl
plan "web" {
    vars-file "defaults.kdl"                   // → plans/roles/web/defaults.kdl

    step "Configure nginx" {
        file "/etc/nginx/nginx.conf" src="nginx.conf" template=#true
        // src → plans/roles/web/nginx.conf
    }
}
```

:::caution[Changed after v1.2.0]
Earlier versions resolved an included plan's `file` sources from the top-level plan's
directory. [`glidesh validate`](/cli/#glidesh-validate) points at a source it finds only
there: `…/nginx.conf exists, but an included plan's sources resolve from its own directory`.
Move the file next to the included plan, or give `src` a path relative to it.
:::

## How It Works

Included plan steps are **inlined** at parse time. The `include` directive is replaced with the steps from the included plan. The result is a flat sequence of steps, as if they were written directly in the parent plan.

## Variable Merging

Included plans can define their own `vars` block and `vars-file`s. These are merged into the parent plan's variables, with the **parent's values taking precedence** on conflicts. The merged variables are visible to every step, not only the included plan's own.

When two plans included side by side define the same variable, the one included first wins; within an included plan, inline `vars` win over its `vars-file`s, as they do at the top level.

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
