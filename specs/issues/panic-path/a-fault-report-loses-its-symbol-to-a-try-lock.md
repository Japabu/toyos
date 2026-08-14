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

`fault_gates` reds intermittently in a full `cargo test` — twice in four runs on
`wt/toyos-logd` at `c1be7c4`+L3 step 2, green in the harness's own ALONE retry
both times, which is the harness reporting a *host-load* classification. The
`rs::fault_gates` message is *"expected the faulting function in the #DE
backtrace"*, and the capture shows exactly the shape above:

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

## What made it visible

Any change to machine-wide scheduling timing exposes it; the branch that found
it wakes a kernel thread at every log record, which is a great many more passes
during a crash report than before. **It is not that branch's defect** — the
`try_lock` and the caller's reading of its `false` both predate it — and the
entry is filed rather than fixed for that reason.
