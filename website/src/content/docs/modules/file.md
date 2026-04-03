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
| `owner` | string | Remote file owner |
| `group` | string | Remote file group |
| `mode` | string | Remote file permissions (e.g., `"0644"`) |

## Idempotency

Copy and template modes compare SHA256 checksums between the local and remote files. If they match, the transfer is skipped. Fetch mode always downloads.

## Example

See the [web-server example](/examples/#web-server) for a template-based nginx deployment.
