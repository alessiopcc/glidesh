---
title: container
description: Manage containers with Docker or Podman.
---

The `container` module manages containers using Docker or Podman. The runtime is auto-detected, or can be specified explicitly.

## Usage

```kdl
container "myapp" {
    image "registry.example.com/myapp:latest"
    state "running"
    runtime "podman"
    install-runtime #true
    restart "always"
    network "host"
    command "nginx -g 'daemon off;'"
    ports {
        - "8080:80"
        - "8443:443"
    }
    environment {
        DATABASE_URL "postgres://db:5432/app"
        LOG_LEVEL "info"
    }
    volumes {
        - "/data/myapp:/app/data"
    }
}
```

Unknown parameters are rejected at check time, so a typo (`privledged`) fails loudly instead of being silently dropped. The same applies to values glidesh cannot act on: an unsupported `runtime`, or `wait "healthy"` on a container that has no healthcheck.

## States

| State | Meaning |
|-------|---------|
| `"running"` *(default)* | A long-lived container is up and matches the plan |
| `"stopped"` | The container exists but is not running |
| `"absent"` | No container with this name exists |
| `"run-once"` | Run the container in the foreground to completion — a job, not a service |

## Parameters

### Core

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Container name |
| `image` | string | Container image reference |
| `state` | string | See [States](#states) |
| `runtime` | string | `"docker"` or `"podman"` (default: auto-detect). Any other value is rejected |
| `install-runtime` | boolean | Auto-install the runtime if not found. Installing happens during apply, never during check |
| `command` | string | Command to run in the container. Appended verbatim, so its own quoting is preserved |
| `entrypoint` | string | Override the image entrypoint |
| `pull` | string | `"always"`, `"missing"`, or `"never"` |
| `restart` | string | Restart policy: `"always"`, `"on-failure"`, `"no"` |
| `ports` | list | Port mappings (`host:container`) |
| `environment` | map | Environment variables |
| `volumes` | list | Volume mounts (`host:container`) |
| `labels` | map | Container labels |
| `network` | string | `"host"`, `"bridge"`, `"none"`, `"container:<name>"`, or a custom network name (auto-created if it doesn't exist) |
| `network-alias` | list | Extra DNS names on the attached network |
| `dns` | list | DNS servers |
| `add-host` | list | Extra `/etc/hosts` entries (`name:ip`) |
| `hostname` | string | Container hostname |
| `user` | string | User (or `uid:gid`) to run as |
| `workdir` | string | Working directory inside the container |
| `stop-signal` | string | Signal sent on stop |
| `init` | boolean | Run an init process as PID 1 |
| `read-only` | boolean | Mount the root filesystem read-only |
| `tmpfs` | list | tmpfs mount paths |

### Namespaces, privileges, and devices

| Parameter | Type | Description |
|-----------|------|-------------|
| `ipc` | string | IPC namespace: `"host"`, `"private"`, `"shareable"`, `"container:<name>"` |
| `pid` | string | PID namespace |
| `uts` | string | UTS namespace |
| `cgroupns` | string | cgroup namespace |
| `userns` | string | User namespace |
| `privileged` | boolean | Run with full host privileges |
| `cap-add` | list | Linux capabilities to add |
| `cap-drop` | list | Linux capabilities to drop |
| `security-opt` | list | Security options (e.g. `"seccomp=unconfined"`) |
| `devices` | list | Host devices to expose (`host:container[:perms]`) |
| `gpus` | string | GPU access. Emits `--gpus` on Docker; on Podman it becomes a CDI device (`all` → `nvidia.com/gpu=all`) |
| `shm-size` | string | Size of `/dev/shm` (e.g. `"16g"`) |
| `memory` | string | Memory limit |
| `cpus` | string | CPU limit |
| `ulimits` | map | ulimits, as `name` → `soft[:hard]` |
| `sysctls` | map | Kernel parameters |
| `extra-args` | list | Raw flags passed to `run` verbatim, unquoted. The escape hatch for anything without a first-class parameter |

Shared-memory workloads (CUDA IPC in particular) need the host IPC namespace — without it, a client mapping another process's GPU buffers fails with `cudaErrorMapBufferObjectFailed`:

```kdl
container "vllm" {
    image "vllm/vllm-openai:latest"
    ipc "host"
    privileged #true
    gpus "all"
    shm-size "16g"
    ulimits {
        memlock "-1"
        stack "67108864"
    }
}
```

### Readiness

`subscribe` orders steps; it does not wait for the service a step started to become usable. These parameters make a container task block until it is actually ready, so the next step can rely on it.

| Parameter | Type | Description |
|-----------|------|-------------|
| `healthcheck` | block | Container healthcheck: `cmd`, `interval`, `timeout`, `retries`, `start-period`. Bare numbers are seconds. `cmd "NONE"` disables the image's healthcheck |
| `wait` | string | `"healthy"` (runtime health status), `"running"`, or `"none"` (default). `"healthy"` requires a healthcheck — from a `healthcheck` block or baked into the image — and errors without one |
| `wait-timeout` | integer | Seconds to wait before failing (default: `300`) |
| `wait-interval` | integer | Seconds between probes (default: `3`) |
| `ready-cmd` | string | Probe run **on the target host**, not inside the container. Ready on exit code 0. Implies `wait "running"` |

```kdl
step "Serve the model" {
    container "vllm" {
        image "vllm/vllm-openai:latest"
        ipc "host"
        gpus "all"
        ports { - "8000:8000" }

        healthcheck {
            cmd "curl -sf http://localhost:8000/health"
            interval 10
            start-period 60
            retries 3
        }
        wait "healthy"
        wait-timeout 900
    }
}

step "Warm the cache" subscribe="Serve the model" {
    shell "curl -sf http://localhost:8000/v1/models"
}
```

Use `ready-cmd` when the image has no shell or no HTTP client to run a healthcheck with:

```kdl
container "lmcache" {
    image "lmcache/vllm-openai:latest"
    ipc "host"
    ready-cmd "curl -sf http://localhost:8000/health"
    wait-timeout 900
}
```

When readiness is not reached in time, the task fails with the container's last 30 log lines attached.

A readiness condition that can never hold is treated as a plan error rather than a wait: `wait "healthy"` on a container with no healthcheck fails immediately, at check time, instead of polling until the timeout and reporting success under `--dry-run`.

### One-shot jobs

`state "run-once"` runs the container in the foreground and waits for it to exit, instead of detaching. This covers work that used to need a raw `shell` invocation of `docker run` — pulling model weights, running a migration, seeding a volume.

| Parameter | Type | Description |
|-----------|------|-------------|
| `check` | string | Guard command. If it exits 0 the job is already done and is skipped |
| `remove` | boolean | Add `--rm` (default: `#true`) |
| `timeout` | integer | Seconds before the run is abandoned |
| `retries` | integer | Attempts before failing (default: `1`) |
| `delay` | integer | Seconds between attempts |
| `success_codes` | string, integer, or list | Exit codes treated as success (default: `0`) |

```kdl
step "Fetch model weights" {
    container "hf-download" {
        state "run-once"
        image "python:3.12-slim"
        entrypoint "/bin/bash"
        command #"-c "pip install -q huggingface_hub && hf download zai-org/GLM-4.6 --local-dir /models/GLM-4.6""#
        volumes { - "/srv/models:/models" }
        environment {
            HF_TOKEN "${hf-token}"
        }
        check "test -d /srv/models/GLM-4.6"
        timeout 7200
        retries 3
        delay 30
    }
}
```

`restart` is rejected with `state "run-once"` — a job that restarts is not a job.

## Idempotency

Every parameter that reaches the runtime is folded into a hash stored on the container as the `sh.glide.param-hash` label. On the next run:

- **Container missing** → create and start it.
- **Hash differs** → stop, remove, and recreate with the new spec.
- **Hash matches, container stopped or paused** → `start` / `unpause` it, keeping the existing container.
- **Hash matches, container running** → nothing to do (then the readiness gate, if any, is evaluated).

The hash is computed from the generated `run` arguments rather than a hand-maintained list, so any parameter you set affects drift detection.

Removal is verified: if the existing container cannot be removed, the task fails with that reason rather than letting the follow-up `run` fail with the runtime's opaque "name is already in use".

Readiness parameters (`wait`, `wait-timeout`, `wait-interval`, `ready-cmd`) are glidesh-side and deliberately excluded from the hash — changing a probe must not recreate a healthy container.

## Custom Networks

When `network` is set to a name other than `host`, `bridge`, `none`, `default`, or a `container:`/`ns:` reference, the module automatically creates the network if it doesn't already exist. This lets containers on the same custom network communicate by container name.

```kdl
container "redis" {
    image "redis:7"
    network "app-net"
}

container "webapp" {
    image "myapp:latest"
    network "app-net"
    environment {
        REDIS_URL "redis://redis:6379"
    }
}
```

## Example

See the [container-app example](/examples/#container-app) for a basic containerized deployment, and the [gpu-inference example](/examples/#gpu-inference) for GPU flags, one-shot jobs, and readiness gating.
