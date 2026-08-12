---
status: open
kind: defect
opened: 2026-08-11
---

# 190 pointers name an issue *directory*, which is the one form `every_named_issue_file_resolves` cannot fail

`specs/issues/README.md` states the rule and states why:

> **Name the file, not the directory.** `specs/issues/audio/hda-tone-phase-check.md`
> is a claim something can check; `specs/issues/audio/` is a claim that an area
> exists, which says nothing about whether the entry you meant is still there. The
> gate resolves every `specs/issues/<area>/<slug>.md` path written anywhere in the
> tree and names the ones that do not exist.

`src/docs.rs`'s `issue_paths_in` keeps only candidates that end in `.md` and
contain a `/` after the prefix, so a bare `specs/issues/audio/` is discarded
before the resolution step. That is correct as written — there is nothing to
resolve — and it means the gate's coverage is exactly the pointers that already
obey the rule. **Writing the unresolvable form is the way past it.**

Measured 2026-08-11 on `9d535d6`, counting `` `specs/issues/<area>/` `` in
`specs/`, `CLAUDE.md` and the four subdirectory `CLAUDE.md`s: **190 occurrences
across 83 (file, area) pairs.** The heaviest are
`specs/assessments/code-quality-review-2026-08.md` with 19 to `design-debt/`,
`specs/assessments/type-safety-audit/crates.md` with 11 to `isolation/` and 9 to `kernel/`,
and `specs/ci-plan.md` with 7 to `hardware/`. Twenty-two of them are inside
`specs/issues/` files pointing at each other. `specs/issues/README.md` itself has
one, as the example of what not to write, which any fix has to allow for.

The cost is the one the README names and it has been paid: an area pointer
survives the entry being closed, renamed or moved, so a reader following one
lands in a directory of thirty files and picks whichever looks closest. The
`hardware/` pointers in `specs/ci-plan.md` §9.2 and §9.4 are the worked example
— each stands for a different entry and none says which.

`src/redlist.rs` works around it rather than fixing it: a row's `source` must
resolve **and** must name the test the row is about, which is a stronger check
than path resolution and is why that field is a file. That is one table's
discipline, not the tree's.

Two shapes for a fix, and the choice is not obvious.

- **Refuse the bare form.** One more branch in `issue_paths_in`, and 190 sites to
  convert first. Some of them genuinely mean "this area", and those become
  awkward.
- **Refuse it only where a file is meant** — a pointer inside a sentence that
  also names a slug, say. Cheaper to adopt and much harder to specify.

Whoever takes it should decide before converting anything, because the
conversion is the expensive half either way.
