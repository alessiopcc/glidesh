---
title: file
description: Transfer files, apply templates, and fetch remote files.
---

The `file` module handles file operations between the local machine and remote hosts via SFTP. It supports three modes: copy, template, and fetch.

## Copy

Upload a local file to the remote host:

```kdl
file "/etc/nginx/nginx.conf" {
    src "files/nginx.conf"
    owner "root"
    group "root"
    mode "0644"
}
```

## Template

Interpolate `${var}` placeholders and expand `${for}` loops before uploading:

```kdl
file "/etc/myapp/config.toml" {
    src "templates/config.toml"
    template #true
    owner "appuser"
    mode "0600"
}
```

Template mode supports:
- `${var-name}` — simple variable interpolation
- `${for item in collection}...${endfor}` — loop over [structured variables](/concepts/variables/#structured-variables)
- `${@inventory.host.address}` — [inventory references](/concepts/variables/#inventory-references)
- `${@os.family}` and the other [OS facts](/concepts/variables/#built-in-os-facts) — detected per host
- `${@fact.cpu.count}`, `${@fact.mem.total-mb}` and the other [host facts](/concepts/variables/#host-facts)
- `${for h in @group.name}...${endfor}` — loop over hosts in an inventory group
- `$${…}` — a literal `${…}`, for the shell or any other program that reads it

See [Template Loops](/advanced/loops-register/#template-loops) for detailed examples.

### Literal `${…}` in a template

Every `${…}` in a templated file is a glidesh reference, comments included. A script, unit
file or Dockerfile that needs its own `${VAR}` writes it `$${VAR}`:

```sh
#!/bin/sh
# deploys ${app} into $${HOME}
cd "$${HOME}/${app}" && exec ./${app} "$${1:-serve}"
```

With `app` set to `api`, the host gets `cd "${HOME}/api" && exec ./api "${1:-serve}"`. See
[Interpolation](/concepts/variables/#interpolation) for the rule, and how it changed.

A name nothing defines fails the upload, naming the template, the line and the name:
`template files/run.sh, line 3: undefined variable HOME (if it is meant for the shell,
write $${HOME} to keep ${HOME} as is)`. [`glidesh validate`](/cli/#glidesh-validate) finds
it before any run: it reads each local templated source and reports every `${name}` that
is not a plan, secret or prompted variable, an inventory variable of every host that runs
the plan, a name an earlier task registers, a built-in, or the binding of a `${for}` around
it — and every `${for}` over a list nothing defines. Without `-i`, a name the inventory might
set only warns ([details](/cli/#glidesh-validate)). A `src` that itself
contains `${…}` is only known at run time, so it is not read.

### Forgetting `template`

Without `template #true`, a file is uploaded byte for byte and every `${…}` in it ships
literally. That is right for a shell script's `${HOME}`, but rarely for a glidesh variable —
`CUDA_VISIBLE_DEVICES=${cuda-devices}` reaching the host unexpanded is almost certainly a
missing `template #true`.

So when an untemplated upload contains `${name}` and `name` is a variable glidesh defines for
that host, the task carries a warning — on `--dry-run` too:

```
warning: files/vllm.env contains ${cuda-devices} but is uploaded as-is, because `template` is not set; add `template #true` to substitute
```

Only names glidesh defines count, so shell and environment variables do not trigger it. The
file is still uploaded unchanged: it is a warning, not an error, because the content may be
intentional. [`glidesh validate`](/cli/#glidesh-validate) reports the same thing before any
run, for directory uploads too.

## Recursive Directory Copy

Upload an entire directory tree to the remote host:

```kdl
file "/etc/myapp/" src="configs/" recurse=#true owner="deploy" file-mode="0644"
```

All files under the local `configs/` directory are uploaded to `/etc/myapp/`, preserving the directory structure. Remote directories are created automatically, empty ones included.

Recursive copy supports:
- **Idempotency** — each file is compared by SHA256 checksum; only changed files are uploaded
- **Template mode** — combine with `template=#true` to interpolate all files in the directory
- **Attributes** — `owner`, `group`, and `mode` apply to the paths the source has: the
  destination directory, the directories under it, and the files. A host file or directory
  the source does not have, or that `exclude` leaves out, keeps its own. The check compares
  each of them, directories included, so a second run is `ok`. A path that is a symlink on
  the host is never followed: `owner`/`group` change the link itself (and the check reads
  the link's), and it gets no mode. A copy to `/` — however it is named, `/tmp/..` or a
  symlink to it too — is refused with any of them, before anything is uploaded. Without
  them, a copy to `/` works.
- **The other kind in the way** — a file on the host where the source has a directory, or a
  directory where it has a file, keeps the task pending, and without `prune` fails it,
  naming the path, before anything changes. With `prune` it is removed and replaced. A
  destination that is itself a file, or a link to nothing, is refused either way. A host
  name shown in the output has its control characters escaped (`\n`, `\u{1b}`).
- **Per-kind modes** — `dir-mode` and `file-mode` set directories and files apart;
  `mode` sets whichever kind has no mode of its own:

  ```kdl
  file "/opt/app/" src="app/" recurse=#true owner="app" dir-mode="0755" file-mode="0644"
  file "/opt/tools/" src="tools/" recurse=#true mode="0755" file-mode="0644"
  ```

### Excluding paths

`exclude` leaves paths of the source out: they are neither uploaded nor, with `prune`,
removed from the host.

```kdl
file "/srv/site/" src="site/" recurse=#true {
    exclude {
        - ".git"        // any .git, file or directory, at any depth
        - "*.log"       // every .log file
        - "build/**"    // everything under the top-level build/
    }
}
```

Patterns work like `.gitignore`'s: one without `/` matches a name at any depth, one with `/`
(or a leading `/`) is matched against the path from the source directory. `*` and `?` match
within a name, `**` any number of directories. Excluding a directory excludes everything
under it. Unlike `.gitignore`, there is no `!` (a pattern starting with it is refused), no
`[…]` class and no `\` escape, and a trailing `/` matches a file of that name too.

Put each pattern on its own line, or separate them with `;` (`- ".git"; - "*.log"`): a `-`
line holding several values is an error, rather than keeping the first alone.

### Removing what the source lacks (`prune`)

Without `prune`, a recursive copy only adds and updates: a file on the host that is not in
the local directory stays, even one you deleted or renamed locally. `prune=#true` removes
host files and directories under the destination that the source does not have (less what
`exclude` leaves out), so the tree on the host is the one in the plan:

```kdl
file "/srv/site/" src="site/" recurse=#true prune=#true {
    exclude {
        - "uploads"     // written by the application: never removed
    }
}
```

The check reports what would go — the task stays pending, its reason giving the count and
the first paths, and [`--diff`](#--diff) listing them, as `remove <path>` lines ahead of the
content diffs, up to the diff's 500-line limit — so `--dry-run` shows a removal before it
happens. Every template is rendered before anything is removed, so a template error fails
the task with the host unchanged. A symlinked directory on the way to the destination is
allowed, but its real path must be two directories deep too. A
directory holding an excluded entry stays, with that entry. An entry of another kind than
the source's at the same path — a file (or a link to one) where the source has a directory,
a directory (or a link to one) where it has a file — is removed too, and the source's takes
its place. A link to a directory where the source has a directory stays: uploads go through
it, and `prune` does not look inside it.

`prune` deletes, so it is refused, before anything changes, for a destination that is not
an absolute path at least two directories deep (`/srv/site`, not `/srv`), that goes through
`.` or `..`, that resolves to `/`, or that is a symlink — and for a source with nothing to
upload (empty, or all of it excluded), which would empty the destination. It lists the tree
without following symlinks: a link under the destination is removed as a link, never what
it points to. A host name it cannot be sure it read exactly stops it, since it could not be
compared: one that is not UTF-8, or holds `U+FFFD` (which stands for bytes that were not
UTF-8) or a carriage return (which `run-as-method="su"` adds to a line break). For the same
reason a source name holding either is refused with `prune`, before anything changes. With `run-as`, it removes
only from directories no one else can write to, as uploads write only there.

:::note
`fetch=#true` and `recurse=#true` cannot be combined.
:::

## Fetch

Download a remote file to the local machine:

```kdl
file "backups/${@host.name}-dump.sql" {
    src "/var/backups/db.sql"
    fetch #true
}
```

A relative destination resolves from the plan file's directory — the same place an upload's
`src` resolves from, so a plan that fetches a file and a plan that uploads it agree on where
it is, from whatever directory you run glidesh. In an [included](/advanced/plan-includes/)
plan, that is the included plan's own directory. An absolute path is used as given. The
task's output names the absolute path written:

```
fetch /var/backups/db.sql -> /home/me/project/plans/backups/web-1-dump.sql (48213 bytes)
```

:::caution[Earlier versions]
Earlier versions resolved a relative fetch destination from the directory glidesh ran in.
A plan run from its own directory writes where it did; one run from elsewhere now writes
beside the plan file.
:::

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Destination path (remote for copy/template, local for fetch) |
| `src` | string | Source file path (required) |
| `template` | boolean | Interpolate `${var}` placeholders and expand `${for}` loops before uploading |
| `fetch` | boolean | Download from remote instead of uploading |
| `recurse` | boolean | Recursively copy a directory tree |
| `owner` | string | Remote file owner |
| `group` | string | Remote file group |
| `mode` | string | Remote file permissions (e.g., `"0644"`); with `recurse`, of the directories and files that have no mode of their own |
| `dir-mode` | string | With `recurse`: the mode of the directories |
| `file-mode` | string | With `recurse`: the mode of the files |
| `exclude` | list | With `recurse`: [paths of the source to leave out](#excluding-paths) |
| `prune` | boolean | With `recurse`: [remove what the source lacks](#removing-what-the-source-lacks-prune) from the destination (default `#false`) |
| `diff` | boolean | `#false` keeps this task's content out of [`--diff`](#--diff) (default `#true`) |

## Owner and Mode

`owner`, `group`, and `mode` always win. Without them, an upload follows the same rules
with or without [`run-as`](/advanced/run-as/):

- **A file that already exists** is rewritten in place: it keeps its owner, group, mode
  and ACL, and every hard link to it sees the new content. Re-uploading a `0755` script
  keeps it executable. One exception comes from the kernel: setuid/setgid bits are
  cleared by a write from a user other than root, and by any `owner` or `group` change —
  set `mode` as well to keep them.
- **A new file** is created by the user writing it (the login user, or the `run-as`
  user), so the usual rules apply: that user's group — the directory's group when the
  directory is setgid — and `0666` minus the umask in effect: `0644` with the usual
  `022`.
- **A symlink** is written through: the file it points to gets the new content and
  keeps its attributes; the link stays a link. `owner`, `group`, and `mode` apply to
  that file too. With `run-as`, see
  [Destinations other users control](/advanced/run-as/#destinations-other-users-control).
- **A new directory** of a recursive copy gets `0777` minus the umask: `0755` usually.

Where the directory has a default ACL, a new file or directory inherits it instead, and
the umask does not apply. The write is not atomic: a program reading the file while it is
uploaded can see it partly written.

## Path Resolution

The `src` path of an upload, and the destination of a [fetch](#fetch), are resolved **relative to the plan file's directory**, not the current working directory. Absolute paths are used as-is.

Given this layout:

```
project/
├── inventory.kdl
├── plans/
│   ├── web.kdl          ← plan file
│   └── files/
│       └── nginx.conf
```

A step in `plans/web.kdl` references the file relative to its own directory:

```kdl
file "/etc/nginx/nginx.conf" src="files/nginx.conf"
```

This resolves to `plans/files/nginx.conf` regardless of where you run glidesh from.

## Idempotency

Copy and template modes compare SHA256 checksums between the local and remote files. If they match, the transfer is skipped. When `owner`, `group`, or `mode` are specified, the module also checks the remote file's attributes — if only permissions differ, the attributes are corrected without re-uploading the file. This applies to both single-file and recursive directory copies. Fetch mode always downloads.

Every module's rules side by side: [Idempotency & Drift](/concepts/idempotency/).

## `--diff`

With [`--diff`](/cli/#previewing-a-run), a copy or template whose content differs shows a
unified diff from the file on the host to the one the plan wants. A destination that does
not exist yet is diffed against nothing, so every line shows as added. A recursive copy
shows one diff per changed file, up to 500 lines in all. Like any task output, a diff is
[capped at 8 KiB](/concepts/logs/) in the console, the TUI and the run log.

```
--- /etc/app.conf (host)
+++ /etc/app.conf (plan)
@@ -1,2 +1,2 @@
 name=app
-port=80
+port=8080
```

The destination is downloaded only once the checksums differ, so `--diff` costs nothing on a
file that is already in place. Instead of a diff, a one-line note says why none is shown for:

- **Binary content** — anything that is not UTF-8 text, or contains a NUL byte.
- **Files over 256 KiB**, on either side. The size is checked before downloading.
- **Content holding a [secret](/concepts/secrets/#redaction)**: `diff hidden (content contains a secret)`.
  Redaction would not be enough here — the old side can hold the value a rotated secret
  replaced, and a diff's `+`/`-` prefixes split a multi-line secret so it no longer matches.
- **A file other users cannot read**, on the host or under the plan's `mode` (`600`, `640`,
  or any symbolic mode such as `u=rw,go=`, which glidesh does not evaluate):
  `diff hidden (not readable by other users)`. Matching secrets cannot catch a value the plan
  no longer uses — it is not registered, yet the host's copy still holds it — so a file kept
  private is treated as sensitive whatever it contains.
- **With `run-as`, a destination another user could redirect**
  ([the rule](/advanced/run-as/#destinations-other-users-control)): they could swap the
  checked file for a link to a private one before it is read. `diff not shown (could not
  read it: …)`, naming the entry; the upload itself is refused for the same reason.
- **A task written with `diff=#false`**: `diff off for this task (diff=#false)`.

That leaves one case glidesh cannot catch: a world-readable file whose host copy still holds
a secret the plan no longer uses. glidesh keeps no state between runs, so it cannot know that
text was ever a secret. Anyone on the host can already read such a file, but a diff would
copy the value into the run log and CI output. Mark tasks like that with `diff=#false`:

```kdl
file "/etc/app/legacy.conf" src="legacy.conf" diff=#false
```

A change to `owner`, `group`, or `mode` alone has no diff: the pending line already names
each attribute, as `Fix attrs on /etc/app.conf: mode: 644 -> 600`. Fetch mode has none either.

## Example

See the [web-server example](/examples/#web-server) for a template-based nginx deployment.
