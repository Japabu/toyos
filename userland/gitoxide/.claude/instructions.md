# Claude Code Instructions for Gitoxide

See [.github/copilot-instructions.md](../.github/copilot-instructions.md) for project conventions,
architecture decisions, error handling patterns, and development practices.

## Quick Reference

- `cargo test -p <crate-name>` to test a specific crate
- `cargo check -p gix` to check the main crate with default features
- `just check` to build all code in suitable configurations
- `just test` to run all tests, clippy, and journey tests
- `cargo fmt` to format all code
- `cargo clippy --workspace --all-targets -- -D warnings -A unknown-lints --no-deps` to lint all code
