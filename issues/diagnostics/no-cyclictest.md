---
status: open
kind: finding
opened: 2026-08-08
---

# There is no cyclictest, so nobody can ask this machine what its wake latency is

`grep -rni cyclictest` over the tree returns 12 hits, all in two spec files and
none in code: `specs/plans/metal-boot-plan.md:350-351` ("A real cyclictest-equivalent
for ToyOS should exist before the first metal boot — it is the instrument that
turns the boot into a measurement") and `specs/reference/production-audio-baselines.md:343-347`
and `:667-670`, which state the design — an RT-priority thread that arms an
absolute timer, sleeps, and histograms `actual − programmed` at 1 µs resolution —
and the consequence: "Until such a tool exists, **no honest 2x claim can be made
on this metric in either direction**." That is CLAUDE.md's hard bar, unmeasurable
for scheduling.

**What exists is not a substitute, and each instrument fails differently.**
soundd's `max_wake_lat_ns` (`userland/soundd/src/main.rs:995-996`, the null-sink
copy at `:1371-1372`) is read by gate A (`tests/common/audio.rs:478-488`,
`:636-645`) and baselined in `tests/audio-baseline.toml:18-22`; the thorough tier
runs Mann-Whitney on `max_wake_lat_us` (`tests/toyos.rs:1668`). But it is a
**max over a ~2 s window, not a distribution** — no percentiles, no sample count;
it measures against a DLL's *prediction of a DMA completion*, not against a
programmed timer, so it folds in the device model; and it needs soundd plus a
sound card to exist at all, which is exactly what the T14 has not got.
`toyos-sched`'s invariant I4 (`specs/scheduler-core-spec.md:1043`) bounds the
same quantity but is marked `sim`, so it can never see TCG distortion, real IPI
delivery, or metal.

**One concrete blocker before it can be written.** `SYS_SET_RT_PRIORITY` is
gated at its dispatch site on owning an audio device claim — `PermissionDenied`
unless the caller owns `VirtioSound` or `HdaAudio`
(`kernel/src/arch/syscall.rs:684-689`), whose own comment says "Spec §9.4 wants
a privilege; a claim is not one". A standalone latency tool cannot reach the RT
band today without also taking the sound card away from soundd, which changes
the machine it is trying to measure.
