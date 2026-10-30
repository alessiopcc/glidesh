# nix-deploy

Install packages with Nix on any distribution, run a command in an ephemeral Nix shell, manage a channel, and garbage-collect the store.

## What It Does

1. Installs `htop` and `ripgrep`, installing Nix itself when it is missing (`install=#true`)
2. Runs a Python one-liner in a Nix shell
3. Ensures the `nixpkgs-unstable` channel
4. Garbage-collects store paths older than 30 days

## Usage

```bash
glidesh run -i examples/nix-deploy/inventory.kdl -p examples/nix-deploy/plan.kdl
```
