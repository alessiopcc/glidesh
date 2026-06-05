---
title: Privilege Escalation (run-as)
description: Run tasks as another user (sudo, doas, su) while connecting over SSH as an unprivileged account.
---

By default every command runs as the user you connect with over SSH. `run-as` lets a
task escalate to another user — usually `root` — using `sudo`, `doas`, or `su`, so you
can log in as an unprivileged account and elevate only where needed.

It applies to **everything** a module does on the host: shell commands, package
installs, user/systemd/disk operations, and file uploads to root-owned paths — for both
the idempotency `check` and the `apply`.

## The model

`run-as` takes the **target user** as its value:

| Form | Meaning |
|------|---------|
| `run-as="root"` | Escalate to `root` |
| `run-as="postgres"` | Escalate to `postgres` |
| `run-as=""` | Explicitly do **not** escalate (cancels an inherited setting) |
| *(omitted)* | Inherit from the less-specific level |

The method defaults to `sudo`; override with `run-as-method="doas"` or
`run-as-method="su"`.

## Where you can set it

`run-as` is configurable at seven levels. The **most specific wins**, and the
work-side levels (task/step/plan) override the machine-side levels (host/group/global):

```
module/task  >  step  >  plan  >  host  >  group  >  global (inventory)  >  --run-as (CLI)
```

### Inventory

```kdl
// Global default for every host.
run-as "root"

group "web" run-as="root" {
    host "web-1" "10.0.0.1" user="deploy"
    host "web-2" "10.0.0.2" user="deploy" run-as-method="doas"  // override method
}

// Connecting as root already — opt out.
host "legacy" "10.0.2.1" user="root" run-as=""
```

The global default uses a top-level `run-as "<user>"` node; groups and hosts use the
`run-as="<user>"` attribute. Both accept `run-as-method="<m>"`.

### Plan

Set a default for the whole plan on the `plan` node — every step inherits it:

```kdl
plan "deploy" run-as="root" {               // every step escalates by default
    step "Install" {                         // inherits the plan's run-as
        package "nginx" state="present"
    }
    step "Audit" {
        shell "id"                           // also root (inherited)
        shell "whoami" run-as=""             // opt out -> the login user
    }
}
```

Or scope escalation to individual steps and tasks instead of the whole plan:

```kdl
plan "deploy" {
    step "Install" run-as="root" {           // whole step escalates
        package "nginx" state="present"
    }
    step "Mixed" {
        shell "whoami"                       // runs as the login user
        disk "/dev/sdb" fs="ext4" run-as="root"   // only this task escalates
    }
}
```

### CLI

The CLI flags are the lowest-precedence default — a baseline that inventory and plan
settings still override:

```bash
glidesh run -i inventory.kdl -p plan.kdl --run-as root --run-as-method sudo
```

## Passwords

`sudo` is the robust path and works two ways:

- **Passwordless** (`NOPASSWD` sudoers entry): nothing else required.
- **With a password**: supply it without echoing to the terminal. Glidesh feeds it to
  `sudo -S` on stdin.

```bash
glidesh run ... --run-as root --ask-pass              # prompt once
GLIDESH_RUNAS_PASS='…' glidesh run ... --run-as root  # from the environment
```

The password is held in process memory only — never logged or written to disk. It is
global for the run; `GLIDESH_RUNAS_PASS` takes precedence over `--ask-pass`.

## Method support and caveats

| Method | Password | Notes |
|--------|----------|-------|
| `sudo` (default) | passwordless **or** password via stdin | Recommended. |
| `doas` | passwordless only | `doas` reads passwords from a TTY; configure `nopass`/`persist` in `doas.conf`. |
| `su` | password via PTY | Requires a PTY, which merges stderr into stdout. Best-effort; prefer `sudo`. |

A denied escalation (wrong password, not a sudoer, missing TTY) is reported as a
distinct error, not confused with a command that failed on its own.

## File uploads to root-owned paths

SFTP writes as the login user, so it cannot create files in directories like `/etc`
directly. With `run-as` set, the `file` module stages the upload in `/tmp`, then moves
it into place and hands ownership to the escalation target using the elevated shell.
Any explicit `owner`/`group`/`mode` you set is applied afterwards.

## How it differs from the SSH user

`run-as` is independent of the SSH **login** user (`user="…"` / `${host.user}`). You
connect as the login user and escalate to the `run-as` user; both can differ per host.
