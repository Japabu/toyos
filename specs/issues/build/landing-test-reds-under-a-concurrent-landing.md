---
status: open
kind: defect
opened: 2026-08-08
---

# `a_landing_fast_forwards_main_to_the_branch` reds when another worktree is landing

`cargo run -- --land` from `wt/toyos-sf2` on 2026-08-08 failed its gate in 3.1 s
on a `--lib` test, not on the suite:

```
thread 'land::tests::a_landing_fast_forwards_main_to_the_branch' panicked at src/land.rs:777:9:
the lock was left held
test result: FAILED. 60 passed; 1 failed
```

The assertion is `buildlock::integration_is_free(&primary)`. It failed during a
run whose own log shows it had queued **123 s** behind `pid 35271 (landing)` —
another worktree's real landing — and the same test passed alone immediately
after, and passed inside two full gates earlier in the same session (61 of 61,
twice). It reds only while a real landing is in flight elsewhere.

**The mechanism is not established, and the obvious explanation is wrong.** The
lock looks correctly scoped: `integration_path` goes through
`git_common_dir(root)`, which resolves git's relative `.git` against `root`
(`src/lib.rs:55`) and canonicalises, so the temp primary's lock is its own file
and not the real repo's. Something else leaves the temp repo's lock held at the
assertion — a child outliving the `Guard` that owns the fd would fit the
load-dependence, since `flock` releases per open file description, but that was
not demonstrated. Reproduce it by holding the real integration lock from a
second process rather than by waiting for a peer.

Cost while open: a landing whose gate is otherwise clean reds on a three-second
`--lib` test. That is the cheapest possible red, but it is indistinguishable at a
glance from a real one. Same class as `parallel-tests-red-under-other-suites`'s
wall-clock reds — a verdict that depends on what else the host is doing — except
this one is in `--lib`, which is meant to be the fast hermetic half.
