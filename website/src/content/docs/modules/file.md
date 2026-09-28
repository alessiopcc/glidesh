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
- `${for h in @group.name}...${endfor}` — loop over hosts in an inventory group

See [Template Loops](/advanced/loops-register/#template-loops) for detailed examples.

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
file "/etc/myapp/" src="configs/" recurse=#true owner="deploy" mode="0644"
```

All files under the local `configs/` directory are uploaded to `/etc/myapp/`, preserving the directory structure. Remote directories are created automatically.

Recursive copy supports:
- **Idempotency** — each file is compared by SHA256 checksum; only changed files are uploaded
- **Template mode** — combine with `template=#true` to interpolate all files in the directory
- **Attributes** — `owner`, `group`, and `mode` are applied recursively to all files and directories

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
| `mode` | string | Remote file permissions (e.g., `"0644"`) |
| `diff` | boolean | `#false` keeps this task's content out of [`--diff`](#--diff) (default `#true`) |

## Path Resolution

The `src` path is resolved **relative to the plan file's directory**, not the current working directory. Absolute paths are used as-is.

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
