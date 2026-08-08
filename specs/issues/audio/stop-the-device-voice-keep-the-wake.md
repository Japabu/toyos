---
status: open
kind: question
opened: 2026-08-01
---

# Stopping the device voice while keeping the timer wake — soundd-only, gate-blocked

Kept out of the cluster above because **its unblock condition is different**, and
that is the useful part: it could land *first* if the quiet tree arrives before
fork access.

Stopping the device voice while keeping the periodic timer wake recovers the DMA
engine and the codec — the battery-relevant hardware — and gives up only the wake
itself. Resume still works unchanged, because soundd keeps writing signal bytes,
so it does not need the missing client→soundd message.

So it is **not blocked on the fork**; it is blocked on the **audio gate**. A
mid-session device stop/restart is an audible transient plus a DLL re-lock, which
needs the thorough tier on a quiet tree.

**A device advertising four buffers panics soundd at startup.**
`assert!(num_buffers > 5, "soundd: pipeline too shallow to defer safely")`
(`main.rs:597`) turns a device shape into a startup panic. Same class as the NVMe
and xHCI zero-device panics closed today — an unanticipated device shape killing a
process rather than being handled — and metal-relevant, since nobody knows what
the T14's codec advertises.

The fix falls out of decoupling the client slot count from the device pipeline
depth, which turns the assert into a clamp. Which is also *why* the assert exists:
`slot_count = num_buffers` (`main.rs:1290`) couples every client's ring geometry to
the kernel's `TX_INFLIGHT_MAX`. The comment's own reasoning establishes
`slot_count >= num_buffers`; **equality was assumed, not derived.** That design is
written up and deliberately not landed — it changes ring geometry and therefore
audio timing, so it needs the thorough gate tier on a quiet tree, not the fast one.

**(5) ASSIGNED — the cpal ToyOS backend hardcodes 44100/2ch/i16** and rejects
everything else, so soundd's resampler and channel-conversion paths (spec `specs/issues/build/`/`specs/issues/hardware/`)
are unreachable from any real client and effectively untested. It also
`assert_eq!`s the device rate against a compile-time constant, so changing the
driver's rate aborts every cpal app.

Deferred to the quiet-tree window, not neglected: editing that fork needs
`.cargo/config.toml` path overrides, which redirect cpal for **every** agent in
the tree. Same scheduling constraint as the fork lint audit
(`specs/fork-lint-audit-plan.md`).

**Client liveness is blocked on this, not on soundd.** The ambiguity between a
paused and a wedged client is *specified*: §6.4 defines pause as "no explicit
coordination required", and the cpal backend's `pause()` is a purely local futex
store soundd is never told about. No change confined to soundd can separate the
two, and landing the soundd and SDK halves alone would kill every paused cpal
client. This is a case where the **spec**, not the implementation, is what needs
to change.

**(6) soundd never frees the per-client shm region.** `SharedMemory::Drop` only
unmaps and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB
page. **Bounded by the next process exit, not by soundd's lifetime** —
`cleanup_process` sweeps every region owned by an exiting process — so this is a
real leak with no release at close time, but not a permanent one. The entry
previously claimed the page was stranded for soundd's whole run, which
overstates it: a long-lived soundd accumulates only until whichever process owns
the region exits.

**ASSIGNED** to the isolation agent, merged with `SYS_GRANT_SHARED`'s missing
revocation: revoke and reclaim are one mechanism, and fixing either alone leaves
the other holding the same page.

**(8) FIXED at `4fce59c`** — `choose_params` (`virtio_sound.rs:62`) now selects a
rate and channel count the device actually advertises, and a device offering
nothing this driver implements logs *which* capability is missing and leaves the
machine to boot without audio, rather than being silently remapped to 44100/2.

> **CAVEAT — not verified by a QEMU boot.** This fix changes device negotiation,
> and seven consecutive boot attempts died in shared-toolchain contention. The
> reasoning that it still selects (44100, 2) on QEMU is *static*, read off an
> earlier boot log's advertised bitmaps. `cargo test -- audio` on a quiet tree is
> owed before this is treated as proven. Recorded as a live gap because an
> unverified change to negotiation is exactly the kind that fails on the one
> machine nobody booted.

