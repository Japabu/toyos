# ToyOS

A bootloader, bootable image creator and kernel attempt from scratch

## Prerequisites

- QEMU
- Rust (with rustup)

## How to run

```
git submodule update --init
cd toolchain && cargo run
cd ../bootable && cargo run
```

The toolchain step builds a custom Rust compiler+std from the `rust/` submodule and links it as `+toyos`. Only needed once (or after modifying `rust/`).
