# jump-host

Connect to internal hosts through an SSH bastion (jump host).

## Files

- `inventory.kdl` — inventory with a group-level jump host and a per-host override
- `plan.kdl` — simple connectivity check that verifies the tunnel works

## What It Does

1. Connects to each target through its configured bastion host
2. Runs `hostname` on the target to confirm end-to-end connectivity
3. Checks uptime on each internal machine

## Usage

```bash
glidesh run -i examples/jump-host/inventory.kdl -p examples/jump-host/plan.kdl
```

## Customization

- Edit `inventory.kdl` to point at your own bastion and internal hosts
- The `jump` node on the group applies to all hosts; override it per-host by adding a `jump` child node to any `host` entry
- Omit `user` on the jump node to inherit the target host's SSH user
