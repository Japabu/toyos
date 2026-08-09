---
status: assigned
kind: defect
opened: 2026-08-03
---

# An empty directory does not stat as a directory: kernel half landed, std half owed

**The kernel half is done and gated.** `Vfs::list` now consults `created_dirs`, so an
empty directory answers `Ok(vec![])` and a path no directory could be answers
`Err(NotFound)` — where both used to be `NotFound` (the entry previously said both were
`Ok(vec![])`; that was wrong in the detail and right in the conclusion, and the code it
named had returned `NotFound` for an empty listing since the root commit).
`empty_dir_stat` is the gate, asserting the distinction at the syscall boundary and
through `fs::read_dir`, with the non-vacuity check that a directory holding a file still
lists. Reverting the `created_dirs` lookup reds it at "an empty directory must list as
empty, not refuse". `fs::read_dir` on an empty directory therefore works now; it used to
be `NotFound`.

**The std half is one line and is not landed.** `sys::fs::toyos::is_dir`
(`rust/library/std/src/sys/fs/toyos.rs:367`) reads a zero-length listing as "not a
directory":

```rust
match syscall::readdir(path_bytes, &mut buf) {
    Ok(n) => n > 0,                                 // <- becomes Ok(_) => true
    Err(SyscallError::ResourceExhausted) => true,
    Err(_) => false,
}
```

With the kernel half landed, `Err(NotFound)` is the only "not a directory" answer, so
`Ok(_) => true` is both correct and complete — a file answers `NotFound` too
(`prefix` is `"foo.txt/"` and nothing lives under it). Until it lands,
`fs::metadata("/tmp/d").is_dir()` is still `false` for an empty `d`, and `cp x d/` still
writes a *file* named `d`. `toybox_file_tools` still puts a file in every directory it
makes for that reason.

**Why it was not landed with the kernel half, which is a process constraint and not a
technical one.** `rust/` is the primary checkout's, and in a linked worktree it is the
empty stub `git worktree add` leaves (`specs/worktrees.md` §2) — so a worktree agent can
neither edit nor build it. The sysroot witness covers `toyos-abi`, `toyos` and
`userland/libc` and *not* std's own sources
(`specs/issues/build/std-change-needs-an-unlanded-abi-change.md`), so the change would also not be picked
up without `--claim-sysroot`, which rebuilds the shared sysroot and cleans every other
worktree's target directories mid-session. This is the same two-half shape as
`specs/issues/isolation/current-dir-returns-wrong-path.md` and takes the same answer: the kernel half lands first
and is safe alone — `is_dir` returns `false` for an empty directory before and after it,
so nothing regresses and the distinction becomes available. Batch the edit with that
same `rust/` work in one quiet-tree window.

Found in the same file and **not** fixed, for the same reason: `FileAttr::file_type`
(`fs/toyos.rs:88`) answers `is_dir` with `self.file_type == syscall::FileType::Pipe`, and
`stat` at `:507` builds a directory's `FileAttr` with `file_type: FileType::Pipe` to
match. A directory is spelled "pipe" throughout, with a comment excusing it rather than a
type that could not express it. Same window.
