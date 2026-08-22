---
status: open
kind: finding
opened: 2026-08-22
---

# Nothing in the suite raises `#DB`, so `exceptions::debug_handler` is 60 lines no test reaches

`kernel/src/arch/idt/exceptions.rs`'s `debug_handler` is the `#DB` arm of
`trap_dispatch`. It writes a marker straight to the UART, disarms `DR7`/`DR6`,
reads `DR0`, resolves a symbol, walks a backtrace and returns to resume — and
no guest test in `tests/toyos.rs` reaches any of it. `rg 'DB TRAP|WATCHPOINT|watchpoint' tests/`
is empty (2026-08-22).

It used to be reachable from inside the kernel: `arch/debug.rs` carried
`set_context`, `watch_write`, `clear`, `monitor_pte` and a timer-tick PTE
poller. Those were deleted as dead code and the module header records why — a
session that wants a watchpoint sets it from the debugger. What is left is the
handler, which is now raised only by a Ring 3 `TF` or an `int 1`, and no
program this tree builds does either.

**Found by the `arch/` `undocumented_unsafe_blocks` sweep**, and it is why one
reduction there was declined rather than taken: the value-at-watched-address
read spells out `safe_read_kernel`'s two checks by hand, and collapsing them is
one fewer `unsafe` block and one fewer copy of a predicate — but a restructure
with no test under it is not what that sweep does. The block says so at the
site.

Either half closes this: a guest binary that sets `TF` (or executes `int 1`)
and a test that reads the handler's own `!!! DB TRAP !!!` marker out of the
console capture, which would make the reduction above ordinary; or the handler
goes the way its arming tools did, and `Vector::Debug` joins the default arm.
The second is not free — `#DB` has a gate because a vector without one
escalates to `#DF`, which halts the machine — so it means routing the vector to
`exception_handler`, not deleting the gate.
