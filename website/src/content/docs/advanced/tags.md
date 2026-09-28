---
title: Tags
description: Run part of a plan with --tags and --skip-tags.
---

Tags let one plan serve several jobs: push only the configuration, skip the slow steps, or
rerun just the deploy. Tag steps in the plan, then pick them on the command line.

```kdl
plan "web" {
    step "Detect version" tags="always" {
        shell "cat /opt/app/VERSION" register="version"
    }
    step "Install packages" tags="packages" {
        package "nginx" state="present"
    }
    step "Deploy config" tags="config" {
        file "/etc/nginx/nginx.conf" src="nginx.conf"
    }
    step "Restart nginx" subscribe="Deploy config" tags="config" {
        systemd "nginx" state="restarted"
    }
    step "Warm caches" tags="config,slow" {
        shell "/opt/app/bin/warm --version ${version}"
    }
}
```

```bash
glidesh run -i inventory.kdl -p plan.kdl --tags config                 # Detect, Deploy, Restart, Warm
glidesh run -i inventory.kdl -p plan.kdl --tags config --skip-tags slow   # Detect, Deploy, Restart
glidesh run -i inventory.kdl -p plan.kdl --skip-tags packages          # everything but Install
```

## Rules

- **`tags="a,b"`** goes on a step: comma-separated names without spaces. A step without
  `tags` has none.
- **`--tags a,b`** runs only the steps carrying at least one of the named tags. Without
  `--tags`, every step is selected.
- **`always`** is a tag like any other, except that `--tags` always selects it. Use it for
  steps whose `register` variables later steps need.
- **`--skip-tags x,y`** leaves out every step carrying any of the named tags. It wins over
  `--tags` and over `always`.
- **A tag no step carries is an error**, reported before connecting to any host. A typo in
  `--tags` would otherwise run nothing, and one in `--skip-tags` would run the very steps it
  meant to hold back. With an inventory whose groups run different plans, a tag counts as
  known if any of those plans uses it.
- Tags work with [included plans](/advanced/plan-includes/): an included step keeps its tags.
- Only steps are tagged. `tags=` on a built-in module's task is an error; move the task to a
  step of its own. An [external module](/modules/external/) may take its own `tags` parameter,
  which is passed to the plugin as usual.

## What a left-out step does

A step left out by tags behaves like a step whose [`when=`](/advanced/conditionals/) does not
hold:

- It is reported as `skipped`, with the reason — `skipped (--skip-tags slow)` or
  `skipped (not in --tags config)` — and counts toward the run's skipped total.
- It does not trigger its [subscribers](/advanced/subscribe/). A subscriber that is itself
  selected still runs, but is not forced to reapply.
- Its `register` variables stay **undefined**. A later step that uses one fails, unless its
  `when=` checks `defined ${var}` first. Tag the step that registers the variable `always`,
  or give both steps the same tag.
- Its `when=` is not evaluated.

In sync mode, hosts still wait for each other at every step, selected or not. Tags combine
with [`--dry-run`](/cli/#previewing-a-run) and `--target`.
