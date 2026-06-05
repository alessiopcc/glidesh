# run-as

Privilege escalation: run tasks as another user (typically `root`) via `sudo`,
`doas`, or `su`, while connecting over SSH as an unprivileged user.

## Files

- `inventory.kdl` — a global `run-as` default, a per-host method override, and a host that opts out
- `plan.kdl` — step-level and per-task escalation, including opting a single task out

## The model

`run-as` takes the **target user** as its value:

- `run-as="root"` — escalate to `root`
- `run-as="postgres"` — escalate to `postgres`
- `run-as=""` — explicitly **do not** escalate (cancels an inherited setting)
- *(attribute absent)* — inherit from the less-specific level

Precedence, most specific wins:

```
module/task  >  step  >  plan  >  host  >  group  >  global (inventory)  >  --run-as (CLI)
```

Set a plan-wide default on the `plan` node itself (`plan "name" run-as="root" { … }`).

The escalation method defaults to `sudo`; override with `run-as-method="doas"` or
`run-as-method="su"`.

## Usage

```bash
# Use the inventory's run-as settings
glidesh run -i examples/run-as/inventory.kdl -p examples/run-as/plan.kdl

# Or set a default from the CLI (lowest precedence; inventory/plan still override)
glidesh run -i examples/run-as/inventory.kdl -p examples/run-as/plan.kdl \
  --run-as root --run-as-method sudo
```

## Passwords

`sudo` is the robust path. If the target requires a sudo password (no `NOPASSWD`
entry), supply it without echoing to the terminal:

```bash
glidesh run ... --run-as root --ask-pass        # prompt
GLIDESH_RUNAS_PASS='…' glidesh run ... --run-as root   # from the environment
```

`doas` is passwordless-only (configure `nopass`/`persist` in `doas.conf`), and `su`
requires a PTY. Prefer `sudo` unless you have a specific reason not to.
