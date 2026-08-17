# specs/

`specs/` itself holds living normative documents, written in the present
tense and maintained to match the current system.

`specs/plans/` holds staged intentions. A completed plan is deleted; any
durable result moves into a spec.

`specs/assessments/` holds dated evidence: written once, frozen, never
maintained. A new assessment carries its date in its filename; legacy files
keep their names.

`specs/reference/` holds non-normative fact sheets: external formats,
hardware inventories.

`specs/issues/` is the defect tracker, governed by its own README.

Work narration is not committed to `specs/`; it belongs in commit messages
and pull-request bodies.

## What a spec contains

A document in `specs/` proper states the system, and only that: invariants
and requirements, interface and protocol semantics, failure semantics,
exclusions. Diagrams are welcome. A policy number appears when the number
itself is the policy.

It contains no code — no snippets or pseudocode, no type, function or test
names as exposition, no file paths or line numbers. No goals, purpose
sections, or justification prose; a rule carries one clause of reason only
where its removal would make the rule ambiguous. No history, measurements,
dates, or provenance. No references into `plans/`, `assessments/`,
`issues/`, a `CLAUDE.md`, or the source tree — a sibling spec may be
referenced, and a plan references its spec, never the reverse. No
self-reference and no slogans. The testing documents are the one exception
for naming instruments and tests: there, the instruments are the subject.

## Before a spec exists

A proposed permanent concept earns its document by surviving three questions,
answered in writing before the document is started. The answers are a
paragraph each; this is minutes of work, and it is cheaper than every
instrument that would otherwise be built to verify a concept that should not
exist.

**Necessity.** Delete the proposed concept. Which required property can no
longer be guaranteed by the concepts that already exist? Name the workload
that demonstrates the loss, and state the property without using the proposed
concept's own vocabulary. No workload, no concept. Where the honest answer is
that an existing concept must grow a second job, the answer is to merge and
not to add.

**Scaling.** For every quantity in every bound the proposal states: who sets
it — the kernel, the hardware, or the workload? A guarantee that omits a term
the workload sets is refused rather than repaired, and a constant derived from
an assumed rate is a guarantee of that kind.

**Authority.** Does the mechanism assert what its own site observes, or does
it predict what another site will do later? A prediction becomes measurement,
accounting, or policy. It does not become an invariant.

An architecture document — one that states permanent concepts rather than one
subsystem's semantics — carries the surviving answers: one sentence per
concept, and a ledger of concepts, mechanisms, invariants and gates before the
design and after it. That is the one justification a document here may carry,
and it is carried because a later reader needs it to delete the concept again.

## Checking a spec against the tree it describes

A spec is read before its subsystem is touched, so a false claim propagates
into the next change. Re-run this method whenever a spec needs re-verifying,
not only in a dedicated sweep.

**`git show HEAD:<path>` is the arbiter, never the working tree.** What is on
disk may be someone's uncommitted, half-finished change in either direction —
looking fixed when it is not, or looking stale when it was just corrected.

- **Check per claim, not per file.** Every falsifiable statement — a path, a
  line number, a type or function name, a call-site count, "X has no
  callers", a threshold, "Stage N done" — goes to the code and gets read
  there.
- **Re-derive every finding.** Verify against the code, not against prose
  handed over by another pass or a commit message that claims to have
  changed it.
- **Ask what would make a gate green other than the thing it tests.** Three
  shapes recur: a gate that cannot fail (nothing ever exercised the failure
  path); a bound whose cost nothing measures (a correct-looking check that
  silently disables what it protects once it bites); a gate that goes quiet
  (the change under test narrows the gate's own coverage rather than
  violating it — publish how much a windowed or filtered check actually
  covered, and gate on that number too, not only on pass/fail).
- **When a spec, comment or plan claims test T guards property P, break P
  and run T.** If T still passes, the claim is false — reading cannot
  substitute for this check.
- **A test may assert a known limitation on purpose**, so that fixing the
  limitation later turns the test red. Its failure message names the
  expected transition, so whoever trips it reads it as the limitation
  lifting, not a regression.
- **Ask what a bound refuses that it shouldn't**, not only whether it refuses
  what it should — a bound can look correct and still silently disable the
  fast path it exists to protect once it bites.
- **Re-verify a generalized fix at every level the shape recurs**, not only
  the level that prompted it.
- **A comment's stated reason is a claim too**, separable from the rule it
  defends: the rule can be right while the reason given for it is false.
- **Check the premise before building the fix** — confirm the defect is real
  and belongs to the subsystem a spec or issue names before changing
  anything. A fix applied over an untouched defect stops the next reader
  from looking.
- **Resolve an ambiguous verb before resolving the claim.** "Revoke",
  "release", "close", "drop", "reset", "flush" each name more than one
  operation; name the actor, the object, and whether it is against the
  object's objection before matching the verb to code.
- **Search both the bare symbol and `.method_name(`** — a bare symbol search
  misses calls made through a guard or trait object. Confirm the symbol
  itself; a similar-looking identifier is not the same one.
- **Parse a file's own data, not its prose.** A file carrying both numbers
  and narrative about them drifts between the two, and the narrative is what
  people read — check the numbers directly.

Classify every finding:

| | Meaning | Where it goes |
|---|---|---|
| **STALE** | Spec asserts something false about current code | Fix the spec |
| **OPEN DEFECT** | Spec identified a real problem; the code never got it | `specs/issues/` — a code bug |
| **PLAN CHANGED** | Code deliberately diverges from the spec's intent | Ask the owner — a design decision, not a doc fix |
| **ACCURATE** | Verified true | Nothing |

Then treat the document by its genre. A living document (`specs/` proper)
gets fixed outright. **A dated record's body is never rewritten** — editing
it falsifies an account of its own date; give a resolved finding a
superseded header at the top, or a bracketed annotation in place, instead.
A document whose filename itself misleads gets its warning at the top, not
the bottom.
