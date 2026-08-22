---
status: open
kind: defect
opened: 2026-08-22
---

# The touched-names scan misses a guest test that is discovered, not registered

Since #216 a landing renders the price verdict only for the names the change
registered or re-tiered (`src/durations.rs`, `--tier-base`): the scan diffs
the registration tables in `tests/toyos.rs` (`MACHINE_TESTS`, `SCREEN_TESTS`,
`AUDIO_TESTS`) and `src/tiers.rs`'s `RELEGATED` between the base and the head.

A Rust guest test is neither. `tests/toyos-rust-tests/src/bin/<name>.rs` is
found by `discover_rust_tests` (`tests/toyos.rs:1255`) from the built
binaries, and its name never appears in a table. PR #218 added
`netd_gone_mid_bind` that way, and its `durations` job on run `32568941432`
said:

    [durations] the tier verdict is rendered for no name: this change
    registered and re-tiered nothing against e93fba7b…

The change had registered one. What saved the run was the marker rule — a
committed `UNMEASURED` is a declaration verdict and is refused at every base —
so the job still reded for the right reason. But the *price* verdict on a
discovered name would be a warning on its own pull request: a Rust guest test
committed at a price in the band would land, and red the nightly the next day
under "somebody else's test" — on exactly the author who should have been
told.

## What would fix it

The scan also counts a file added or changed under
`tests/toyos-rust-tests/src/bin/` between base and head, with the name being
the file stem — the same rule `discover_rust_tests` applies. One more source
in the same function, and a unit test that a synthetic diff touching only a
`src/bin/` file names that test.
