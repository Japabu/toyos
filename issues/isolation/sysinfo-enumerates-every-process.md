---
status: open
kind: finding
opened: 2026-08-20
---

# `SYS_SYSINFO` enumerates every process in the machine, by name, to anyone

Found by the capability audit of 2026-08-20
(`issues/kernel/the-capability-end-state-is-twelve-answers.md`, question 4).

`sys_sysinfo` takes the process table, counts every thread of every process, and
writes one 64-byte entry each into the caller's buffer
(`kernel/src/arch/syscall.rs:2472`-`2554`). Each entry carries the pid, the tid,
the scheduler state, whether it is a secondary thread, the process's resident
memory summed over its demand pages, mmap regions, TLS blocks and loaded
libraries, its accumulated CPU time, and its 28-byte **name**
(`kernel/src/arch/syscall.rs:2544`-`2551`).

**It takes no handle and demands no right.** It is one arm of the dispatch
reached by number (`kernel/src/arch/syscall.rs:483`), so a process holding an
empty handle table gets the whole census. `ps` is a toybox applet
(`userland/toybox/src/ps.rs:28`), which every symlink in `system.toml`'s
`[symlinks]` table makes reachable, and anything `/bin/init`'s `launcher` can be
asked to start reaches it too.

## Why this is the one enumeration hole

Everything else in the object graph is clean and was checked in the same pass:
no syscall lists another process's handles; a `Namespace` answers `lookup` and
has no listing operation (`kernel/src/object/namespace.rs:59`);
`SYS_ENDOWMENTS` answers the caller's own table
(`kernel/src/arch/syscall.rs:1707`); reading every record every CPU wrote is
gated on `Rights::LOG` on a `SysCap`, deliberately, because it "is every
process's business and no process's right by default"
(`kernel/src/arch/syscall.rs:1683`, `toyos-abi/src/handle.rs:80`); and the
per-kind object census is a `SYS_DEBUG` action a shipping kernel does not carry
at all (`kernel/src/arch/syscall.rs:704`, `:792`).

So one syscall answers a question about objects the caller holds no handle to,
in a tree where the log — a strictly less identifying reading — costs a right.

## What it costs

A process that was endowed one connector and nothing else learns the name, size
and CPU share of every daemon and every program the user is running, and can
watch one start and stop. It is not a path to authority: the pids it hands back
buy nothing, because `SYS_PROCESS_OPEN` is the only call that takes one and it
demands `Rights::MANAGE` on a `SysCap` (`kernel/src/arch/syscall.rs:1602`). What
leaks is the census itself.

`free` reads the same call for the header alone
(`userland/toybox/src/free.rs:5`), which needs no entries and would be
unaffected by a gate on the per-process rows.

## What would fix it

One more `SysCap` bit — bits 10..31 of `Rights` are free, `Rights::ALL` is
`0x3ff` (`toyos-abi/src/handle.rs:92`) — demanded by `sys_sysinfo` before it
writes any entry, given a name in the `SYSCAP_RIGHTS` table
(`toyos-manifest/src/lib.rs:52`) and endowed by `system.toml` to whichever
program is meant to carry `ps`, exactly as `logread` is endowed to `logd` today.
The header — total and used memory, CPU count, uptime — is a machine fact like
`SYS_CPU_COUNT` and can stay ambient.

Not done here: whether `SYS_SYSINFO` should be rights-bearing at all is one of
the four rulings
`issues/kernel/the-capability-end-state-is-twelve-answers.md` puts before the
owner.

**Ruled 2026-08-20**: gate it — one more SysCap bit, endowed by system.toml
to whatever carries ps, exactly as logread is endowed today. Implementation
queued behind the in-flight ABI landings: one ABI-bearing task holds the
machine at a time.
