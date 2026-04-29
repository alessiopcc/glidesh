# host-module

End-to-end demo of the `host` module — a forced sync barrier whose command
runs **once** and whose `register` value is broadcast to every target's var
map.

This example exercises the host-module's two execution paths:

| Step                                 | What it shows                                |
|--------------------------------------|----------------------------------------------|
| Generate shared deploy token         | Single `cmd=` string, runs on the controller |
| Pick a leader hostname               | `on="<inventory-host>"` remote execution     |
| Write the shared values to every host| Downstream `shell` reads the broadcast vars  |
| Verify each host received the …      | Confirms identical values across the fleet   |

## Running it

Dry-run (no SSH, prints what would happen):

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run
```

Against a real fleet (point `inventory.kdl` at hosts you control):

```bash
glidesh run -i inventory.kdl -p plan.kdl
```

Per-host run logs land under `~/.glidesh/runs/<timestamp>_host-module-demo/`.
Every host's log should contain the **same** `deploy_token` and `leader`
values — that's the broadcast guarantee.

## Why `host` instead of `shell`?

`shell "openssl rand -hex 16" register="t"` would generate a *different*
token on every host. `host` runs the command exactly once (on the
controller, or on the named inventory host with `on=`) and shares the
captured stdout with every target.

## Failure semantics

If the single execution fails, **every** host fails the step with the same
error message — `host` is an implicit sync barrier.
