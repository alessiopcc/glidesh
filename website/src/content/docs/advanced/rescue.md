---
title: Rescue & Always
description: Handle a step's failure with rescue tasks, and run cleanup whether it failed or not.
---

A failing step stops its host: nothing after it runs there. `rescue` and `always` change what
happens around that failure. They are blocks of tasks written inside the step, beside its own
tasks:

```kdl
plan "deploy" {
    step "Deploy" {
        shell "touch /tmp/deploy.lock"
        shell "/opt/app/deploy.sh"
        rescue {
            shell "/opt/app/rollback.sh"
            file "/var/log/deploy-failure.txt" src="templates/failure.txt" template=#true
        }
        always {
            shell "rm -f /tmp/deploy.lock"
        }
    }
    step "Migrate" {
        shell "/opt/app/migrate.sh"
    }
}
```

- **`rescue`** runs only if the step failed. If every rescue task succeeds, the failure is
  handled: the host goes on to the next step — here `Migrate` — and ends the run as a
  success.
- **`always`** runs after the step and any rescue, whether or not they failed — the place for
  cleanup such as releasing a lock.

A step may have either, both or neither, each at most once. Each needs at least one task,
and takes no attributes: put `when=` on its tasks instead.

## What counts as a failure

`rescue` covers the step's work once it has started:

- a task that fails — a command that exits non-zero, a module error, an undefined variable,
  or [`register=`](/advanced/loops-register/#register) refusing output that is too long;
- its [`until=`](/advanced/until/) gate timing out;
- its [`loop=`](/advanced/loops-register/#step-loops) variable being undefined.

An error in the step's own [`when=`](/advanced/conditionals/) is not rescued: it is decided
before the step starts, and fails the host as before.

At the failure, the step's tasks stop. The ones after it do not run, and neither do the
remaining items of a loop.

## Reading the failure

The `rescue` and `always` tasks can read two variables that describe it:

| Variable | Holds |
|---|---|
| `${@error.msg}` | The error, as the run reported it |
| `${@error.task}` | The task that failed, as `module 'resource'` — for example `shell 'deploy.sh'`. Empty when the step failed outside its tasks, in its `until=` gate or its loop |

They are defined only while a failed step's `rescue` and `always` tasks run, so in `always`
`when="defined ${@error.msg}"` tells whether the step failed:

```kdl
always {
    shell "rm -f /tmp/deploy.lock"
    shell "curl -s -X POST https://hooks.example.com/deploy-failed" when="defined ${@error.msg}"
}
```

The step's own tasks, `when=`, `until=` and `loop=` cannot use them — there is no failure yet
— and a plan that tries is rejected when it is parsed. A `file` template is read only when its
task runs, so a run fails that task instead; [`glidesh validate`](/cli/#glidesh-validate)
checks the templates too, and reports it before any host is touched.

[Secrets](/concepts/secrets/) in the failure are redacted before a rescue sees it: a decrypted
value in the failed command line or its output reads `***`, as it does in every log.

### Using them safely

**Never write `${@error.msg}` or `${@error.task}` into a shell command.** The message holds
the failed command's output, and the task its interpolated command line: text a host or a
variable controls. Inside a command it is run as shell — no quoting or heredoc is safe
against every value it can hold.

Render them into a file with a [`file`](/modules/file/) template instead, and let a fixed
command read that file. A template writes each value as text: it is never run, nor expanded
again. The first example above does this, with this `templates/failure.txt` beside the plan:

```text
${@error.task} failed:
${@error.msg}
```

A later task can then pass it on without the text ever reaching a command line:

```kdl
plan "deploy" {
    step "Deploy" {
        shell "/opt/app/deploy.sh"
        rescue {
            file "/var/log/deploy-failure.txt" src="templates/failure.txt" template=#true
            shell "logger -t deploy -f /var/log/deploy-failure.txt"
        }
    }
}
```

`when=` is safe too: a condition compares values, and runs nothing.

## Undo, then still fail

A rescue that succeeds lets the host go on as if the step had worked. When the step's change
must not count as done — a config the service rejected, say — end the rescue with a task that
fails. The host stops as before, but only after putting things back:

```kdl
plan "nginx" {
    step "Deploy config" {
        shell "cp -p /etc/nginx/nginx.conf /etc/nginx/nginx.conf.previous" changed-when=#false
        file "/etc/nginx/nginx.conf" src="files/nginx.conf" template=#true
        shell "nginx -t" changed-when=#false
        rescue {
            shell "mv /etc/nginx/nginx.conf.previous /etc/nginx/nginx.conf"
            shell "echo 'nginx -t rejected the config; the previous one is back' >&2; exit 1"
        }
        always {
            shell "rm -f /etc/nginx/nginx.conf.previous" changed-when=#false
        }
    }
    step "Reload nginx" subscribe="Deploy config" {
        systemd "nginx" state="restarted"
    }
}
```

## Rules

- **A failed rescue fails the host**, as the step would have: `always` still runs, then the
  host stops. The same holds for a failed `always`, even when the step itself succeeded.
- **Once per step.** `rescue` and `always` run after the step's loop, not per item, and do
  not see `${@item}`: a plan whose `rescue` or `always` uses it is rejected when it is parsed,
  and `glidesh validate` reports a `file` template in them that does.
- **`register=`** works in both blocks. A rescue that does not run leaves its `register=`
  variables **undefined**, as a [skipped task](/advanced/conditionals/#what-a-skip-does)
  does — so a later `when="defined ${var}"` asks whether the rescue ran:

  ```kdl
  plan "deploy" {
      step "Deploy" {
          shell "/opt/app/deploy.sh"
          rescue {
              shell "/opt/app/rollback.sh" register="rolled-back"
          }
      }
      step "Report the rollback" when="defined ${rolled-back}" {
          shell "logger -t deploy 'rolled back'"
      }
  }
  ```

- **Changes count.** Tasks in `rescue` and `always` count toward the run's changed total like
  any other, and a step whose rescue or always changed something has changed: steps that
  [subscribe](/advanced/subscribe/) to it are triggered.
- **A skipped step skips all three.** When [`when=`](/advanced/conditionals/) or
  [tags](/advanced/tags/) leave a step out, its rescue and always blocks do not run either.
  The skipped count includes the step's own and `always` tasks — the ones a run that
  succeeds would have run.
- The run shows each block as it starts — `RESCUE step 'Deploy'`, `ALWAYS step 'Deploy'` — in
  the plain output, the TUI and the [run log](/concepts/logs/). The failure it handles is
  still shown and logged above it, but the host's summary in `glidesh logs` does not report a
  failure the rescue handled.
- A [`host`](/modules/host/) task in `rescue` runs once for all the hosts that reach it, as it
  does anywhere else.

## Under `--dry-run`

A preview runs the blocks the same way: `rescue` if computing the step's preview failed,
`always` in any case, each task reporting what it would do. A preview cannot know whether the
real run will fail, so a rescue's `register=` variables are neither defined nor undefined
there, and a [`when=`](/advanced/conditionals/#under---dry-run) that tests one is reported as
undetermined. For the same reason, when a step's preview did not fail, an `always` task's
`when="defined ${@error.msg}"` is undetermined too.
