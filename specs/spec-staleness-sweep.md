# Spec staleness sweep — how to re-run it

A spec that asserts something false about current code is worse than no spec,
because the next agent acts on it — and this project's rule is to read the spec
before touching the subsystem it covers, so a stale spec propagates directly
into wrong work.

The first full sweep ran 2026-08-01 across all 25 files in `specs/`. **Its value
was not that it was thorough once.** A sweep is a snapshot that starts decaying
the moment it lands, and six agents commit into this tree. What makes it
survivable is being cheap to re-run. That is what this file is for.

## The rule that cost two wrong conclusions in one day

**`git show HEAD:<path>` is the arbiter. Never the working tree.**

In a tree this active, what is on disk is somebody's uncommitted opinion. Both
failures happened on 2026-08-01:

- A listener defect was recorded as *already fixed*, citing a type and a doc
  comment that were the isolation agent's work-in-progress, written twenty
  minutes earlier and not committed. Against `HEAD` the descriptor still held a
  `String` and the attack still ran.
- The same day, `allocation-owners.md` was flagged stale and was current by the
  time it was re-checked — the cache agent had committed in between.

A finding has a shelf life in **both** directions: it goes stale because the bug
got fixed, and it looks fixed because someone's WIP is on disk. Re-verify at the
moment you *act*, not when you found it. If a file looks half-finished or fails
to compile, that is the signal to switch to `git show HEAD:`.

## Method

Per item, not per file. For each falsifiable claim — file paths, line numbers,
type and function names, call-site counts, "X has no callers", "Stage N done",
thresholds, invariants — go to the code and read it.

**Re-derive; do not transcribe.** Every time a finding was handed over as prose
and applied without re-reading the code, it was worse than re-deriving it. The
code's own comments are usually better than any summary: `queue.rs:18-22`,
`msg.rs:49-54` and `task.rs:757-761` each explain a decision more precisely than
the audit that found them. Quote those.

**Verify against code, not commit subjects.** A commit titled "make the caches
evict" is evidence that someone intended to; the call site is evidence they did.
Check that `init` has a caller, that the guard has a non-`usize::MAX` value, that
the deleted function is actually gone.

## Break it and run it — the one check you cannot do by reading

**The pattern: a claim that a named test proves something, where nobody ever
checked that the test fails.** Three turned up in a single sweep:

- I5 (fairness) sat in a table headed "checked after every sim step",
  implemented nowhere.
- A spec sentence said kernel `check` builds carry a max-pass-duration assert.
  No kernel build can compile that feature, and the assert does not exist.
- A comment named `screen_late_panic` as "the one test that fails if the capture
  stops happening". Replace `panic_console::capture`'s body with `return` and it
  still passes.

**The check: when a spec, comment, or plan claims test T guards property P, break
P and run T.** If T still passes, the claim is false and the gap is real,
whatever the suite says. Cheap, decisive, and the only check here that reading
cannot satisfy — two of the three above were found this way; the third was found
by accident.

**The asymmetry is why it is worth doing: a green suite is what makes these
invisible.** Nothing looks wrong until you try to make it go red. Prose asserting
coverage reads exactly the same whether the coverage exists or not, and the
passing run is taken as confirmation rather than as the thing that was never
tested.

`specs/metal-track-history.md` records the same class from the other direction —
twelve certifications that could not fail. This is the method that would have
caught them.

## The inverse: a test that enforces a documented limitation

The dead-gate pattern is prose claiming a test proves something. Its **inverse**
is a test that asserts the current, *known-wrong* behaviour on purpose, so that
fixing the behaviour turns the test red and tells you the limitation lifted.

`pipe_peer_scope` is the model. It asserts `be604ef`'s stated residual — a peer
that only ever called `connect()` can open a pipe it was never handed and read
another client's data. It passes today. The day `SYS_HANDLE_SEND` lands it goes
red, with a panic message that says what to do:

> `GOOD NEWS, BAD TEST: delete this file and assert the refusal instead.`

**Use this whenever an entry is blocked on a named condition.** Of the three
isolation items currently blocked, it is the only one that can watch for its own
unblock; the other two rely on someone remembering. A stopgap's residual is
exactly what gets forgotten once the headline is fixed, and a test is the only
thing in this tree that reliably remembers.

Two properties make it work, and both are required: the assertion is on the
behaviour rather than on the mechanism, so it does not go red for an unrelated
refactor; and the failure message names the *expected* transition, so whoever
trips it does not read it as a regression and re-file it.

