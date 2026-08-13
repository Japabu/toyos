---
status: open
kind: defect
opened: 2026-08-13
---

# Two concurrent `cargo test --lib` invocations collide on fixed temp-dir names

Reproduced by running two `cargo test --lib` processes at once in the same
worktree: one run failed 1 test, the other 27, both from git-plumbing
fixtures stepping on each other's checkout under
`/var/folders/.../T/toyos-*`. Examples pulled straight from the two logs —
same directory name, two processes racing inside it:

```
git ["commit", "-qm", "base"] in .../T/toyos-pr-abi-alone-seed
git ["config", "commit.gpgsign", "false"] in .../T/toyos-pr-merge-first-seed
git ["add", "f", ".gitignore"] in .../T/toyos-pr-first-push-seed
git ["commit", "-qam", "somebody else's ABI change, landed"] in .../T/toyos-standing-behind
```

`buildlock::tests::killed_holder_releases_the_lock` failed the same way in
the other process: `the lock was not actually held` — the fixture's lock
file lives at a name derived the same way. Re-running either process's
`cargo test --lib` alone, immediately after, is green — confirming
contention rather than a real regression.

The fixture paths are `std::env::temp_dir().join(format!("toyos-<thing>-{name}"))`
where `name` is the test's own name, not the process's:
`src/buildlock.rs:709`, `src/pr.rs:512,513,517,620,650`,
`src/toolchain.rs:1318,1459,1573`. Two processes running the same test
function name build the identical path. `src/assets.rs:317` and
`src/forkcheck.rs:428` already fold `std::process::id()` into the same kind
of name for the same reason — the fix here is the same pattern, applied to
the three files that don't have it yet.

This matters because multi-agent sessions run `cargo test --lib` concurrently
as a matter of course — a build lock serializes the *build*
(`src/buildlock.rs`) but nothing serializes these tests' own fixture
directories against a second `cargo test --lib` process using them at the
same time.
