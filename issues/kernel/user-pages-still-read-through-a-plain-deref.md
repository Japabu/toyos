---
status: open
kind: finding
opened: 2026-08-22
---

# Two kernel reads of user memory are a plain `*ptr`, not a `read_volatile`

`kernel/src/user_ptr.rs`'s `copy_in` reads user memory with `read_volatile`, on
a stated argument: one read, not one the compiler may split, fold or repeat,
because another thread of the same process can change the bytes between any two
instructions. Two reads outside that module do not follow it.

| site | what it reads |
|---|---|
| `kernel/src/scheduler.rs`, `futex_wait`'s `read` closure | `unsafe { *phys_addr.as_ptr::<u32>() }` — the futex word |
| `kernel/src/process.rs`, `dump_user_memory`'s `read_user` | `unsafe { *phys.as_ptr::<u64>() }` — a word of the crashing process |

Found by the `undocumented_unsafe_blocks` sweep of the kernel's root files
(2026-08-22): both sites are sound in every other respect — the address is
re-translated in the current address space immediately before the read, and the
alignment is checked — so the `SAFETY:` comments they now carry are true. What
they cannot say is that the read happens exactly once.

**The crash-dump one is arguably fine and is here for completeness**: the value
is printed and decides nothing, so a value another thread substituted is the
report's subject rather than a hazard.

**The futex one does not have that excuse.** It is a predicate the scheduler
evaluates to decide whether a thread parks, it runs inside a closure
`completion::wait_until` may call more than once, and a plain load is one LLVM
may hoist out of that. The fix is one word — `phys_addr.as_ptr::<u32>().
read_volatile()` — and it is behaviour-identical on x86, which is exactly why
no guest test would show the difference either way.

Not done in the sweep that found it: it is a change on the scheduler's own
park path, which root `CLAUDE.md` calls high-risk, and the sweep's brief is
documentation and reduction, not semantics.
