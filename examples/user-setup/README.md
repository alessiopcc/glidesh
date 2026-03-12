# user-setup

Create deploy users with SSH keys, shells, and group membership.

## What It Does

1. Creates a `deploy` user with sudo and docker groups
2. Creates an `appuser` for running applications
3. Deploys SSH authorized keys for the deploy user
4. Verifies both users exist

## Usage

```bash
glidesh run -i examples/user-setup/inventory.kdl -p examples/user-setup/plan.kdl
```

Edit `files/authorized_keys` with your actual public keys before running.
