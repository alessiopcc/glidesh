# Contributing to glidesh

Thank you for your interest in contributing to glidesh! This guide will help you get started.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/<your-username>/glidesh.git`
3. Create a branch: `git checkout -b my-feature`
4. Make your changes
5. Submit a pull request

## Development Setup

You need Rust 1.85+ installed. Then:

```bash
cargo build
cargo test
cargo clippy
cargo fmt
```

## Before Submitting

- Run `cargo fmt` to format your code
- Run `cargo clippy -- -D warnings` and fix all warnings
- Run `cargo test` and ensure all tests pass
- Write tests for new functionality

## Integration Tests

Integration tests require Docker and are gated behind an environment variable:

```bash
GLIDESH_INTEGRATION=1 cargo test
```

These spin up Ubuntu containers with SSH and systemd to test modules against real systems.

## Commit Messages

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat: add new module for X
fix: handle edge case in file upload
refactor: simplify template interpolation
docs: update CLI reference
test: add integration test for disk module
chore: update dependencies
```

## Code Style

- Keep comments minimal — use rustdoc (`///`) for public APIs and "why" comments for non-obvious decisions. Avoid "what" comments that restate the code.
- Follow the existing module pattern (`check`/`apply`) when adding new modules.
- Keep functions small and files focused.

## Adding a New Module

1. Create `src/modules/<name>.rs` implementing the `Module` trait
2. Register it in `src/modules/mod.rs` via `ModuleRegistry`
3. Add integration tests in `tests/<name>.rs`
4. Document it in `website/src/content/docs/modules/<name>.md`

## Reporting Issues

- Use [GitHub Issues](https://github.com/alessiopcc/glidesh/issues)
- Include steps to reproduce, expected behavior, and actual behavior
- Include your OS, Rust version, and glidesh version

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
