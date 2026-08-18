---
status: open
kind: design-debt
opened: 2026-08-15
---

# The kernel's console tag is composed by replacing the ABI formatter's first byte

The log architecture puts one formatter in `toyos-abi`, beside
the record, so the kernel's console sink, the panel, `logd` and any diagnostic
tool produce byte-identical text from one implementation. `Display for
LogRecord` renders

```
[0.123 cpu0 tid=3] the message
```

and the kernel's console line has always been

```
[kernel 0.123 cpu0 tid=3] the message
```

— the same line with a tag *inside* the bracket. So the sink cannot simply
prepend: the formatter's first byte is the bracket the tag has to go through.
`kernel/src/log/console.rs`'s `Line::write_str` therefore strips a leading `[`
from the first fragment and writes `[kernel ` in its place.

## Why it is like that

**`toyos-abi/src` is sysroot source** (`src/toolchain.rs`'s `SYSROOT_SOURCES`),
so a branch that touches it claims the shared sysroot from its first build until
it lands, and `pr::abi_lands_alone` refuses a branch that mixes an ABI commit
with work that depends on it. The log architecture's ABI landed as chunk zero
(§11) and this branch may not reopen it. Re-deriving the fields kernel-side was
the alternative and is the thing §3.3 exists to prevent — two implementations of
one line, and the panel is the one that would drift.

## The fix, for whoever opens the ABI next

```rust
impl LogRecord {
    /// The line with `tag` inside the bracket: `[kernel 0.123 cpu0] …`.
    pub fn tagged(&self, tag: &str) -> Tagged<'_>;
}
```

`Display` becomes `self.tagged("")` over one private `fmt_with_tag`, so there is
still exactly one implementation, and `log::console::write_line` loses its
`strip_prefix` and its `tagged` flag. `logd` wants the same wrapper for its own
wall-clock prefix (§5), so the next ABI landing has two callers for it.

## What it is not

It is not a silent failure mode. If the formatter's leading bracket ever goes,
the fragment passes through whole and the line reads `[kernel [0.123 …` — wrong
and visible — rather than losing its first character.

## Also here: an early record now carries `cpu0`

Composition changed one thing about the line, deliberately. The byte ring's
prefix wrote `[kernel 0.001 boot]` for a record written before per-CPU was
ready; the record renders `[kernel 0.001 cpu0 boot]`, because cpu0's shard *is*
the boot shard and the ABI writes the origin before the flag. No test reads that
field and the new form says strictly more. Recorded because "byte for byte what
it is today" is the ruling this sits under, and this is the byte it is not.
