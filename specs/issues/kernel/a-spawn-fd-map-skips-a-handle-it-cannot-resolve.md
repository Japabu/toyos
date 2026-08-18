---
status: owner
kind: question
opened: 2026-08-18
---

# A spawn's `fd_map` skips a parent handle that does not resolve, and §1.2 does not name that exception

`loader::start`'s child-handle builder answers a parent handle it cannot resolve
by moving on:

```rust
// kernel/src/loader/start.rs, in build_child_handles
let Ok(rights) = data.handles.rights_of(parent) else { continue };
```

The comment beside it argues the case: *"A pair naming a parent handle that does
not resolve contributes nothing: the child simply does not get it, which is what
a caller asking for a closed handle deserves and is not a reason to refuse the
spawn."*

**`specs/capability-endowment-spec.md` invariant 2 says the opposite, and names
exactly one exception, which is not this one.** `BadHandle`, `Stale` and
`WrongType` end the offending process; the sole carve-out is the connector
argument to `SYS_NAMESPACE_BUILD`. So the tree has a second exception that
lives only as a comment, and the child starts without a handle its parent asked
for and cannot tell that from having asked for nothing.

## Why this is the owner's and not an agent's

Three sibling bypasses of this shape were found in one audit and the other three
are fixed — `IORING_OP_POLL_ADD` and `IORING_OP_ACCEPT` (`kernel/src/io_uring.rs`)
and `SYS_DEVICE_REG` (`kernel/src/arch/syscall.rs`), each of which now answers
by `HandleError::refuse_as_error` like everything else. This one is different in
kind: the other three are a *call* refusing its own argument, and this is a
spawn deciding what a child is born holding. Making it fatal changes what a
correct parent may do, and the argument at the site is a real one rather than an
oversight. Either it becomes §1.2's second named exception or it becomes a kill;
both are rulings, and `specs/capability-endowment-spec.md` §1.2 is where the
answer belongs once it is made.

Found by the mechanism-consolidation audit
(`specs/assessments/2026-08-15-mechanism-consolidation-audit.md`), filed
separately when its three siblings were fixed.
