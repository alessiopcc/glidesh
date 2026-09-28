# web-server

Deploy nginx with a templated configuration file.

## What It Does

1. Installs nginx via the system package manager
2. Deploys a templated `nginx.conf` with variable interpolation (`${server-name}`, `${doc-root}`)
3. Starts and enables the nginx service
4. Restarts nginx only when its configuration changed (`subscribe`)
5. Runs a health check with retries

## Usage

```bash
glidesh run -i examples/web-server/inventory.kdl -p examples/web-server/plan.kdl
```

### Run part of the plan

Each step is tagged (`packages`, `config`, `service`, `check`), so a run can pick steps:

```bash
# Push a config change: deploy it, restart nginx if it changed, run the health check
glidesh run -i examples/web-server/inventory.kdl -p examples/web-server/plan.kdl --tags config

# Everything except package installation
glidesh run -i examples/web-server/inventory.kdl -p examples/web-server/plan.kdl --skip-tags packages
```

See [Tags](https://glidesh.netlify.app/advanced/tags/).

## Files

- `inventory.kdl` — target hosts
- `plan.kdl` — deployment plan
- `files/nginx.conf` — nginx config template with `${var}` placeholders
