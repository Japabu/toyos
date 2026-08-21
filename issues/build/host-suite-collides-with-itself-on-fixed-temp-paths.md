---
status: open
kind: defect
opened: 2026-08-21
---

# Two `cargo test --lib` runs on one host red each other, on fixed temp-directory names

`cargo test --lib` in the root project is the host suite for `src/`. Its
`buildlock`, `pr` and `toolchain` tests each build a git fixture in a temp
directory named for the test rather than for the process, so two runs on one
machine are two processes writing one directory.

Measured 2026-08-21 on the dev host: one run alone is `149 passed; 0 failed;
1 ignored`, exit 0. Two runs overlapping — which is what a second worktree doing
the same thing looks like — gave `FAILED. 130 passed; 19 failed` and exit 101,
with reds of the shape

```
thread 'buildlock::tests::a_clean_cannot_land_inside_a_build' panicked at src/buildlock.rs:716:9:
git init in /var/folders/.../T/toyos-buildlock-race-unlocked

thread 'toolchain::tests::a_checkout_identical_to_main_has_no_standing_to_claim' panicked at src/toolchain.rs:1847:9:
git ["add", "-A"] in /var/folders/.../T/toyos-standing-identical
```

and one that is not a `git` refusal but a wrong answer:

```
thread 'buildlock::tests::a_queued_exclusive_phase_goes_first' panicked at src/buildlock.rs:1128:9:
assertion `left == right` failed
  left: "ex\nex\nsh\nsh\n"
 right: "ex\nsh\n"
```

That last one is the reason this is a defect and not an annoyance. The others
fail loudly on a `git` command that cannot run; this one *completes* and compares
the union of two runs' output against one run's expectation. A reader who did not
know a sibling was running would read it as the queue ordering being wrong.

**Why it matters here.** `CLAUDE.md`'s working model is one agent per worktree on
one host, and `src/buildlock.rs` exists precisely to arbitrate that host across
worktrees — but it arbitrates *guests and builds*, not these fixtures. Nothing
warns; the second run simply reds. The names are `/tmp/toyos-<subject>` with no
pid, no nonce and no lock.

**The fix is a name, not a lock.** A fixture directory per process — a pid or a
counter in the name, removed on drop — makes the suite reentrant and costs
nothing. A host-wide lock would serialise a suite that has no reason to be
serial.

Found while running the gate for `.github/workflows/gate-a.yml`'s exit-code fix;
not fixed there, because a red that only appears when two agents overlap is
exactly the kind that should be reproduced deliberately before it is touched.
