---
status: open
kind: finding
opened: 2026-08-10
---

# Gate A's thorough tier on a runner compares against the dev host's sample, and needs its own

`tests/audio-baseline.toml`'s recorded sample was taken on the dev host under
cross-arch TCG. The thorough tier compares a fresh sample against *that*, so
`gate-a.yml` on a GitHub runner — KVM, four Azure cores, and the same QEMU
11.0.3 as the dev host ever since gate A moved out of a bare runner's apt 8.2.2
and into `ci.yml`'s `debian:sid` container, so that CI and the dev host differ
in the accelerator and nothing else — is comparing two instruments and calling
the difference a regression.

The measurement is run `31386117376`, `iterations=30`, tree `99e47d9`.
Shard 1 drew an AMD EPYC 7763, shard 2 an Intel Xeon Platinum 8573C; both were
quiet (`qemu 1-1`, `toyos-build 1-1` over 60 runs each). What differs:

- **`wakes` is a level difference in every config** — 1,310–1,416 fresh against
  779–990 recorded. A guest under KVM wakes about 1.6× as often as the same
  guest under cross-arch TCG. Nothing about that is a defect and no comparison
  across the two means anything.
- **`max_wake_lat_us`'s spread is comparable for `smp=1` and not for `smp=8`.**
  1.19× and 0.089× of the dev host's spread on the single-CPU configs; 3.37× and
  210× on the eight-CPU ones, where every boot prints QEMU's own `Number of SMP
  cpus requested (8) exceeds the recommended cpus supported by KVM (4)`.
- **No harm in any of it**: dropouts 0/60 in both jobs against a recorded 0/60,
  underruns 0 everywhere, and one drain in 120 runs.

So a runner-based thorough tier is possible for the single-CPU configs and needs
a `[runner]` sample of its own in `tests/audio-baseline.toml`. **This is the
sample**, kept here because the run's artifacts expire in 30 days and this is
one run of an instrument that took eighteen minutes to produce it.

`audio_tone.smp1` (AMD EPYC 7763):

```
max_wake_lat_us = [4533, 4859, 4948, 5054, 5234, 5303, 5319, 5377, 5650, 5661, 5730, 5840, 5979, 6002, 6107, 6121, 6159, 6271, 6316, 6489, 7182, 7696, 8108, 8149, 8240, 8293, 8517, 8579, 8585, 9779]
wakes = [1387, 1389, 1390, 1391, 1392, 1393, 1393, 1394, 1394, 1394, 1394, 1395, 1395, 1395, 1395, 1396, 1396, 1397, 1397, 1397, 1399, 1399, 1399, 1399, 1400, 1400, 1400, 1401, 1403, 1407]
underruns = [0 x 30]
drains = [0 x 30]
```

`audio_tone.smp8` (AMD EPYC 7763):

```
max_wake_lat_us = [4072, 6237, 6539, 6589, 6619, 6980, 6994, 7179, 7220, 7245, 7313, 7836, 8102, 8156, 8205, 8496, 8873, 9118, 9240, 9597, 9652, 9702, 9982, 9990, 10162, 10187, 10254, 10298, 10336, 11296]
wakes = [1373, 1374, 1375, 1381, 1383, 1386, 1388, 1391, 1391, 1392, 1393, 1394, 1395, 1396, 1397, 1397, 1397, 1398, 1398, 1398, 1398, 1399, 1399, 1400, 1400, 1401, 1402, 1402, 1403, 1407]
underruns = [0 x 30]
drains = [0 x 30]
```

`audio_tone_load.smp1` (Intel Xeon Platinum 8573C):

```
max_wake_lat_us = [3726, 3741, 3756, 3756, 3765, 3766, 3775, 3785, 3787, 3787, 3792, 3794, 3796, 3801, 3806, 3808, 3810, 3817, 3817, 3819, 3825, 3826, 3826, 3828, 3831, 3833, 3847, 3865, 3866, 3921]
wakes = [1395, 1395, 1395, 1397, 1397, 1398, 1398, 1399, 1400, 1400, 1403, 1404, 1404, 1405, 1406, 1406, 1407, 1407, 1409, 1409, 1410, 1410, 1410, 1411, 1411, 1411, 1414, 1414, 1415, 1416]
underruns = [0 x 30]
drains = [0 x 30]
```

`audio_tone_load.smp8` (Intel Xeon Platinum 8573C):

```
max_wake_lat_us = [6852, 7373, 7678, 8164, 8229, 8254, 8289, 8312, 8384, 8565, 8569, 8641, 9185, 9244, 9482, 9520, 10116, 10277, 10648, 10706, 10947, 11089, 11334, 12387, 12443, 13561, 13856, 96175, 157977, 357156]
wakes = [1310, 1329, 1367, 1374, 1375, 1377, 1379, 1379, 1384, 1384, 1387, 1392, 1394, 1394, 1395, 1395, 1396, 1396, 1397, 1397, 1398, 1398, 1399, 1399, 1399, 1399, 1402, 1402, 1404, 1407]
underruns = [0 x 30]
drains = [0 x 25, 1, 1, 1, 1, 1]
```

**Two things anyone recording this has to decide, and neither is a mechanical
edit.** The vendor is not selectable, so a `[runner]` sample is a sample over
two machines unless it names one; and one run is one sample of a spread, so the
number to record wants two or three more dispatches of
`gh workflow run gate-a.yml -f iterations=30` behind it. Writing it lands in
`tests/audio-baseline.toml`, whose own prose justifies every number in it.
