---
status: open
kind: defect
opened: 2026-08-08
---

# Four deletions the 2026-08 review named, each small and each still there

Grouped because none is worth a task of its own and all four are the same
judgement: code that exists to have existed. Verified on `main` 2026-08-08.

- **`arch/gdt.rs`** — a 4-line re-export shim left behind when the GDT went
  per-CPU. The review recorded one caller of `KERNEL_CS`; it is now four
  constants (`KERNEL_CS`, `STAR_SYSRET_BASE`, `USER_CS`, `USER_DS`) over five
  sites — `arch/syscall.rs:107`, `loader/start.rs:60,61,85,86`. They import
  from `percpu` and the file goes. The owner's note: *"no empty files no lazy
  refactorings."*
- **`arch::apic::init_timer_ap()`** (`apic.rs:269`) — an empty function with
  one caller (`smp.rs:269`). The behaviour is correct: calibration is one
  global measurement and `arm_one_shot` programs divide and LVT per call, so
  there is genuinely nothing for an AP to do. Delete the ceremony. It may
  legitimately return for heterogeneous ARM cores; that day reintroduces it.
- **`arch/mod.rs:3`'s `#[allow(dead_code)]` on `debug`** — masks exactly five
  uncalled investigation tools: `set_context`, `watch_write`, `clear`,
  `monitor_pte`, `check_pte_monitor`, zero callers each. `read_dr6` and
  `context` have one caller each, from the #DB handler. Delete the five (git
  history is the shelf), keep the two, drop the allow. Distinct from the
  crate-level `#![allow(dead_code)]` in `specs/issues/build/`, and smaller — but the same bar.
- **`block.rs:73`'s private `PAGE_SIZE = 4096`** — the owner asked whether it
  belongs to the paging subsystem. It does, and **there is nothing there to
  move it to**: `mm/paging.rs` exports only `PAGE_SIZE_BIT` (a PDE flag) and
  `mm/user_span.rs` only `PAGE_2M`. Five private 4 KiB constants exist with no
  common owner — `block.rs:73`, `file_cache.rs:13`, `file_backing.rs:9,10`,
  `fat32_adapter.rs:75`, `usb_gate.rs:31`. Whoever does this makes the export
  first.
