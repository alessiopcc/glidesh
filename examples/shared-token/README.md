# shared-token

Demonstrates the `host` module: run a command **once** on the controller and
broadcast its output to every target host's var map via `register`.

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run
```

The `host "random token" cmd="openssl rand -hex 16" register="deploy_token"`
line runs `openssl rand -hex 16` exactly once on the machine executing
glidesh. Every target host sees the same `${deploy_token}` value in
subsequent steps — unlike `shell`, which would generate a different token
per host.

Pointing at a specific inventory host instead of the controller:

```kdl
host "read leader" cmd="hostname" on="node-1" register="leader_name"
```

Still runs once; `leader_name` is the same string everywhere.
