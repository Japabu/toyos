---
status: open
kind: defect
opened: 2026-08-03
---

# A deleted guest test binary keeps running until its build artifact is deleted

`discover_rust_tests` enumerates whatever is in
`tests/toyos-rust-tests/target/x86_64-unknown-toyos/debug/`, and cargo does not
remove a binary when its `src/bin/*.rs` is deleted. So a renamed or merged guest
test keeps being compiled into the initrd and keeps appearing in the test list,
from an artifact nothing in the tree can produce any more.

Cost, 2026-08-03: merging three new guest binaries into one left the three
originals on disk, which (a) put ~5 MiB of dead binaries into the initrd and
overflowed the ESP — `Failed to write initrd: No space left on device`, from
`src/image.rs`, which reads as a host-disk problem and is not — and (b) gave a
*machine* test the same name as a stale *rust* test, which silently dropped it
from the run. Both took a while to see because neither error names the artifact.
The ESP's sizing was fixed in the same session and is no longer the tripwire it
was; the stale artifacts are still enumerated.

Fix shape: enumerate from the source directory, or clean the bin directory before
the build. Neither is done.
