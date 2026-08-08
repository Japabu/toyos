---
status: open
kind: finding
opened: 2026-08-02
---

# Rustup narrates its cargo fallback on every invocation

`info: cargo is unavailable for the active toolchain` followed by `info:
falling back to ".../nightly-.../bin/cargo"`, one pair per cargo call: 5 pairs
in `cargo run -- --build-only`, 249 in a full `cargo test`. The build system
sets `RUSTUP_TOOLCHAIN=toyos` (`src/build.rs:185`,
`tests/common/compile.rs:42`) or passes `+toyos` (`src/libc.rs:29`), and the
linked `toyos` toolchain is rust's stage2 sysroot, which ships `rustc` and
`rustdoc` and no `cargo`.

Recorded rather than fixed, because each way out costs more than the noise:

- **Ask rustup once and reuse the answer.** It will not answer:
  `RUSTUP_TOOLCHAIN=toyos rustup which cargo` fails with `'cargo' is not
  installed for the toolchain 'toyos'`. Only the shim applies the fallback and
  only by narrating it, so "resolve once" means parsing the path out of a
  human-facing `info:` line — a diagnostic used as an interface.
- **Reimplement the fallback rule** in the build system. Duplicates rustup
  policy; when the two disagree the symptom is a cargo/rustc mismatch rather
  than a clear failure.
- **Give the toolchain a cargo** — symlink `rust/build/<host>/stage0/bin/cargo`
  into stage2's `bin/` from `link_toolchain`. Smallest, and arguably the right
  pairing, since stage0's cargo is the one rust's own bootstrap runs against
  this compiler where the ambient fallback is four months older (1.96.0-nightly
  driving a 1.99.0-dev rustc). But it writes into a directory `x.py` owns and
  changes the cargo behind every ToyOS build, so it needs a verification run of
  its own.

Not by redirecting the shim's stderr: rustup reports real errors on it.
