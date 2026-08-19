---
status: open
kind: defect
opened: 2026-08-18
---

# Comments across the tree cite section numbers of documents that no longer exist

This began as a log-subsystem finding: `specs/log-architecture-spec.md` was
deleted, every citation of it by path went with it, but two vocabularies it left
behind did not, because removing them is a judgement at each site rather than a
substitution.

**The whole of `specs/` has since been deleted, so the class is now tree-wide
and it is a defect rather than a finding.** Every `§N.N` in a ToyOS comment now
points at nothing. Nothing greps it: a path citation is findable and a bare
section mark is not, which is why this one outlived the sweep that removed the
paths.

Measured with `git grep -c '§' -- '*.rs' '*.toml' '*.yml' ':!specs/' ':!rust'`
on 2026-08-19: **581 lines**, by area —

| area | lines | the document it pointed at |
|---|---|---|
| `toyos-sched/` | 222 | the scheduler core |
| `kernel/` | 164 | mixed: the log, the IOMMU, the user machine state, capability endowment — **and genuine external citations** (Intel SDM, xHCI 1.2, USB 3.2, virtio 1.2) that must stay |
| `tests/` | 55 | the testing strategy, the audio subsystem, the device-test rules |
| `userland/` | 50 | the audio subsystem, 36 of them in `userland/soundd/src/main.rs` alone |
| `toyos-xhci/` | 31 | **external** — xHCI 1.2. Leave. |
| `toyos-fat32-check/` | 16 | **external** — Microsoft's fatgen103. Leave. |
| `src/` | 11 | the testing strategy and the CI assessment |
| `toyos-hda/` | 10 | mostly **external** — the HDA specification |
| `toyos-keymap/` | 9 | mostly `§` as a character literal in layout tests. Leave. |
| `toyos-cc/` | 4 | **external** — the C standard. Leave. |
| `toyos-pci/` | 3 | **external** — the PCI specification. Leave. |
| `toyos-abi/` | 1 | |

**581 is an upper bound on the work and roughly half of it is not work at all.**
An external citation is the correct kind of pointer — it names a document that
exists, is versioned, and is not ours to delete. Separating the two is the first
pass, and it cannot be done by grep: it is read per site.

The same is true of the stage labels the log work left — "at L6 of", "between L3
and L5" — measured at **48 occurrences in 27 files**, an upper bound, since some
are CPU cache levels.

None of this is a false claim. Each mark sits beside prose that states the rule
on its own, which is why the deletion did not have to wait for it. What it is, is
a pointer at nothing in the one place a reader looks when the comment beside it
does not answer their question.

**The fix is a judgement per site**: delete the reference where the sentence
stands without it, and say the thing where the reference was doing the work.
`toyos-sched/` is the largest single block and was already recorded on its own
before this widened — `issues/design-debt/toyos-sched-cites-sections-a-spec-cut-deleted.md`.
