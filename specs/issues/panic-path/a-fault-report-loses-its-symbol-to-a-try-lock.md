---
status: open
kind: defect
opened: 2026-08-14
---

# A fault report prints a bare address instead of a symbol when the process table is busy, and says nothing about it

`process::with_user_symbols` — what `resolve_user_symbol` and
`resolve_user_symbol_return` both go through — opens with

```rust
let Some(guard) = PROCESS_TABLE.try_lock() else { return false };
```

and takes a second `try_lock` on the symbol table below it. Its callers in
`arch/idt/exceptions.rs` read the `false` as *"this address has no symbol"* and
print the raw number:

```rust
if !process::resolve_user_symbol(pid, ctx.frame.rip) {
    log!("    {:#x}", ctx.frame.rip);
}
```

**Those are two different facts and the report cannot tell them apart.** A
symbol that is genuinely absent and a lock that happened to be held both render
as a hexadecimal address, and nothing in the line says which.

## Measured, on the dev host

**Two different tests, one mechanism.** In five full `cargo test` runs on
`wt/toyos-logd` at L3 step 2 (`72d88e6`), `fault_gates` reds twice —
*"expected the faulting function in the #DE backtrace"* — and `disk_backtrace`
once — *"expected the faulting function's demangled name — a process loaded off
a disk got a backtrace with no names in it"*. Neither is on `--known-red`'s
index. All three reds were **green in the harness's own ALONE retry**, which is
the harness reporting a host-load classification rather than a defect in the
code under test.

The capture shows exactly the shape above:

```
[kernel 3.884 cpu0 tid=0]   rip:
[kernel 3.884 cpu0 tid=0]     0x10000004cbb
...
[kernel 3.884 cpu0 tid=0]   Backtrace:
[kernel 3.884 cpu0 tid=0]     0x10000004f36  fault_gate_child::main+0x136
```

The **backtrace under it resolved**, from the same table and the same process,
milliseconds later. So the symbols were there and the first lookup lost the
race; nothing about the address was unresolvable.

## Why it is worth fixing rather than tolerating

The `try_lock` is right: this runs from a fault handler, and a handler that
blocks on a lock the faulting thread may itself hold is worse than one that
degrades. What is wrong is that the degradation is **silent**, which is the
defect this tree names everywhere else — a bound whose overrun says nothing.
The report should say `(symbols unavailable: the process table is held)` rather
than print a number that reads as a verdict, and a gate asserting on a symbol
should then red on the *reason* rather than intermittently on the symptom.

## What holds the lock, which is the other half

`scheduler::reap_poisoned` takes `PROCESS_TABLE.lock()` — unconditionally, and
on **every trip round the idle loop** (`sched/driver.rs`'s `idle_loop`). So the
window a crash report's `try_lock` has to miss is not rare at all: it is
whatever fraction of the time some CPU is between the top of that loop and its
`pass`. Anything that makes CPUs go round the idle loop more often widens it,
and heavy logging is exactly such a thing — today because a CPU declines to
sleep while the byte ring owes bytes (`driver.rs`'s pre-`hlt` conditions, and
`scheduler::log_health`'s own doc records that feedback), and after
`specs/log-architecture-spec.md` L3 because a kernel thread is woken at every
record and hands its CPU back to the loop when it parks.

**It is not L3's defect.** The `try_lock`, the caller's reading of its `false`
and `reap_poisoned`'s unconditional lock all predate it; what L3 changes is how
often the two meet. The entry is filed rather than fixed for that reason, and
because the fix is a *reporting* change — say why the symbol is missing — rather
than a lock change.
