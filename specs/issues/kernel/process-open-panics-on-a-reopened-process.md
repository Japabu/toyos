---
status: open
kind: defect
opened: 2026-08-10
---

# `SYS_PROCESS_OPEN` asserts, so re-opening a live process panics the kernel

`HandleEntry::new` asserts `!core.retired()` (`kernel/src/object/handle.rs`),
and `retired` is set by `HandleEntry::drop` for **every** row, `immediate`
included. The rule it encodes — *"resurrection is a kernel bug and never a
userland one, because userland cannot name an object it holds no handle to"* —
holds for every object type but one.

`ProcessObject` is the exception. The process table keeps an `Arc` to it for the
process's whole life (`kernel/src/process.rs`), and `sys_process_open`
(`kernel/src/arch/syscall.rs`) installs a *fresh* handle onto that same object
from a pid. So:

1. spawn a child, keep running it, close its `Process` handle;
2. `SYS_PROCESS_OPEN(syscap, pid)` for the same pid;
3. `assert!` in `HandleEntry::new` → kernel panic.

Userland can name it, because a pid is a name and `SYS_PROCESS_OPEN` is the one
call that turns one into authority.

## Reachability

`Rights::MANAGE` on a `SysCap`, which only `/bin/init`'s carries — the kernel
mints exactly one and init narrows every dup. And `SysCap::open_process`
(`toyos/src/syscap.rs`) has no caller in the tree. So it is **latent**: the
mechanism is in the ABI, nothing calls it, and the first caller finds a panic.

The analogous hazard for device buffers was noticed and avoided —
`device::try_claim` builds a fresh `SharedMemObject` per claim over a shared
`Arc<Region>` (`kernel/src/device.rs`, argued in `kernel/src/object/shm.rs`) —
so the shape was understood; `ProcessObject` is the one long-lived `Arc` that is
re-installable and was not checked against it.

## The fix

A `ProcessObject` whose last handle went is not retired, because the process
table still answers for it. Either give `retired` a per-row meaning the macro
carries (a row that says its object outlives its handles), or have
`sys_process_open` refuse a retired object with `SyscallError::Gone` — which is
the wrong answer for a live process and would be a second defect wearing a word.
The first is the honest one.

Whichever: the assert must not be reachable from a syscall argument. *"Fail-fast
is for kernel bugs, not for untrusted input"*, and a pid is untrusted input.

## Ruled not a merge blocker, 2026-08-10

Judged while clearing PR #22's blockers.

**It is not reachable from userland at all today.** `SYS_PROCESS_OPEN` needs
`Rights::MANAGE` on a `SysCap`; the kernel mints exactly one, for `/bin/init`,
and init narrows every duplicate to what a program's `syscap` row asks for —
`rg -n 'syscap' */system.toml tests/*/system.toml` names `rt`, `device` and
`dup` and never `manage`. `SysCap::open_process` has no caller anywhere. So the
panic is latent: the mechanism exists in the ABI and the first caller finds it.

**And the fix is not a line.** `retired` is set by `HandleEntry::drop` for every
row, and the honest repair is a `kobject!` column saying which objects outlive
their handles — a second keyword on all thirteen rows, plus a weakening of the
resurrection assert for exactly one of them. That is a change with its own
argument to make about which objects the *kernel* answers for after userland
stops naming them, and it should be made where somebody is looking at that
question rather than beside a launcher fix.
