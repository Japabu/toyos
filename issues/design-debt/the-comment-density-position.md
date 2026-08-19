---
status: none
kind: rejected
opened: 2026-08-08
---

# The comment-density position, and the hardware-name scrub it comes with

Five of the owner's 35 review notes are one position stated five times —
*"the whole codebase has too many comments. good code speaks for itself.
accompanied by spec documents per subsystem or whatever that should suffice"*
(`main.rs`), *"why so long comments?"* (`bootloader/main.rs:164`), *"does it
make sense to have so many comments in each source file or should we instead
refer to the spec in the module and just let the code speak for itself?"*
(`sched/driver.rs`), *"theres slop narration in the comments"* (`log_file.rs`),
and *"'now runs the whole way' thats narration slop"* (`bcachefs_adapter.rs:17`,
still present verbatim). Filed once, not five times.

Measured 2026-08-08, counting lines whose first non-space characters are `//`,
`/*` or `*` (so trailing comments are **not** counted and the real figure is
higher):

- `kernel/src`: **11,920 comment lines of 43,739 — 27%.**
- First-party Rust as a whole (`kernel bootloader toyos toyos-abi userland
  toyos-desktop toyos-elf toyos-sched src`): **21,424 of 96,848 — 22%.**
- Worst files over 200 lines: `heartbeat.rs` 174/260 (**66%**),
  `arch/tlb.rs` 138/279 (49%), `log_file.rs` 264/564 (46%), `arch/apic.rs`
  145/323 (44%), `drivers/xhci/mod.rs` 777/1825 (42%), `fat32_adapter.rs`
  407/997 (40%).

The rule this measures against already exists — CLAUDE.md's slop-comment
paragraph, and the 2026-08 code-quality review's narrowing of the surviving
kinds to three: the one-clause invariant at the edit site, the boundary
contract, and the refusal-reason at a surprising decision, over a module doc
that is the contract and nothing else, target ten lines. **What does not
exist is the sweep**, and nothing in the tree measures density or would notice
it rising.

The same note carries the **hardware-name scrub**: *"never mention ThinkPad in
the kernel source code. its just our example machine we have right now. the
kernel is general."* The review recorded this as "~20 sites across six kernel
files". Measured, it is much larger: **6 `ThinkPad` mentions across two kernel
`.rs` files** (`log_file.rs:3`, `drivers/i8042/mod.rs` ×5) and **59 `T14`
mentions across 26 kernel `.rs` files**, plus 34 more outside `kernel/`
(`bootloader/`, `toyos/`, `toyos-abi/`, `toyos-xhci/`, `toyos-hda/`,
`toyos-ps2/`) and a further set in `kernel/Cargo.toml`'s feature commentary.
The bound or the behaviour stays; the machine that produced it moves to the
commit message. Note that a plain deletion loses information in the cases where
the machine *is* the evidence — `i8042/mod.rs:1420`'s "a device that will not
answer at all" is one — so this is a rewrite per site, not a `sed`.
