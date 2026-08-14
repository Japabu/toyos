---
status: open
kind: defect
opened: 2026-08-14
---

# The toolchain's own cargo does not survive a sysroot rebuild, and CI never had one

The `toyos` rustup toolchain's `bin/` holds only `rustc`, so every
`cargo` invocation under `RUSTUP_TOOLCHAIN=toyos` runs through rustup's
shim, which falls back to stable's cargo and narrates it:

    info: `cargo` is unavailable for the active toolchain
    info: falling back to ".../stable-.../bin/cargo"

This was fixed once — the toolchain was given its own cargo instead of
the narrated fallback — and the 2026-08-14 sysroot rebuild (the
staleness path after the ghost-claim fix) recreated the toolchain
directory without it, so the fix lived in a step the rebuild path does
not run. CI links its toolchain fresh from the published artifact every
run and prints the fallback on every one of its cargo invocations, so
CI never had the fix at all.

The functional cost is nil (stable's cargo drives the toyos rustc
correctly; ~8-10 cargo processes per full build each print two lines).
The defect is that a fix rotted silently because nothing asserts it:
whatever step provisions or links the toolchain — the bootstrap, the
staleness rebuild, and CI's artifact link equally — must be the step
that puts cargo beside rustc, and `check_prerequisites` (or a lib
test over the link layout) should refuse a toolchain directory that
would make rustup narrate.
