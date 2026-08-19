---
status: open
kind: defect
opened: 2026-08-03
---

# `audio_tone_load (smp=1)` fails gate A's fast tier intermittently, on both trees

Six runs in one session, 2026-08-04, while fix bundle D was being gated. The
fast tier's own two-boot rule is what fails it: dropouts on the first boot *and*
on the confirming re-boot.

```
5408cfb, bundle stashed  RED   6 [1p 3p 5p 6p 11p 41p] /  67 of 1203,  then 1 [5p]      /   5 of 1144
5408cfb, bundle stashed  RED   5 [2p 3p 5p 19p 40p]    / 210 of 1336,  then 1 [5p]      /   5 of 1163
bundle D                 RED   8 [1p×2 2p×2 4p 8p 22p 57p] / 97 of 1231, then 3 [1p 30p 111p] / 142 of 1340
bundle D                 RED   3 [3p×2 10p]            /  16 of 1181,  then 2 [28p 40p] /  68 of 1231
bundle D, in a full suite GREEN gaps: none, wake_lat 5522us (0.24 pipelines), underruns 0/70
bundle D, twice more     GREEN
```

`audio_tone_load (smp=8)`, `audio_tone` at both widths, `audio_idle_suspend` and
`metal_sim_null_audio` were green in every one of the six.

**`smp=8` is not exempt — 2026-08-07, task #58's session.** It failed the same
two-boot rule twice, on a tree whose only kernel delta was the MSI-X unification
(boot-time register programming; the per-period path is untouched). A/B in one
session, `cargo test -- audio_tone_load`, HEAD against `main`'s tip merged into
it:

```
branch, in a full suite  RED  smp=8  1 [1p]/1118, then 1 [2p]/1124   wake_lat 76124us then 44159us
branch, alone            RED  both   smp=8 1 [3p]/1138, then 3/1131  wake_lat 102055us then 296659us
branch, alone × 7        GREEN both widths                            wake_lat 6543-45930us
main,   alone × 3        GREEN both widths                            wake_lat 6556-54197us
```

Both reds coincided with another worktree's suite holding all twelve guest
slots, and both carried wake latencies of 76-297 ms where every green run on
either tree sat at 6.5-46 ms — soundd not being scheduled, rather than a cost
per period. That is the same signature as `one-boot-put-142ms-of-silence-on-the-wire` and adds nothing to
the diagnosis; what it adds is that **the smp=8 config reds too**, so a future
A/B may not treat it as the quiet control.

**Two things are established and a third is not.** It is real harm — a gap in
the capture, on two boots running, four times. It is **not fix bundle D**: the
A/B alternated trees in one session on one host, and both sides are red with
overlapping totals. What is *not* established is a rate, or that the four reds
and the three greens differ in anything but when they ran; the host was carrying
three other agents' suites throughout, and the 15-minute load average was still
28 on 14 cores when the greens came in. Under the owner's ruling of 2026-08-04
that is not an excuse and not grounds to re-run it away.

It can fail a landing, since a landing's gate is a full suite. It is almost
certainly the same defect as `one-boot-put-142ms-of-silence-on-the-wire`, which had one boot and could not
be reproduced; this is four more boots of it, and the leads there apply.
Whoever takes it should get the rate first — the thorough tier
(`--audio-gate N`) is the instrument, and no single run of the fast tier can
say anything about how often this happens.
