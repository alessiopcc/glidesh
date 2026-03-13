# external-module

Use an external plugin module to set the system MOTD (Message of the Day) on target hosts.

## What It Does

1. Uses the `example/motd` external module to set `/etc/motd` to a custom message
2. Verifies the MOTD was written correctly with a shell command

This example demonstrates the external module plugin system: the `external` keyword in plans, the JSON-over-stdio protocol, and the SSH proxy for running commands on the target.

## Files

- `inventory.kdl` — inventory with a `web` group
- `plan.kdl` — plan using the `external` keyword to invoke the plugin
- `modules/glidesh-module-example-motd` — a Python plugin implementing the `example/motd` module

## Usage

```bash
glidesh run -i examples/external-module/inventory.kdl -p examples/external-module/plan.kdl
```

The plugin is auto-discovered from the `modules/` directory next to the plan file.

## How the Plugin Works

The `glidesh-module-example-motd` plugin follows the JSON-over-stdio protocol:

1. **Describe** — reports its name (`example/motd`) and protocol version
2. **Check** — downloads `/etc/motd` via the SSH proxy and compares it to the desired content
3. **Apply** — uploads the new content to `/etc/motd` via the SSH proxy

The plugin never has direct SSH access. All remote operations go through glidesh's SSH proxy.

## Requirements

The plugin requires Python 3 on the machine running glidesh (not on the target hosts).
