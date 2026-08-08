---
status: open
kind: defect
opened: 2026-08-05
---

# An std change that depends on an unlanded ABI change cannot be built at all, from anywhere

`library/std` names its ToyOS dependencies by relative path —
`toyos-abi = { path = "../../../toyos-abi" }` in `rust/library/std/Cargo.toml`
— and `rust/` is the primary checkout's. So std always compiles against
`/Users/jan/Dev/jan/toyos/toyos-abi`, which is main's, no matter which worktree
runs the build and no matter who holds the sysroot. `--claim-sysroot` does not
change it: a claim decides *whose witness is recorded*, not which sources
`x build library` reads. There is no copy step.

The consequence is an ordering nobody is told about: **a change to
`toyos-abi`/`toyos` and a change to std that uses it cannot land in one series.**
The ABI half must reach main first; only then does the primary's tree carry it
and the std half compile. Found on 2026-08-05 by task #140, whose
`clock_epoch() -> Option<u64>` and the `SystemTime::now` that consumes it are
exactly that pair — the std half fails with `no method named unwrap_or found for
type u64`, which reads like a broken checkout and is not one.

The second half of that task's fix, to be applied *after* the ABI is on main.
`rust/library/std/src/sys/time/toyos.rs`, replacing the body of
`SystemTime::now`:

```rust
    pub fn now() -> SystemTime {
        // `UNIX_EPOCH` on a machine that has no wall clock: this signature
        // cannot express the absence, and the kernel reads its clock once at
        // boot, so the answer is the same for the life of the process.
        SystemTime(Duration::from_secs(toyos_abi::syscall::clock_epoch().unwrap_or(0)))
    }
```

Until it is applied `SystemTime::now()` returns `UNIX_EPOCH` on every ToyOS
machine — which is what it did before task #140, so nothing regressed by
leaving it out; it is the improvement that is deferred, not a defect that is
introduced.

**`rust/` is shared, so an uncommitted edit there is everyone's.** That patch
sat in the primary's submodule for about an hour on 2026-08-05 and failed three
other worktrees' landings at `--land`'s step 4, which requires the submodule
clean. `rust/` takes the fork-estate discipline in CLAUDE.md: explicit paths, no
`stash`, and nothing left dirty across a task boundary.
