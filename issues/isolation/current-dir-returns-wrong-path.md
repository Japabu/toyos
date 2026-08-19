---
status: assigned
kind: defect
opened: 2026-08-01
---

# `std::env::current_dir()` silently returns a wrong path

`getcwd` in `rust/library/std/src/sys/pal/toyos/os.rs:7` passes a fixed
`[u8; 256]`, and `sys_getcwd` copies `min(cwd.len(), buf.len())` and returns that
length with **no error and no signal that it truncated**
(`kernel/src/arch/syscall.rs:736-743`). Any cwd over 256 bytes yields a
truncated path, which the program then builds every other path from.

**A correctness defect, not a path-length limitation.** A refusal would be a
limitation; a wrong answer that looks right is worse, because every consumer
inherits it silently. Found the hard way — it reported 256 bytes for a 2 KiB cwd
and made an agent's test fail against a broken instrument, which is the specific
cost of an instrument that lies rather than refuses.

Fix approved and staged as two halves, and **the kernel half must land first**:
`sys_getcwd` reports the required length instead of claiming success, then std
allocates and retries. Landing the std half alone would have nothing to retry
against.
