# container-app

Deploy a containerized application with ports, environment variables, and volumes.

## What It Does

1. Deploys an nginx container with port mapping, environment variables, and a persistent volume
2. Runs a health check with retries to verify the container is responding

## Usage

```bash
glidesh run -i examples/container-app/inventory.kdl -p examples/container-app/plan.kdl
```

Customize `app-image` and `app-port` in the plan vars for your application.
