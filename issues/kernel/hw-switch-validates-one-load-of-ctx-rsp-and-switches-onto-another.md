---
status: open
kind: defect
opened: 2026-08-21
---

# `Hw::switch` validates one load of `ctx.rsp` and switches onto a second one

`kernel/src/hw.rs`'s `switch` reads the incoming context's saved stack pointer
**twice**, and the guard that decides whether the machine may stand on it sees
only the first read.

```rust
let incoming: &KernelCtx = &*restore;
check_switch_frame(incoming, &token);   // reads ctx.rsp, tests it three ways
...
incoming.cr3.activate();                // inline asm, memory clobber
...
let rsp = incoming.rsp;                 // reads ctx.rsp again
context_switch(&raw mut (*save).rsp, rsp);
```

It is not a compiler whim that can be relied on to go the other way. `objdump`
of a `sched-tripwire` kernel built from `bc6675e0`:

```
    fd94:  mov (%r14),%rcx      <- check_switch_frame's load
    ...    (canonical + 8-alignment tests, the [rsp+56] read, the stack-bounds
           test, then set_count / set_current_tid / set_kernel_stack)
    fe5c:  mov %rax,%cr3        <- opaque asm with a memory clobber
    ...
    fefc:  mov (%r14),%rsi      <- the value the switch actually uses
    ff17:  jmp context_switch
```

`cr3.activate()` is `asm!` with side effects, so LLVM *may not* forward the
first load across it. The second load is therefore forced by the code as
written, and `stack-witness`'s third test — "the incoming `rsp` is inside the
stack its own `kernel_stack_top` names" — guards a word the machine then
discards and re-reads. A write landing on `KernelCtx.rsp` in that span passes
every check and the `popfq`/`ret` runs off whatever it left.

## It is not being exercised, and that is measured

`kernel/Cargo.toml`'s `switch-witness` compares the eight words of the frame
*and* re-reads `ctx.rsp` from inside `context_switch`, one instruction after
`mov rsp, rsi` — so a disagreement between the two loads is exactly what it
reports. In 6,901 twelve-wide `bootable.img` boots on 2026-08-21, against 20
kernel deaths in those same boots, it fired **not once**: `ctx.rsp` was the same
word at the check and at the switch every time.

So this is a soundness hole in the guard rather than a live defect. It is filed
because a guard that validates a different load than the one it guards is worth
nothing the day something does write that field, and because the fix is smaller
than this file: have `check_switch_frame` *return* the value it validated and
pass that to `context_switch`, so one load exists.

## What it would cost to take

A scheduler change, so CLAUDE.md's two checks bind. The negative control already
exists and is committed — `switch-witness-mutate-rsp` moves `ctx.rsp` by eight
after the check, which today reaches the `mov rsp, rsi` and would not once the
load is hoisted, so the arm's verdict flips from "the witness fires" to "the
machine survives". The independent oracle is `objdump` of the emitted `switch`:
one `mov (%r14),…` instead of two.
