---
status: open
kind: defect
opened: 2026-08-08
---

# Four reds on a runner that are not the xHCI class and not the width

Run `31247206462`, each red again when re-run alone, none of them reproduced on
the dev host:

- `doom_sound_flood` — `timed out after 88s` alone, against 4–26 s here.
- `hda_client_stall` — `the ring arm: timed out`, and `timed out after 9s` alone.
- `metal_sim_null_audio` — `soundd did not present a null sink on a device-less
  machine`, in 4 s.
- `sshd_fail_closed` — red alone in 22 s, having taken 152 s in the phase.

Three of the four are soundd's, which makes them worth reading together rather
than one at a time. Nothing here is diagnosed; they are recorded so the next
green run cannot quietly be read as their absence.

**Two of the four are 0 of 5 on the current tree and one is closed.**
`doom_sound_flood` and `sshd_fail_closed` did not fire in the rate probe.
`metal_sim_null_audio` was 5 of 5 and is closed, together with
`hda_two_live_refused` — one question about how the two tests read the boot
console, and not one about soundd's device-less path, which was doing its job on
every one of those runs. `hda_client_stall` is the one still standing.
