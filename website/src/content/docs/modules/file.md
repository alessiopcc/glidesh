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
- `${for h in @group.name}...${endfor}` — loop over hosts in an inventory group

See [Template Loops](/advanced/loops-register/#template-loops) for detailed examples.

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
file "backups/${host.name}-dump.sql" {
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

## Example

See the [web-server example](/examples/#web-server) for a template-based nginx deployment.
