---
title: External Modules
description: Use community and custom modules via the plugin system.
---

External modules extend glidesh with community or custom functionality. They are standalone executables that communicate via a JSON-over-stdio protocol. Plugins are process-isolated and never receive SSH credentials — they request operations through glidesh, which proxies them over the existing SSH session.

## Usage in Plans

External modules use the `external` keyword followed by the module name and an optional resource name:

```kdl
step "Configure nginx" {
    external "acme/nginx-vhost" "mysite" server_name="example.com"
}
```

- First positional argument — module name (e.g., `"acme/nginx-vhost"`)
- Second positional argument — resource name (e.g., `"mysite"`)
- Named arguments — module parameters, same as built-in modules

The `external` keyword makes it visually clear when a step uses community code vs a built-in module.

## Module Naming

Module names support GitHub-style `owner/name` format for distributed modules:

```kdl
external "acme/nginx-vhost" "mysite" server_name="example.com"
external "acme/cleanup" timeout=30
external "mycompany/deploy-helper" "api" env="production"
```

The canonical name comes from the module's `describe` response, not its filename.

## Discovery

glidesh searches for external modules in this order (first match per name wins):

1. `./modules/` relative to the inventory file
2. `~/.glidesh/modules/`

Executables must start with the `glidesh-module-` prefix and respond to the describe handshake. The filename doesn't need to encode `/` characters — `glidesh-module-acme-nginx-vhost` is fine as long as the describe response returns the correct name.

## Parameters

External modules receive the same parameter types as built-in modules:

| Type | KDL Syntax | JSON Encoding |
|------|-----------|---------------|
| string | `key="value"` | `{"string": "value"}` |
| integer | `key=42` | `{"integer": 42}` |
| boolean | `key=#true` | `{"bool": true}` |
| list | `key { - "a"; - "b" }` | `{"list": ["a", "b"]}` |
| map | `key { x "1"; y "2" }` | `{"map": {"x": "1", "y": "2"}}` |

## Namespacing

External modules live in a completely separate namespace from built-ins. An external module named `"shell"` does not conflict with or shadow the built-in `shell` module — they are looked up from different registries.

## Example

```kdl
plan "deploy" {
    step "Install base" {
        package "nginx" state="present"
    }

    step "Configure vhost" {
        external "acme/nginx-vhost" "mysite" {
            server_name "example.com"
            listen 443
            upstream "http://localhost:8080"
        }
    }

    step "Reload nginx" {
        systemd "nginx" state="restarted"
    }
}
```

See [Writing Plugins](/advanced/writing-plugins/) for how to build your own external module.
