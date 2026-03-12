# disk-management

Dynamically discover, format, and mount disks using register and loop.

## What It Does

1. Discovers available block devices (excluding the boot disk `sda`)
2. Formats each discovered disk as ext4 and mounts it under `/mnt/<name>`
3. Verifies all mounts are active

This example demonstrates the `register` and `loop` features for dynamic infrastructure tasks.

## Usage

```bash
glidesh run -i examples/disk-management/inventory.kdl -p examples/disk-management/plan.kdl
```

**Warning:** This plan will format disks. Use `--dry-run` first to verify which disks will be affected.
