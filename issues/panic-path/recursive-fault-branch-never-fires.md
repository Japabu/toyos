---
status: open
kind: defect
opened: 2026-07-31
---

# `fatal_exception`'s `recursive` branch never fires for a nested `#PF`

`page_fault_handler` swaps the fault state to `PageFault` *before* dispatching
(`arch/idt/exceptions.rs:468`), and `fatal_exception`'s `recursive` tests only
`Fatal | Panic` (`:522`). A `#PF` nested inside a panic — the exact case the
short-circuit exists for — is therefore classified non-recursive and runs the
full `crash_report` again.

Termination still holds, through the panic console's `PAINTING` latch and the
per-CPU reentry guard, so this is not a live loop. But `a431e02`'s commit
message credits the `recursive` branch with bounding a renderer fault, and that
mechanism does not fire; the latch is doing all the work. Either widen the test
to include `PageFault`, or stop claiming the branch bounds anything.
