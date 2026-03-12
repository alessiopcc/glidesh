---
title: package
description: Install or remove packages using the system package manager.
---

The `package` module manages system packages. It automatically detects the host's package manager.

## Usage

```kdl
package "nginx" state="present"
package "vim" state="present"
package "telnet" state="absent"
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Package name |
| `state` | string | `"present"` (install) or `"absent"` (remove) |

## Supported Package Managers

| Manager | Distributions |
|---------|--------------|
| `apt` | Debian, Ubuntu |
| `dnf` | Fedora, RHEL 8+ |
| `yum` | CentOS, RHEL 7 |
| `pacman` | Arch Linux |
| `apk` | Alpine Linux |
| `zypper` | openSUSE |

The package manager is detected automatically based on the target host's OS.

## Idempotency

The module checks if the package is already installed (or already absent) before acting. No action is taken if the current state matches the desired state.

## Example

```kdl
step "Install web stack" {
    package "nginx" state="present"
    package "certbot" state="present"
    package "curl" state="present"
}

step "Remove unused" {
    package "telnet" state="absent"
}
```
