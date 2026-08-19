---
status: open
kind: finding
opened: 2026-08-17
---

# `MAX_SYMBOL_BYTES` is justified by two numbers the tree no longer produces

`kernel/src/loader/symbols.rs:80-86` sets the bound at 16 MiB and argues it:

> Policy, and generous by design: the largest tables any binary in this tree
> has are `bin/toyos-cc`'s 13,152,031 bytes, and `bin/sshd` is next at
> 3,769,757 — so this is the next power of two above the real worst case.

Measured on 2026-08-17 by reading the binaries with `toyos-elf` and summing the
`SHT_SYMTAB` section's size and that of the `.strtab` its `sh_link` names — which
is exactly what `read_backtrace_table` computes as `syms.size + strs.size`:

| binary | comment says | measured |
|---|---|---|
| `toyos-cc` | 13,152,031 | **4,382,380** |
| `sshd` | 3,769,757 | **2,953,531** |

Both are off by enough that the phrase "the next power of two above the real
worst case" no longer describes 16 MiB — the measured worst case is under a
quarter of it.

The binaries read were the primary checkout's
`toyos-cc/target/x86_64-unknown-toyos/toyos/toyos-cc` and
`userland/target/x86_64-unknown-toyos/toyos/sshd`, built 2026-08-15 under the
`toyos` profile. **A build-configuration difference is a live possibility and is
not ruled out** — `hosted-rustc` and the profile both move symbol table sizes —
so what is certain is that the comment's numbers do not describe the artifacts a
current build produces, not that they were wrong when written.

Nothing is broken: the bound is not reached either way, and when it is reached
the consequence is a log line and a process whose backtraces are bare addresses.
What is owed is the number, because a policy constant defended by a measurement
is only as good as the measurement, and the next agent asked to move this bound
will re-derive it from the stale figure.

Found while scoping the loader's symbol handling, 2026-08-17.
