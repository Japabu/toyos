---
status: open
kind: defect
opened: 2026-08-07
---

# `toyos-cc`'s preprocessor exits the process instead of returning

Three `process::exit(1)` calls in `toyos-cc/src/preprocess/mod.rs`: a `#error`
directive (line 309) and a missing include, system or otherwise (lines 527,
530). A library denying its caller the choice — the compiler cannot report the
diagnostic in its own format, cannot continue to find a second error, and
cannot be embedded in a driver that wants to keep going. Every other error in
the crate returns. Recorded by the determinism task rather than fixed, on the
owner's standing rule about staying focused.
