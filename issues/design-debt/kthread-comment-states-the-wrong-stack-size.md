---
status: open
kind: finding
opened: 2026-08-15
---

# `kthread.rs` justifies a panic with a 16 KiB stack; the stack is 128 KiB

```
kernel/src/sched/kthread.rs:191:  /// allocate a 16 KiB stack at boot has nothing to fall back to, and a kernel
kernel/src/process.rs:214:        pub const KERNEL_STACK_SIZE: usize = 128 * 1024;
```

`kthread::spawn` allocates through the same `alloc_kernel_stack`
(`kernel/src/loader/start.rs:24`) as the other two `enqueue_new` callers, and
that function uses `KERNEL_STACK_SIZE`. The comment is off by 8×.

It matters only because of what the sentence is doing: it is the justification
for treating an allocation failure as unrecoverable at that point. "A machine
that cannot allocate 16 KiB at boot has nothing to fall back to" is a much
stronger claim than the same sentence about 128 KiB, and the argument gets its
force from the smaller number. The conclusion may still be right; the premise as
written is not the one the code tests.

Filed as a finding rather than a defect because nothing misbehaves — no branch
reads the number, and the allocation itself is correct. It is a false statement
in the place a reader goes to learn why the panic is allowed.

Found during the 2026-08-15 mechanism-consolidation audit while inventorying
spawn paths; verified at `71a0559`.
