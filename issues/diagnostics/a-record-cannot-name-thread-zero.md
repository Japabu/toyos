---
status: open
kind: defect
opened: 2026-08-14
---

# A record's one formatter drops `tid=0`, and the first thread of every process is `Tid(0)`

`toyos_abi::log::LogRecord`'s `Display` — the one formatter §3.3 of
`specs/log-architecture-spec.md` says every consumer renders through — writes
the thread only when it is non-zero:

```rust
if self.tid != 0 {
    write!(f, " tid={}", self.tid)?;
}
```

So zero is the field's "no thread here". **It is also a real thread.**
`ProcessEntry::new` returns *"the allocated main tid (always `Tid(0)` for the
first thread)"* (`kernel/src/process.rs`), and tids are per-process, so `Tid(0)`
is the main thread of every process on the machine. Measured over the committed
T14 logs (`grep -oh 'tid=[0-9]*' specs/assessments/metal-logs/*/*.log`, 2026-08-14):
**738 `tid=0` against 49 `tid=1`** — the value the formatter drops is the one
almost every line carries.

The kernel's own sentinel is a third value again: `PerCpu::current_tid` is
`u32::MAX` when no thread is running (`kernel/src/arch/percpu.rs:85`), which the
formatter would render as `tid=4294967295` on every line a kernel thread logs.

## What is in the tree now

L2 translates at the boundary: `kernel/src/log/mod.rs`'s `on_a_thread` maps
`u32::MAX` to zero, so the panel never prints the raw sentinel. That closes the
loud half and leaves the quiet one — a main thread and a kernel thread now
render identically, where the byte ring's prefix distinguished them (`[kernel
0.123 cpu0 tid=0]` against `[kernel 0.123 cpu0]`). The byte ring still carries
the old prefix, so today the distinction survives on serial and is lost only on
the panel; **L3 deletes the byte ring** and the distinction goes with it.

## Why it was not fixed there

`toyos-abi/src/log.rs` is a sysroot source (`src/toolchain.rs`'s
`SYSROOT_SOURCES`), so an ABI change lands on its own pull request and L2 cannot
carry one.

## The options

1. **A `flags` bit.** `LogRecord::flags` has one bit used (`FLAG_EARLY`); a
   `FLAG_NO_THREAD` says "this record has no thread" out loud and leaves `tid`
   meaning only what it says. Costs nothing on the wire and matches how the
   early-boot label is already carried.
2. **Render `pid`/`tid` together.** The record already holds `pid` and no
   consumer prints it, which is its own smell: a per-process tid is only an
   identity beside the pid it belongs to. `[0.123 cpu0 3/0]` names a thread;
   `tid=0` names one only if you know the process.
3. **Renumber tids from one.** Cheapest to render and the worst of the three —
   it puts an ABI rendering decision inside the process table.

Option 1 or 2, on the ABI-only landing that L3 or L4 already needs.
