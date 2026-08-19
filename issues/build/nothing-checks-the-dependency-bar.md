---
status: open
kind: defect
opened: 2026-08-08
---

# Nothing in the tree checks the dependency bar

Both violations that prompted the audit — `fsck_msdos` and the SoundFont's GPLv2
— existed for months and were found by collision. There is no ledger of allowed
crates, allowed binaries, or asset provenance, and no check reads one.

Three offline checks were proposed on 2026-08-08, all cheap enough to live
inside `cargo test --lib`, each priced including what it cannot catch:

- **A crate ledger** — a committed file naming every third-party crate the tree
  may resolve, with a one-line reason; a `#[test]` unions the `name = ` lines of
  all 28 `Cargo.lock` files and reds, by name and lockfile, on anything not in
  it. The one with teeth, because it does not judge a crate but forces somebody
  to judge one at the moment it arrives, which is the step that was missing.
  Seeding it is 471 names, 43 of them direct.
- **An external-binary ledger** — the same shape over `Command::new` string
  literals and `/sbin/…`-style path literals in every `.rs` file and every
  `.github/workflows/*.yml`. The weakest of the three: it catches the CI YAML's
  `dosfstools` and the macOS tools invoked by absolute path, and cannot
  distinguish `Command::new("find")` from a string that happens to say "find" —
  which is how three of the four macOS tools are actually invoked.
- **An asset ledger** — every binary file git tracks plus every third-party
  source corpus, with `sha256`, upstream, licence, and whether it ships in the
  image; a new file or a changed hash is red.

None of the three reaches `rust/`'s own dependencies, what a third-party build
script does, the truth of a licence claim, or a fork clone that a gitignored
`.cargo/config.toml` path-overrides. The same constraint that governs fork pins
governs all of them: **anything touching the network must be an on-demand
command, never `cargo test` and never the landing gate.**

`NOTICE` is now most of that asset ledger written out by hand — every committed
third-party file with its `sha256`, its upstream and its licence — so the
remaining work there is a `#[test]` that reads it, not the research. Nothing
reads it today, so a new binary file still arrives unremarked.

**Nothing was built, and the owner refused all three as brittle on 2026-08-08**,
accepting only the fork-pin check `cargo run -- --check-forks`, which goes red on
the state of the world rather than on the tree. Every one of the three would go
red on the tree as it stands, and seeding the ledgers is a decision about which
of the audit's findings are accepted. The concern they were written against is
what this entry is: the bar is stated and nothing measures it.
