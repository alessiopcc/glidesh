---
title: Local Hosts
description: Apply a plan to the machine running glidesh, without SSH, for container image builds and the machine you are on.
---

glidesh normally reaches every host over SSH. A **local host** is the machine running glidesh itself: the same plan, the same modules, but no SSH server, key or agent in between. That makes glidesh usable where starting an SSH server just to reach yourself is awkward, such as a `RUN` step that builds a container image.

## Running a Plan Locally

`run --local` applies a plan to this machine, with no inventory:

```bash
glidesh run --local -p plan.kdl
```

The host is called `local` (`${@host.name}`, and the `[local]` prefix of each output line). Plan variables and the secrets file apply as with [`--host`](/cli/#ad-hoc-host-with-a-plan). `--local` cannot be combined with `--host`, `--inventory`, `--target` or `--user`.

In a Dockerfile:

```dockerfile
FROM ubuntu:24.04
COPY glidesh /usr/local/bin/glidesh
COPY plan/ /srv/plan/
RUN cd /srv/plan && glidesh run --local -p plan.kdl
```

## In an Inventory

`local=#true` makes an inventory host a local one. It takes only a name: no address, `user`, `port` or `jump`, each of which is an error, and a group's or the inventory's `jump` does not apply to it. It can have `vars`, `plan=` and `run-as`, and run beside SSH hosts.

```kdl
host "builder" local=#true plan="image.kdl"

group "web" {
    host "web-1" "10.0.0.1" user="deploy"
}
```

A run whose hosts are all local needs no SSH key or agent. With SSH hosts beside them, the key is resolved as usual.

## How It Differs from an SSH Host

- **Commands** run in `sh -c` as the user running glidesh, starting in their home directory, as an SSH command starts in the login user's. Over SSH the command runs in the login shell instead, which is `sh` compatible on almost every host. Like a command over SSH, it has no terminal: one that would ask on it, such as a `sudo` outside `run-as`, fails at once instead of waiting for an answer.
- **Environment**: commands inherit glidesh's environment, without the `GLIDESH_*` variables (which hold passphrases) and `SSH_AUTH_SOCK`, which a command run over SSH never sees either.
- **Files** are read and written directly instead of over SFTP.
- **Built-in variables**: `${@host.address}` is `localhost`, `${@host.user}` the user running glidesh, `${@host.port}` `0`. `${@os.*}` and `${@fact.*}` are detected as for any host.
- **Privilege escalation**: [`run-as`](/advanced/run-as/) works with `sudo` and `doas`. `su` does not: it asks for its password on a terminal, which a local host does not give it.
- **The console** opens your own `$SHELL` for a local host; port forwarding needs SSH, so it is not available.

## Platforms

Every module runs POSIX shell commands on its host, so a local host needs a Linux or macOS controller. On Windows, a run with a local host fails before doing anything.