## Resolve the verb before you resolve the claim

An entry that uses a verb naming more than one operation can be retired against
the wrong one, which silently deletes a real limitation.

`SYS_GRANT_SHARED`'s "no revoke" was retired on the existence of `release` — but
`release` is a *grantee dropping its own access* (`sys_release_shared` passes
`current_process()`), while the entry meant *the owner withdrawing a grantee's
access*, which still has no mechanism. Both are "giving up access"; only one was
what the entry claimed. The tell was context: the clause sat alongside
"re-grantable" and "unvalidated target", all three about the owner's control over
who ends up with the region.

Not a stale claim — **two people using one word for two operations.** When a
retirement turns on a verb like revoke, release, close, drop, reset or flush, name
the actor and the object first: *who* does it, *to whom*, and *over their
objection or not*. Then check whether that is what the code provides.

## The two traps that actually bit

**Grep misses calls that go through a guard or trait object.** Searching
`page_cache::` showed `read`, `write_new` and `sync` as dead. They are reached as
`cache.read(...)` from `bcachefs_adapter.rs` through the guard. Before claiming
"no callers", also search `.method_name(`. This nearly retired a live entry.

**A similar name is not the symbol.** `last_armed_ticks` reads like a survivor of
`ensure_armed_before`; it is unrelated. Confirm the symbol, not the substring.

## Classification — so findings route instead of being patched into a doc

| | Meaning | Where it goes |
|---|---|---|
| **STALE** | Spec asserts something false about current code | Fix the spec |
| **OPEN DEFECT** | Spec identified a real problem; the code never got it | `known-issues.md` — a *code* bug |
| **PLAN CHANGED** | Code deliberately diverges from the spec's intent | Ask the owner; a design decision, not a doc fix |
| **ACCURATE** | Verified true | Nothing |

**OPEN DEFECT is the sweep's real yield.** It is a code bug the spec knew about
and the code never received — and it does not surface from reading code alone,
because nothing in the code looks wrong. The three uncertifiable scheduler
instruments were all found this way: I5 fairness sitting in a table headed
"checked after every sim step" and implemented nowhere; a kernel `check` build
that cannot compile because nothing forwards the feature; `from-qemu` as
`unimplemented!()` behind a §10.4 that describes a working pipeline. Each is a
certification that could not fail. That class is why this is worth repeating
rather than a documentation chore.

When the code deliberately differs, **record the reason, not just the shape** — a
spec that states what without why invites someone to "fix" the code back.

## Document classes — treat them differently

- **Live specs** must be true: fix them.
- **Dated records** (`forks-audit-2026-07-27.md`, `metal-track-history.md`,
  `scheduler-migration-log.md`, `audio-gate-history.md`, `cpu-attribution.md`,
  the `*-2026-*.md` files) are history. **Never rewrite the body** — a dated
  audit's value is being an accurate account of its date, and editing it
  falsifies a record rather than fixing a document. If a reader could act on a
  resolved finding, add a **superseded header** at the top (see
  `forks-audit-2026-07-27.md`) or a **bracketed annotation** in place (see the
  Stage 2 paragraph of `scheduler-migration-log.md`).
- A doc whose *filename* misleads gets its warning at the **top**, not the
  bottom: `bcachefs-reference.md` documents upstream bcachefs and not one byte
  this repo reads. A reader who scrolls is already lost.

## Slices

Five parallel agents, one slice each, opus (this is judgment work). They report;
they do not edit — the owner of the file applies the fix.

1. `scheduler-core-spec.md`, `scheduler-migration-log.md`
2. `iouring-blocking-spec.md`, `capability-handles-spec.md`
3. `audio-subsystem-spec.md`, `production-audio-baselines.md`,
   `tests/audio-baseline.toml`, the two audio records
4. `boot-image-split.md`, `metal-boot-plan.md`, `device-test-strategy.md`,
   `net-gate-plan.md`
5. `memory-ownership-spec.md`, `daemon-testability.md`, `bcachefs-reference.md`,
   `console-records.md`

Skip files another agent is actively editing; report those instead of racing.

## Where numbers live, check both directions

`tests/audio-baseline.toml`'s prose claimed the bar was missed on 2 of 120
config-runs. Parsing the sample 100 lines below it: `ceiling_runs` 0 in all four
configs, worst run 0.43 of a pipeline depth. **Parse the data rather than reading
the notes** — a file that carries both its numbers and its narrative will drift
between them, and the narrative is what people read.
