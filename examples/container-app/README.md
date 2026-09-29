# container-app

Deploy a containerized application with ports, environment variables, and volumes.

## What It Does

1. Deploys an nginx container with port mapping, environment variables, and a persistent volume
2. Gates the step on `ready-cmd`, so it only completes once the container answers

## Usage

```bash
glidesh run -i examples/container-app/inventory.kdl -p examples/container-app/plan.kdl
```

glidesh asks which nginx tag to deploy (Enter keeps `alpine`). To answer up front, as a
script or CI would:

```bash
glidesh run -i examples/container-app/inventory.kdl -p examples/container-app/plan.kdl --var app-tag=1.27-alpine
```

Customize the image and `app-port` in the plan for your application.
