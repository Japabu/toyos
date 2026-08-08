---
status: open
kind: defect
opened: 2026-08-08
---

# Nothing in the tree checks any of the above

Both violations that prompted the audit — `fsck_msdos` and the SoundFont's GPLv2
— existed for months and were found by collision. There is no ledger of allowed
crates, allowed binaries, or asset provenance, and no check reads one.

`NOTICE` is now most of §11.3's asset ledger written out by hand — every
committed third-party file with its `sha256`, its upstream and its licence — so
the remaining work there is a `#[test]` that reads it, not the research. Nothing
reads it today, so a new binary file still arrives unremarked.

`specs/dependency-audit-2026-08-08.md` §11 proposes three, all offline, all
inside `cargo test --lib`, and prices each including what it cannot catch — the
crate ledger is the one with teeth, the binary-literal scan is the weakest, and
neither reaches `rust/`'s own dependencies or a third-party build script. The
same constraint that governs fork pins governs these: **anything touching the
network must be an on-demand command, never `cargo test` and never the landing
gate.** Nothing was built, deliberately: every one of these would go red on the
tree as it stands, and seeding the ledgers is a decision about which findings
above are accepted.
