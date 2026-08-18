---
status: open
kind: finding
opened: 2026-08-18
---

# A wait whose ceiling is shorter than `GUEST_QUIET` reports a kernel death as a stall

`ceiling_verdict` (`tests/common/qemu.rs`) reaches its panic arm only when the
guest has been silent for `GUEST_QUIET`, which is 15 s:

```rust
if let Some(line) = dying {
    if quiet >= GUEST_QUIET { return Some(kernel_died_here(line)); }
}
if elapsed <= ceiling { return None; }
```

The silence is the discriminator and it has to be — the same panic handler
recovers a `panic!` taken in syscall context and the machine carries on — but it
means the *order* of the two arms is decided by which clock runs out first. A
wait whose effective ceiling is under 15 s expires while `quiet` is still short,
falls through to the second arm and returns `STALLED: Ns of guard expired`,
about a guest that has a `KERNEL PANIC` in its own capture.

**No instance is known, and the margin is thin rather than absent.** Every
ceiling is `budget`-scaled by the phase width, so at width 12 the shortest in
the tree is far above the bound. At width 1 — which is exactly what the
harness's own `ALONE` re-run of a red is — the scaling is 1 and the two
shortest, `panic_recovery`'s `Duration::from_secs(15)` (`tests/toyos.rs`) and
`nvme_home_roundtrip`'s 20 s, sit on the bound and just above it. The 15 s one
is a tie the poll loop resolves in the panic arm's favour, because
`ceiling_verdict` is called before the next line is read and tests `quiet >=`.

What it costs is the word and not the evidence: since 2026-08-18 a verdict
carries `serial::death_report` whenever the capture holds a kernel death,
whichever arm produced the sentence, so a mislabelled stall still arrives with
the kernel's own report under it. What it still costs is
`Outcome::is_stall`, `src/redlist.rs`'s `Instrument` classification and
`specs/issues/build/every-recorded-stall-predates-the-panic-discriminator.md`'s
whole premise — a row recorded as a stall is a row nobody re-reads.

The fix is presumably to make the shortest ceiling in the suite a floor at
`GUEST_QUIET`, or to have the second arm say that the capture carries a death
even when the silence has not yet earned the first. Neither is worth doing on no
instance; what is worth having is the note, so the next `STALLED` on a short
ceiling is read with this in mind.
