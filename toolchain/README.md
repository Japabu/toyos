# toolchain

Build orchestrator that compiles the Rust compiler and standard library targeting x86_64-unknown-toyos (using Cranelift codegen) and registers the result as a rustup toolchain.

Builds toyos-ld first, then runs the Rust bootstrap build system.
