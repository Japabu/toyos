---
status: open
kind: finding
opened: 2026-08-18
---

# Log comments still cite section and stage numbers of a document that no longer exists

`specs/log-architecture-spec.md` was a completed plan and was deleted. Every
citation of it by path went in the same change, but two vocabularies it left
behind did not, because removing them is a prose edit at each site rather than a
substitution:

- **Bare section numbers.** `(§4.2)`, `§2.3a's bracket`, `§5.6's half`. Measured
  2026-08-18, `grep -c '§'` over the fourteen source files the log subsystem
  owns — `kernel/src/log/`, `userland/logd/src/`,
  `userland/test-runner/src/log_gate.rs`, `tests/common/logread.rs`: **42**.
  More sit in `kernel/src/arch/apic.rs`, `kernel/src/object/ops.rs`,
  `kernel/src/drivers/serial.rs`, `kernel/src/sched/`, `src/build.rs` and
  `src/redlist.rs`.
- **Stage numbers.** "at L6 of", "between L3 and L5", "not built at L4".
  `grep -rnE '\bL[0-9]\b'` over `kernel/`, `userland/`, `tests/`, `src/` and the
  small crates: **48 occurrences in 27 files**, an upper bound — a few of those
  are CPU cache levels rather than chunk names.

Neither is a false claim: each sits beside prose that states the rule on its
own, which is why the deletion did not have to wait for this. What they are is a
pointer at nothing, in the one place a reader looks when the comment beside it
does not answer their question.

The fix is mechanical and it is a judgement per site: delete the reference where
the sentence stands without it, and say the thing where the reference was doing
the work. It is not a substitution, which is why it was not folded into the
deletion.
