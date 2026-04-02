---
title: Examples
description: Ready-to-use glidesh examples for common infrastructure tasks.
---

Each example includes an `inventory.kdl`, `plan.kdl`, a `README.md`, and any supporting files. Find them in the [`examples/` directory](https://github.com/alessiopcc/glidesh/tree/main/examples) of the repository.

## hello-echo

A minimal example that deploys an HTTP echo server. Great starting point.

**Modules used:** shell, container

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/hello-echo)

## web-server

Install nginx, deploy a templated configuration file, enable the service, and run a health check.

**Modules used:** package, file (template), systemd, shell

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/web-server)

## user-setup

Create deploy users with specific shells, groups, and SSH authorized keys.

**Modules used:** user, file, shell

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/user-setup)

## container-app

Deploy a containerized application with port mappings, environment variables, and persistent volumes.

**Modules used:** container, shell

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/container-app)

## disk-management

Discover available disks dynamically, then format and mount each one using register and loop.

**Modules used:** disk, shell, register, loop

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/disk-management)

## multi-tier

A multi-group setup with web, app, and database tiers. Uses plan includes for shared configuration.

**Modules used:** include, package, file, systemd, container, shell

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/multi-tier)

## external-module

Use an external plugin to set the system MOTD. Demonstrates the `external` keyword and the JSON-over-stdio plugin protocol with a simple Python module.

**Features used:** external modules, plugin protocol, SSH proxy

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/external-module)

## jump-host

Connect to internal hosts through an SSH bastion (jump host). Demonstrates group-level and per-host jump host configuration with user/port inheritance.

**Features used:** jump hosts, SSH tunneling

[View source →](https://github.com/alessiopcc/glidesh/tree/main/examples/jump-host)
