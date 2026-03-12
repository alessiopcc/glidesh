# hello-echo

A minimal glidesh example that deploys an HTTP echo server to a group of nodes.

## Files

- `inventory.kdl` — inventory defining an `app` group with two hosts
- `plan.kdl` — three-step plan that:
  1. Runs a hello-world shell command
  2. Starts a containerized echo server (`hashicorp/http-echo`) on port 4242
  3. Verifies the server is responding with a curl health check (retries up to 5 times)

## Usage

```bash
glidesh run -i examples/hello-echo/inventory.kdl -p examples/hello-echo/plan.kdl
```

## Customization

Edit `inventory.kdl` to point at your own hosts. The `echo-port` variable in the plan controls which port the echo server listens on.
