---
status: open
kind: defect
opened: 2026-08-12
---

# `kernel-loom`'s supported matrix is two commands, not `cargo test --no-default-features`

`kernel-loom/Cargo.toml`'s `loom` feature is default-on, and it is what makes
the crate's `AtomicU64`/`UnsafeCell` shims resolve to loom's instrumented
types instead of `core`'s. Every `#[test]` body that calls `loom::model(...)`
depends on that: loom's model driver runs the closure across the
interleavings it finds by intercepting loom's own atomics, so a body run
against plain `core::sync::atomic` types is not exploring anything — it is
just racing real OS threads inside loom's coroutine (green-thread) runner.

The whole-crate `cargo test --no-default-features` invocation does exactly
that, and two of the four test binaries do not survive it:

- **`tests/log_record.rs`** (added on `wt/toyos-logd`) had no feature gate.
  All 5 `loom::model` bodies ran against plain atomics inside loom's
  coroutine runner and overflowed the coroutine stack in every case:
  `coroutine in thread '<name>' has overflowed its stack`. Fixed on that
  branch by adding `#![cfg(feature = "loom")]` as the file's first inner
  attribute — the sibling `tests/log_zeroed_init.rs` already documented the
  right shape: it is gated `#![cfg(not(feature = "loom"))]` and is the one
  file in the crate meant to run under `--no-default-features`.

- **`tests/tlb_shootdown.rs`** carries no such gate and reds deterministically
  under `--no-default-features`, 2 of its 3 tests, byte-identical on `main`
  (`82f85dc`) and on `wt/toyos-logd`, reproduced across repeated standalone
  runs with no variation:

  ```
  thread 'an_acknowledged_flush_postdates_the_page_table_write' panicked at tests/tlb_shootdown.rs:124:13:
  assertion `left == right` failed: cpu 1 acknowledged the shootdown while still holding a translation for the page the initiator is about to free
    left: 1
   right: 0

  thread 'one_serve_answers_two_concurrent_shootdowns' panicked at tests/tlb_shootdown.rs:163:13:
  assertion `left == right` failed: the first initiator freed a page cpu 1 could still reach
    left: 1
   right: 0
  ```

  This is not a `wt/toyos-logd` regression — it reproduces identically on
  `main` at `82f85dc`, which never touched this file.

`tests/ticket_lock.rs` passes under `--no-default-features` (2/2), but
silently: it runs no `loom::model`, so the pass says nothing about the
interleavings the file exists to check, only that a single deterministic
execution succeeded.

The supported matrix, going forward, is:

- `cargo test` (default features — every model, loom-instrumented)
- `cargo test --no-default-features --test log_zeroed_init` (the one
  file designed to run without loom)

Whoever owns this decides the fix: gate `tests/tlb_shootdown.rs` the same way
`log_record.rs` now is, and decide whether `ticket_lock.rs`'s silent
non-loom pass is meaningful enough to keep exercising or should be gated
too.
