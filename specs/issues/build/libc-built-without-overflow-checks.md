---
status: open
kind: finding
opened: 2026-08-07
---

# `userland/libc` is the one guest artifact built without overflow checks

`src/libc.rs` passes `--release`, so the C runtime std links into every userland
binary has `overflow-checks` and `debug-assertions` off while everything around
it has them on. Deliberate on two grounds — CLAUDE.md gives the POSIX
compatibility layer explicitly relaxed rules, and `libc::build` is gated on
`stamps::dir_changed` over the *source* directory, so changing the flag alone
would not rebuild the installed archive and the manifest would then claim
something the artifact does not have. Recorded so that "one profile, applied
consistently" is not read as covering it.
