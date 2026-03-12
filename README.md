<p align="center">
  <img src="website/public/logo.jpg" alt="glidesh" width="200">
</p>

<h1 align="center">glidesh</h1>

<p align="center">
  Fast, stateless, SSH-only infrastructure automation built in Rust.
</p>

<p align="center">
  <a href="https://opensource.org/licenses/MIT"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License"></a>
  <a href="https://github.com/alessiopcc/glidesh"><img src="https://img.shields.io/badge/rust-1.85%2B-orange" alt="Rust"></a>
</p>

---

## Features

- **Blazing fast** — built in Rust with async SSH, executes across hundreds of hosts concurrently
- **Zero dependencies** — no agent, no runtime, no Python on target machines. Just SSH.
- **Stateless** — no state files or databases. Desired state is computed fresh every run.
- **Idempotent** — two-phase check/apply pattern. Run plans repeatedly, only necessary changes are applied.
- **Modern config** — uses [KDL](https://kdl.dev) for clean, readable inventory and plan files
- **7 built-in modules** — shell, package, user, systemd, container, file, disk
- **Dry-run support** — preview changes before applying them
- **Interactive TUI** — real-time progress with a terminal UI (with non-TTY fallback)

## Install

Download the latest binary from [GitHub Releases](https://github.com/alessiopcc/glidesh/releases):

```bash
curl -L https://github.com/alessiopcc/glidesh/releases/latest/download/glidesh-linux-amd64 -o glidesh
chmod +x glidesh
sudo mv glidesh /usr/local/bin/
```

### From source

Requires Rust 1.85+.

```bash
git clone https://github.com/alessiopcc/glidesh.git
cd glidesh
cargo build --release
```

The binary will be at `target/release/glidesh`.

## Quick Start

Define your targets in `inventory.kdl`:

```kdl
group "web" {
    host "web-1" "192.168.1.10" user="deploy"
    host "web-2" "192.168.1.11" user="deploy"
}
```

Describe the desired state in `plan.kdl`:

```kdl
plan "setup" {
    target "web"

    step "Install nginx" {
        package "nginx" state="present"
    }

    step "Start nginx" {
        systemd "nginx" {
            state "started"
            enabled #true
        }
    }
}
```

Run it:

```bash
glidesh run -i inventory.kdl -p plan.kdl
```

Preview changes without applying:

```bash
glidesh run -i inventory.kdl -p plan.kdl --dry-run
```

## Documentation

Full documentation is available at the [glidesh documentation site](https://glidesh.netlify.app).

## Examples

See the [`examples/`](examples/) directory for ready-to-use examples:

| Example | Description |
|---------|-------------|
| [hello-echo](examples/hello-echo/) | Minimal example — deploy an HTTP echo server |
| [web-server](examples/web-server/) | Nginx with templated config |
| [user-setup](examples/user-setup/) | Create users with SSH keys and groups |
| [container-app](examples/container-app/) | Containerized app with ports and volumes |
| [disk-management](examples/disk-management/) | Dynamic disk formatting with register/loop |
| [multi-tier](examples/multi-tier/) | Multi-group setup with plan includes |

## License

Licensed under the [MIT License](LICENSE).
