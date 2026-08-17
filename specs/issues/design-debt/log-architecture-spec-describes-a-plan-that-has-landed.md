---
status: open
kind: finding
opened: 2026-08-17
---

# `log-architecture-spec.md` still reads as a pending plan; the design it describes has already landed

`specs/log-architecture-spec.md` lives in `specs/` proper — a "living
normative document, written in the present tense and maintained to match the
current system" (`specs/README.md`) — but its entire body is written as a
staged plan: `L0`–`L9` "chunks" not yet done, `C7+C8` obligations placed on a
sibling branch, "this branch removes", "L6 deletes `log_file.rs` whole", and
so on throughout.

The design it describes is not pending. It is on the tree, landed on
2026-08-15, three days after this file was last touched:

- `kernel/src/log_file.rs` and `kernel/src/drivers/log_ring.rs` — both do not
  exist (`git log --oneline --diff-filter=D -- kernel/src/log_file.rs`:
  `9ca7631`, "log: /bin/logd writes the file, and the kernel stops being a
  logger").
- `userland/logd/` exists and is what `CLAUDE.md` now describes as owning
  `/log`.
- `toyos-abi/src/syscall.rs` has `SYS_LOG_READ: u64 = 114` — exactly the
  number this spec's §3.4 says its L0 computes.
- `kernel/src/arch/apic.rs`'s `wait_for_log_file` and `owed` are written
  against `crate::log::user::durable_ns()` and
  `crate::log::read::newest_committed_at_ns()` — the post-migration shape —
  and the function's own doc comment says **"Re-derived at L6"**, in the
  vocabulary of this very document.
- `kernel/src/arch/percpu.rs`'s `alloc_log_shard` doc comment reads *"Here
  rather than in `init_ap`, which is where an earlier draft of **the
  spec** put it"* — the kernel source is citing this document by name as
  settled history, not as an open plan.

None of this is a normative claim this pass may touch — it is reported here,
not fixed, per the citations-only scope of the pass that found it. But the
document's own framing is now false in a way that outlasts any individual
citation: a reader who does not already know the log design landed will read
`L3`, `L6`, `C7+C8` and reasonably conclude the work is still ahead of the
tree. `CLAUDE.md`'s "Planned work" list does not carry this document (unlike
`specs/completion-architecture-spec.md`, which is genuinely still pending),
which is itself a second signal that the owner already considers this one
settled.

Found 2026-08-17 during a citation-format pass over
`specs/log-architecture-spec.md` (mirroring PR #110's pass over
`specs/completion-architecture-spec.md`). What is owed is a rewrite to
present tense describing the shipped design, with the chunk-by-chunk plan
narrative either deleted or moved to a place `specs/README.md` allows history
to live — not a citations-only agent's to do.
