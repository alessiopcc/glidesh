---
title: Conditionals
description: Run a step or task only when a condition holds, with when=.
---

The `when` attribute runs a step or a task only if its condition holds. When it does not, the
step or task is **skipped**: nothing is checked or applied on the host, and the run reports it
as `skipped` along with the condition that caused it.

```kdl
plan "web" {
    step "Install nginx (Debian)" when="${@os.family} == debian" {
        package "nginx" state="present"
    }

    step "Install nginx (Red Hat)" when="${@os.family} == redhat" {
        package "nginx" state="present"
        shell "setsebool -P httpd_can_network_connect 1"
    }
}
```

Conditions are evaluated on the controller, from variables glidesh already has — no command
runs on the host to decide them. The [OS facts](/concepts/variables/#built-in-os-facts)
(`${@os.*}`) and [host facts](/concepts/variables/#host-facts) (`${@fact.*}`) are the most
common thing to branch on; any variable a module argument can use
works too. [`@inventory.*` references](/concepts/variables/#inventory-references) are the
exception: they exist only while a `file` template is rendered, so a condition that uses one is
rejected when the plan is parsed.

## On a step or on a task

`when` can go on a step, on a task, or both.

```kdl
step "Accounts" {
    user "deploy" state="present"
    user "backup" state="present" when="${enable-backups}"
}
```

- **On a step**, it is checked **once**, before anything in the step runs. If it does not
  hold, every task in the step is skipped.
- **On a task**, it is checked just before that task, and it applies to that task only.

The difference matters for [loops](/advanced/loops-register/). A step's condition is checked
before its `loop` is resolved, so it can guard a loop over a variable that might not exist —
but for the same reason it cannot see `${@item}`:

```kdl
step "Format extra disks" loop="${extra-disks}" when="defined ${extra-disks}" {
    disk "${@item}" fs="ext4"
}
```

A task's condition is checked on every iteration, after `${@item}` is set, so use it to filter
items:

```kdl
step "Format disks" loop="${disks}" {
    disk "${@item}" fs="ext4" when="${@item} != sda"
}
```

Referring to `${@item}` in a step's condition is rejected when the plan is parsed.

## Syntax

The grammar is deliberately small — enough to branch, not enough to program.

| Form | Holds when |
|---|---|
| `${a} == value` | `a` equals `value` |
| `${a} != value` | `a` does not equal `value` |
| `${a} == ${b}` | the two variables are equal |
| `${a}` | `a` is truthy (see below) |
| `defined ${a}` | `a` exists, whatever its value — including a [structured variable](/concepts/variables/#structured-variables) |
| `undefined ${a}` | `a` does not exist |
| `!term` | `term` does not hold |
| `x && y` | both hold |
| `x \|\| y` | either holds |

**Values** are a `${variable}`, a bare word such as `debian` or `22.04`, or a single-quoted
string for anything containing spaces or operators: `${motd} == 'managed by glidesh'`. Single
quotes avoid escaping double quotes inside the KDL attribute.

**Comparison is always between strings.** `${@os.version} == 22.04` compares text; there is no
`<` or `>`.

**Structured variables** — lists of maps, such as a `vms { - name="web" }` block — have no
single value, so comparing one or testing its truthiness is an error. `defined` and
`undefined` work on them, which is what guarding a `loop` over one needs.

**Truthiness.** A bare `${a}` is false only if `a` is exactly `""`, `false` or `0`. Anything
else — including `no`, `False` and `00` — is true.

**Precedence.** `&&` binds tighter than `||`, so `a || b && c` means `a || (b && c)`. There are
no parentheses; `!` applies to the single term after it.

**Short-circuiting.** Both `&&` and `||` stop as soon as the answer is known. This is what
makes a `defined` guard work:

```kdl
shell "systemctl restart ${service}" when="defined ${service} && ${service} != none"
```

### Undefined variables are an error

Reading a variable that does not exist — `${a} == x` or `${a}` with no `a` — **fails the task**,
just as referencing it in a module argument would. That is deliberate: a misspelled variable
name should stop the run, not quietly evaluate to false and skip. Guard optional variables with
`defined`, as above.

### Not a shell expression

`when` does not run commands. `when="test -f /etc/app.conf"` is rejected as a malformed
condition, not executed. To decide based on the state of the host, either use
[`check=`](#when-or-check) or run a command first and `register` its output:

```kdl
step "Look for a legacy config" {
    shell "test -f /etc/app/legacy.conf && echo yes || echo no" register="legacy"
}

step "Migrate it" when="${legacy} == yes" {
    shell "/opt/app/bin/migrate-config"
}
```

## What a skip does

A skipped step or task:

- runs nothing on the host — no `check`, no `apply`;
- is reported as `skipped`, with the condition as written in the plan;
- is counted separately from `changed` in the host and run summaries — a skipped step counts
  each of its tasks (its [`always`](/advanced/rescue/) tasks too, not its `rescue` tasks);
- **does not trigger [subscribers](/advanced/subscribe/)**: a skipped step made no change, so
  steps that `subscribe` to it do not run because of it;
- **leaves its `register` variable undefined** — not empty. If it held a value from earlier,
  that value is removed.

The last point is on purpose. An empty value inside a later command is silent and can be
dangerous — `rm -rf /data/${old-release}` would expand to `rm -rf /data/`. An undefined one
fails the task instead, and `defined ${var}` lets you test for it:

```kdl
step "Find the old release" when="${upgrade}" {
    shell "readlink /opt/app/previous" register="old-release"
}

step "Remove the old release" {
    shell "rm -rf /opt/app/releases/${old-release}" when="defined ${old-release}"
}
```

With `--no-tui`, the `web` plan from the top of this page, followed by the `Accounts` step
above, prints this on a Debian host with `enable-backups` set to `false`:

```
[web-1] Step 1/3: Install nginx (Debian)
[web-1]   Checking package 'nginx'
[web-1]   package 'nginx': changed
[web-1] Step 2/3: Install nginx (Red Hat)
[web-1]   skipped (when: ${@os.family} == redhat)
[web-1] Step 3/3: Accounts
[web-1]   Checking user 'deploy'
[web-1]   user 'deploy': ok
[web-1]   user 'backup': skipped (when: ${enable-backups})
[web-1] OK (1 changed, 3 skipped)
```

The Red Hat step counts as two skips, one for each of its tasks.

The reason is always the condition **as written**, never with its variables filled in, so a
condition that compares a [secret](/concepts/secrets/) never prints the secret's value.

## `when` or `check`?

Both can stop a task from doing anything, but they answer different questions:

| | `when=` | `check=` (on `shell`) |
|---|---|---|
| Question | Does this task apply to this host at all? | Is this task's work already done? |
| Evaluated | On the controller, from variables | On the host, by running a command |
| Stops the task when | the condition does not hold | the check command succeeds |
| Reported as | `skipped` | `ok` |
| Cost | Nothing | One command on the host |

Use `when` for decisions you can make from what you know — the OS, inventory variables, feature
flags, an earlier registered value. Use `check=` when only the host can tell you, such as
whether a file already exists. They combine: a task whose `when` holds still runs its `check=`.

Neither one fails the host. To stop a host whose state is wrong, use `check=` as an
[assertion](/modules/shell/#assertions-check-as-the-condition): the condition goes in `check=`
and the command is the failure.

## With `host` tasks

A [`host`](/modules/host/) task runs once and shares its result with every target. Its `when`
is evaluated per target: hosts where it does not hold skip the task and receive nothing; the
remaining hosts still share a single execution.

## Under `--dry-run`

A preview evaluates conditions exactly as a real run would — with one exception.

A value [registered](/advanced/loops-register/) earlier in the same preview is always empty,
because the task that would produce it did not run. A condition that needs that value cannot be
answered, so the preview skips the task and says why instead of guessing:

```
[web-1]   shell 'touch /root/probed': skipped (undetermined in preview: when: ${out} == yes depends on ${out}, which is not known until the real run)
```

The real run evaluates it normally.

Checking only whether a registered variable exists — `defined ${out}` — is usually answered
in a preview, since registering always defines the variable. The exception is a variable
registered by a task that was itself undetermined: the real run may or may not run that task,
so it may or may not define the variable, and a `defined` test on it is undetermined too.
The uncertainty carries forward rather than turning into a confident answer the real run
might contradict.

## Validation

Conditions are parsed with the plan, so `glidesh validate` reports a malformed one before
anything connects:

```
FAILED: Config parse error: invalid when="${@os.family} = debian": '=' is not an operator; use '=='
```

Two related mistakes are caught the same way:

- **An unknown step attribute is an error.** A misspelled `wehn=` on a step used to be ignored,
  which for a condition means a step that silently always runs.
- **`when` must be an attribute.** Written as a child node inside a task block, it would be
  passed to the module as an argument and ignored, so it is rejected.

:::caution
glidesh 1.2 and earlier do not know `when`. They ignore it on a step, and on a task they pass
it to the module as an ordinary argument, which every module except `container` ignores. Either
way the step or task **runs unconditionally**. Do not run a plan that uses `when` with an older
glidesh.
:::
