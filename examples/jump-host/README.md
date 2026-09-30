# jump-host

Connect to internal hosts through an SSH bastion (jump host).

## Files

- `inventory.kdl` — inventory with one jump host for every host, a per-host override, and a
  host reached directly
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
- The top-level `jump` node applies to every host; a `jump` inside a `group` or `host` overrides
  it (the most specific wins), and `jump #false` reaches that group or host directly
- Omit `user` on the jump node to inherit the target host's SSH user
