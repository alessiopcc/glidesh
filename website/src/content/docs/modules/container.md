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

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Container name |
| `image` | string | Container image reference |
| `state` | string | `"running"`, `"stopped"`, or `"absent"` |
| `runtime` | string | `"docker"` or `"podman"` (default: auto-detect) |
| `install-runtime` | boolean | Auto-install the runtime if not found |
| `network` | string | Network mode: `"host"`, `"bridge"`, `"none"`, or a custom network name |
| `restart` | string | Restart policy: `"always"`, `"on-failure"`, `"no"` |
| `command` | string | Custom command to run in the container (overrides image default) |
| `ports` | list | Port mappings (`host:container`) |
| `environment` | map | Environment variables |
| `volumes` | list | Volume mounts (`host:container`) |

## Idempotency

The module checks if a container with the given name exists and is in the desired state. For `running`, it also verifies the image, network, and command match. If a container exists with a different image, network, or command, it is replaced.

## Example

See the [container-app example](/examples/#container-app) for a complete containerized deployment.