**(10) REFUTED — the two TPDF dither draws are independent enough that nothing
can tell.** Kept rather than deleted: the measurement is the finding, and an
entry removed silently gets re-filed next year by the next person who reads
`rng.next() + rng.next()` on one `Xorshift32` state and assumes.

Measured over two million samples, one state stepped twice versus two
independent states:

| | variance (TPDF ideal 0.16667) | χ²/df vs triangular | lag-1 autocorrelation |
|---|---|---|---|
| one state, two draws | 0.16672 | 0.98 | −0.00048 |
| two independent states | 0.16652 | 0.63 | −0.00050 |

The joint distribution of the summand *pair* is where a deterministic
relationship would actually show, and it does not: χ²/df ≈ 1.00 with zero empty
cells at 32×32, 128×128 and 512×512, for both arrangements. The step function
decorrelates the two draws well enough that the pair is empirically
indistinguishable from two independent streams.

**Deliberately not "fixed anyway".** Changing the dither changes the captured wav
bit-for-bit, so it would perturb the audio gate to chase a defect nobody can
demonstrate. This project has been bitten specifically by gates that cannot fail
(`specs/metal-track-history.md`); spending the gate's sensitivity on a
non-defect is the same error wearing a tidier hat.

Two of the three lower-severity items are **FIXED at `4fce59c`**. The passthrough
gain was not a rounding nicety: decoding by 32768 and quantizing by 32767 meant
**32,703 of the 65,536 i16 values did not survive a round trip**, each off by one
LSB. Now 0, gated by an exhaustive host test over every i16
(`soundd/src/main.rs:1347`). `AudioInfo::as_bytes` no longer publishes
uninitialised kernel stack: the padding is spelled out as named fields with a
`const _` size assert, so omitting one is an E0063 compile error rather than a
convention someone can quietly break.

Still open: unknown audio device command bytes report success and do nothing.

**The kernel's byte-1 audio fd verb has no SDK caller.** `kernel/src/fd.rs`
still dispatches `1 => crate::audio::start()`, but suspend-on-idle deleted
`AudioDev::start()` from `toyos/src/device.rs`: the only PCM start left is the
implicit one inside `submit_buffer`, which is what makes resume a single
control verb inline with the first submit. Recorded rather than deleted,
deliberately — a dead-code sweep that removes the arm narrows the ABI, and
the syscall surface is a contract, not an implementation detail. Byte 0
(stop) is live; soundd calls it every suspend.

**Residual from the `069d158` fix:** the deferral predicate cannot distinguish
"mid-refill" from "stopped producing". `9ed8eda` closed most of it by releasing
soundd's read end of the client's signal pipe at the first period the client
delivers, so a dead client is now detectable — but the control thread only
notices when it next reads, and until then the stream stays `is_streaming()` and
the mix loop keeps deferring buffers for a producer that no longer exists.
Bounded harmlessly by `refill_floor_nanos`.

**`f32::round()` lowers to a `compiler_builtins` `roundf` call on the ToyOS
target, not `roundss`.** The quantizer calls it once per sample (256/period,
~344 periods/s ≈ 88k calls/s). SSE4.1 is universally present on the 2020+
hardware baseline, so enabling it in the target spec turns this into one
instruction; whether to widen the target's feature set is a separate decision.

**CLOSED — gate A's fast tier could fail a run on `drains` alone**, with an empty
gap histogram and zero underruns. The proportional-recovery fix (`91a653c`) had
already decoupled drains from harm; the owner's ruling of 2026-08-04 made the
fast tier's verdict harm itself — a mid-tone gap in the capture, or a period
soundd put on the wire with no client audio behind it (`AudioRun::harm`). The
three per-run ceilings are still measured, printed with every run's counters and
kept; what they feed is the thorough tier's `ceiling_runs` rate, unchanged.

Two things moved the other way in the same change, and neither is a loosening.
`underruns` was judged against a ceiling of 12-70 depending on config, so 40
periods of silence on the wire passed a run; it is now judged against zero, which
is what all 120 recorded runs measured. And a run where soundd printed no stats
window at all used to be a ceiling breach — which under a harm verdict would have
passed — so it moved to the instrument-broken set, fatal in both tiers. It was
also the one breach that could enter the thorough tier's sample as a run of
all-zero counters, i.e. as the best run ever measured.
