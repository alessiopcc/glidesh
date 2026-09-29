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

#### Included plans

When a plan pulls in another with [`include`](/advanced/plan-includes/), the included
plan's **plan-level `run-as` applies to its own steps**. The including plan's steps are
unaffected, and a step in the included plan can still opt out with `run-as=""`.

```kdl
// common/db.kdl
plan "db" run-as="root" {        // governs the steps below, even when included
    step "Install Postgres" {
        package "postgresql" state="present"
    }
}
```

```kdl
// plan.kdl
plan "deploy" {
    step "App config" {           // runs as the login user
        shell "whoami"
    }
    include "common/db.kdl"       // its "Install Postgres" step escalates to root
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
directly. With `run-as` set, the `file` module stages the upload in a private (`0600`)
file in `/tmp`, then the elevated shell writes its content into the destination and
removes it. The staging file's mode and owner never reach the result: a file that
already existed keeps its owner, group, and mode, and a new one is created by the
escalation target, so the umask in effect (the session's; `sudo` adds its own, `022`
by default) or the directory's default ACL decides the mode (`0644` usually) — the same
as a plain upload
([Owner and Mode](/modules/file/#owner-and-mode)). Any explicit
`owner`/`group`/`mode` you set is applied afterwards.

## Destinations other users control

An escalated write follows symlinks, as any write does. If another user could put a
symlink at the destination — or swap one in while glidesh works — they could aim the
privileged write, or `owner`/`group`/`mode`, at any file on the host. So before creating
directories, writing, or changing attributes with `run-as`, glidesh applies the rule the
Linux kernel uses for symlinks (`protected_symlinks`) — whether or not the host enables
it — and fails the task otherwise, naming the entry. Along the path to `/`, and through
every symlink on the way:

- a directory must not be writable by others, nor by a group other than root's, nor
  carry an ACL that may let others write: on Linux, an ACL on a group-writable directory
  (its group bits then show the ACL mask); on macOS and BSD, an entry allowing
  `add_file`, `add_subdirectory`, `delete_child`, `writesecurity` or `chown` — which the
  mode bits do not show — or an ACL glidesh cannot read. A deny-only ACL, like the
  `everyone deny delete` of a macOS home, is fine;
- a directory writable by others is accepted when it is sticky, like `/tmp`, and what
  sits in it already exists as a directory or link: others cannot rename that. A file or
  a missing entry right under it is refused, since anyone could create it first — so
  upload to `/tmp/app/x` with `/tmp/app` in place, not to `/tmp/x`;
- a symlink, including the destination itself, must be owned by root, the `run-as`
  user, the login user, or the owner of the directory it sits in; what it points to is
  checked the same way;
- a directory that does not exist yet is skipped: its parent decides who can create it.

For a recursive copy the rule covers the path to the destination and each file uploaded;
`owner` and `group` over the tree change a symlink found in it, never what it points
to, and `mode` skips symlinks. A `--diff` read is checked the same way: a destination
that fails shows no diff in the preview, and the upload itself is refused for the same
reason.

A directory's owner is trusted with what is in it, as the kernel trusts it: uploading
as root into `/var/www/html` owned by `www-data` works, and `www-data` could redirect
that write — the same holds for any tool that writes there as root. To deploy into a
directory a team shares (say `2775 root:devs`), write it without `run-as` as a user in
that group, or tighten the directory first.

## How it differs from the SSH user

`run-as` is independent of the SSH **login** user (`user="…"` / `${@host.user}`). You
connect as the login user and escalate to the `run-as` user; both can differ per host.
