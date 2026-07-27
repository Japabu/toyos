# ToyOS

A custom OS with bootloader, kernel, and userland built from scratch in Rust.

## Current milestones

**Self-hosting the Rust compiler** -- Getting `rustc` to compile and run inside ToyOS, building Rust programs from within the OS itself.

**Running Doom** -- Doom (doomgeneric) runs natively on ToyOS with the custom compositor, software rendering, keyboard input, and sound.

![Doom running on ToyOS](doom.jpg)

## Prerequisites

- QEMU
- Rust (with rustup)

## How to run

```
cargo run
```

This automatically initializes git submodules, bootstraps the custom Rust toolchain (on first run), builds the kernel, bootloader, and userland, then launches QEMU.

Subsequent runs detect changes and only rebuild what's needed. Std-only changes rebuild in ~8 seconds.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Two exceptions: `userland/doom` links [doomgeneric](https://github.com/ozkl/doomgeneric) and is therefore GPL-2.0, and the third-party crates vendored under `userland/` keep their own upstream licenses.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be dual licensed as above, without any additional terms or conditions. No CLA.
