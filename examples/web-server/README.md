# web-server

Deploy nginx with a templated configuration file.

## What It Does

1. Installs nginx via the system package manager
2. Deploys a templated `nginx.conf` with variable interpolation (`${server-name}`, `${doc-root}`)
3. Starts and enables the nginx service
4. Runs a health check with retries

## Usage

```bash
glidesh run -i examples/web-server/inventory.kdl -p examples/web-server/plan.kdl
```

## Files

- `inventory.kdl` — target hosts
- `plan.kdl` — deployment plan
- `files/nginx.conf` — nginx config template with `${var}` placeholders
