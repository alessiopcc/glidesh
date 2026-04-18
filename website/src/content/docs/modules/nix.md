---
title: nix
description: Cross-distro Nix package management and environments.
---

The `nix` module manages Nix packages and environments on **any** Linux distribution — not just NixOS. If Nix is not installed on the target, it can be auto-installed with `install=#true`, similar to how the [container](/modules/container/) module handles runtime installation.

## Usage

```kdl
// Install a package (auto-install Nix if missing)
nix "ripgrep" install=#true

// Run a command in an ephemeral Nix shell
nix "python3 -c 'import sys; print(sys.version)'" {
    action "shell"
    packages {
        - "python3"
    }
}

// Build a flake derivation
nix ".#myapp" {
    action "build"
    out-link "/opt/myapp"
}
```

## Actions

The module dispatches on the `action` parameter. Default is `"install"`.

### install (default)

Install or remove Nix packages. For the default user profile, uses `nix profile install` with a fallback to `nix-env`. The idempotency check looks at **both** mechanisms, so a package installed via either is treated as satisfied.

```kdl
nix "htop" install=#true
nix "htop" state="absent"

// Install into the multi-user default profile (visible to any user / systemd).
// Typically requires connecting as root.
nix "htop" install=#true profile="default"

// Or an explicit profile path
nix "htop" profile="/nix/var/nix/profiles/custom"
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Package name |
| `state` | string | `"present"` (default) or `"absent"` |
| `install` | boolean | Auto-install Nix runtime if missing |
| `profile` | string | Target profile: `"user"` (default, `~/.nix-profile`), `"default"` or `"system"` (`/nix/var/nix/profiles/default`), or an explicit path |

:::note
Non-user profiles (`"default"` / `"system"` / custom path) are written with `nix profile --profile <path>` only — `nix-env` does not support `--profile` the same way. Writing to `/nix/var/nix/profiles/default` typically requires root; either SSH in as root or have your glidesh SSH user be `root`.
:::

### shell

Run a command inside an ephemeral Nix shell with specific packages available. Packages are only present for the duration of the command — nothing is permanently installed.

The command is passed to `bash -c` inside the Nix shell, so pipes, redirections, variable expansion, and quoted arguments work as expected:

```kdl
nix "python3 -c 'print(42)'" {
    action "shell"
    packages {
        - "python3"
        - "curl"
    }
}

// Pipes, redirects, and shell syntax are preserved
nix "curl -sf https://example.com | jq . > /tmp/out.json" {
    action "shell"
    packages { - "curl"; - "jq" }
}
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Command to run (interpreted by `bash -c`) |
| `packages` | list | Nix packages to make available. Entries containing `#` are treated as explicit flake refs (e.g. `"github:user/repo#tool"`); bare names become `nixpkgs#<name>`. |
| `install` | boolean | Auto-install Nix runtime if missing |

### build

Build a Nix derivation on the remote host.

```kdl
nix ".#myapp" {
    action "build"
    out-link "/opt/myapp"
}
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Flake reference or derivation path |
| `out-link` | string | Output symlink path (default: `"result"`) |
| `install` | boolean | Auto-install Nix runtime if missing |

### channel

Manage Nix channels.

```kdl
nix "nixpkgs-unstable" {
    action "channel"
    url "https://nixos.org/channels/nixpkgs-unstable"
}

nix "old-channel" {
    action "channel"
    state "absent"
}
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Channel name |
| `url` | string | Channel URL (required for `state="present"`) |
| `state` | string | `"present"` (default) or `"absent"` |
| `update` | boolean | Run `nix-channel --update` after (default: `true`) |
| `install` | boolean | Auto-install Nix runtime if missing |

### flake-update

Update flake inputs on the remote host.

```kdl
nix "/etc/nixos" {
    action "flake-update"
    input "nixpkgs"
}
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Flake directory path |
| `input` | string | Specific input to update (optional — updates all if omitted) |
| `install` | boolean | Auto-install Nix runtime if missing |

### gc

Garbage collect the Nix store.

```kdl
nix "cleanup" {
    action "gc"
    older-than "30d"
}
```

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Label (not used functionally) |
| `older-than` | string | Delete paths older than this (e.g. `"30d"`) |
| `install` | boolean | Auto-install Nix runtime if missing |

## Auto-Install

When `install=#true` is set and Nix is not found on the target, the module installs it using the [Determinate Systems installer](https://install.determinate.systems/nix). This works on all major Linux distributions.

```kdl
step "Install tools via Nix" {
    // First task auto-installs Nix, subsequent tasks reuse it
    nix "htop" install=#true
    nix "ripgrep"
    nix "jq"
}
```

## Running installed Nix tools from `shell`

After `nix "…" install=#true` places a binary in the user's profile, it lives at `~/.nix-profile/bin/<tool>` (or `/nix/var/nix/profiles/default/bin/<tool>` in the multi-user default profile). SSH non-interactive sessions do **not** source the profile scripts that put these on `PATH`, so a plain `shell "mytool"` will fail.

Three options, in order of preference:

1. **Use the [`shell` module with `login=#true`](/modules/shell/#login-shell-environment-logintrue)** — sources `/etc/profile` and picks up the Nix PATH automatically:

   ```kdl
   shell "rg TODO" login=#true
   ```

2. **Call the absolute path** — reliable and what you want inside `systemd` unit files:

   ```kdl
   shell "/nix/var/nix/profiles/default/bin/rg TODO"
   ```

3. **Use `action "shell"` on this module** for an ephemeral environment (no persistent install):

   ```kdl
   nix "rg TODO" {
       action "shell"
       packages { - "ripgrep" }
   }
   ```

## Using Nix-installed binaries in systemd units

Systemd runs commands with an empty environment and no `PATH`, so unit files must reference absolute paths. Two styles:

**Follow the profile symlink** — auto-upgrades when the package is reinstalled:

```kdl
file "/etc/systemd/system/mytool.service" {
    content "[Unit]
Description=My tool
After=network-online.target

[Service]
ExecStart=/nix/var/nix/profiles/default/bin/mytool --flag
Restart=on-failure

[Install]
WantedBy=multi-user.target
"
}
systemd "mytool" state="started" enabled=#true
```

**Pin the store path** (reproducible but requires a unit rewrite on every upgrade) — resolve with `readlink -f` and write the exact `/nix/store/<hash>-mytool-<ver>/bin/mytool` path into the unit.

:::note
Per-user Nix profiles (under `$HOME/.nix-profile`) are only visible to that user. If your systemd service runs as `root`, install into the multi-user default profile with `profile="default"` (requires root SSH), or set `User=<installing-user>` on the service.
:::

## Idempotency

- **install:** Checks if the package is already installed before acting. For the user profile the check matches either a `nix profile` or a legacy `nix-env` install; for a non-user profile (`profile="default"` / custom path) only the profile manifest is consulted.
- **shell:** Always runs (imperative command).
- **build:** Always rebuilds (cannot cheaply determine if the derivation changed).
- **channel:** Checks if the channel already exists.
- **flake-update:** Always runs.
- **gc:** Always runs.

## Example

See the [nix-deploy example](/examples/#nix-deploy) for a complete plan using multiple Nix actions.
