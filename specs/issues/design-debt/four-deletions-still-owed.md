---
status: open
kind: defect
opened: 2026-08-08
---

# Three deletions the 2026-08 review named, each small and each still there

Grouped because none is worth a task of its own and all of them are the same
judgement: code that exists to have existed. Verified on `main` 2026-08-08.

**One of the four is done.** `arch/mod.rs`'s `#[allow(dead_code)]` on `debug`
and the five tools it masked went on 2026-08-16 (Wave A item A5 of
`specs/assessments/2026-08-15-mechanism-consolidation-audit.md`): `set_context`,
`watch_write`, `clear`, `monitor_pte` and `check_pte_monitor`, 78 lines,
with the allow. `read_dr6` and `context` stayed, as this entry said they should
— and `context` now has no writer at all, which the module header records
rather than hides: it reports zero until somebody adds a tool that arms a
watchpoint, and that tool brings the store back with it.

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
- **`block.rs:73`'s private `PAGE_SIZE = 4096`** — the owner asked whether it
  belongs to the paging subsystem. It does, and **there is nothing there to
  move it to**: `mm/paging.rs` exports only `PAGE_SIZE_BIT` (a PDE flag), and
  the one page size `mm` names is `PAGE_2M`, re-exported from
  `toyos-userbound`. Five private 4 KiB constants exist with no
  common owner — `block.rs:73`, `file_cache.rs:13`, `file_backing.rs:9,10`,
  `fat32_adapter.rs:75`, `usb_gate.rs:31`. Whoever does this makes the export
  first.
