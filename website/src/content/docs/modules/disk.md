---
title: disk
description: Format and mount block devices with fstab management.
---

The `disk` module manages block device filesystems and persistent mounts. It creates filesystems, manages fstab entries using UUID, and handles mounting/unmounting.

## Usage

```kdl
disk "/dev/sdb1" {
    fs "ext4"
    mount "/mnt/data"
}

disk "/dev/sdb1" {
    fs "xfs"
    mount "/mnt/storage"
    opts "defaults,noatime"
    state "mounted"
}

disk "/dev/sdb1" {
    fs "ext4"
    mount "/mnt/data"
    state "absent"
}
```

## Parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| *(positional)* | string | Device path (e.g., `/dev/sdb1`) |
| `fs` | string | Filesystem type: `ext4`, `xfs`, `btrfs`, etc. (required) |
| `mount` | string | Mount point path (required) |
| `opts` | string | fstab mount options (default: `"defaults"`) |
| `force` | boolean | Allow reformatting an existing filesystem (default: `false`) |
| `state` | string | `"mounted"` (default), `"unmounted"`, or `"absent"` |

## States

- **mounted** — ensure filesystem exists, fstab entry present with UUID, device mounted
- **unmounted** — unmount and remove fstab entry
- **absent** — unmount and remove fstab entry (does not wipe the filesystem)

## Idempotency

The module checks:
- Current filesystem type via `blkid`
- fstab entries via `grep`
- Mount status via `findmnt`

Only formats when the filesystem type doesn't match. fstab entries use UUID for reliability across device name changes.

## Example

See the [disk-management example](/examples/#disk-management) for a complete disk setup with register and loop.
