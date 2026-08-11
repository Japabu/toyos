---
status: open
kind: defect
opened: 2026-08-08
---

# Eleven names are red on CI, at a rate that is now measured

Supersedes *a runner reds a rotating handful every run, and the rate is
unmeasured*, whose whole ask was this run. `probe-rate.yml`, run `31258202923`,
tree `f8f73e1`: **five reps of the exact twelve-shard configuration `ci.yml`
runs** — same image, same accelerator, same `--jobs 1` — sixty jobs, **all sixty
finished**, 292 tests each, 1460 outcomes. **281 of the 292 names were green in
all five.** `specs/ci-plan.md` §9.1 has the per-shard clocks.

| test | red | shard | `Sched` | what it says |
|---|---|---|---|---|
| ~~`usb_transport_break`~~ | ~~**5/5**~~ | 6 | Serial | **CLOSED** — `specs/issues/hardware/`, a Bulk-Only Reset that raced the transfer it was recovering from |
| `std_unwind` | **5/5** | 10 | shared block | `exit code Some(-1)` — the #MF of `specs/user-machine-state.md` §2, whose fix landed after this probe and has not been re-measured on CI |
| `std_unwind_so` | **5/5** | 10 | shared block | the same |
| `metal_sim_null_audio` | **5/5** | 11 | Serial | soundd did not present a null sink on a device-less machine — **closed**: it always did, and the test read the line through a span of host wall clock |
| `hda_tone` | **4/5** | 4 | Serial | 1 mid-tone silence in the capture (`specs/issues/audio/`) |
| `late_storage_connect` | 2/5 | 7 | Serial | the boot scan bound a disk, so the port was not held empty |
| `hda_two_live_refused` | 2/5 | 2 | Parallel | `"presenting a null sink" never reached the boot console` — **closed** with it |
| `blocked_dump` | 2/5 | 3 | Parallel | two *different* reasons — the census half, and /bin/terminal racing the compositor |
| `dump_nmi_probe` | 1/5 | 2 | Serial | the rip resolved to `u128_div_rem`, not to the spin |
| `kernel_heartbeat` | 1/5 | 5 | Serial | 2 of 12 heartbeats dropped a healthy CPU from the mask |
| `usb_disk_index_stable` | 1/5 | 2 | Parallel | nothing enumerated on the first controller |

A twelfth name has been seen since and is not in the table because the probe did
not see it: `xhci_slow_connect`, red alone in run `31261669826`. It has its own
entry, `xhci-slow-connect-has-a-1ms-margin` — a 1 ms margin *inside the guest's
boot*, which is why running alone moves it by milliseconds and not by a verdict.

A thirteenth, with one sample each way on **one tree**: `desktop_audio_client`
stalled wide *and* alone in run `31264914759` and passed in run `31266194663`,
same commit, half an hour apart. It is 0 of 5 in the table's own probe, so this
is a rate and not a reproduction — but the capture is worth the note, because it
is #172's signature away from the T14: two clients connect, both tones say
`done`, and only one `client N removed` ever follows. The wait it blew is
`both clients to leave the mixer`.

**The top five reproduce, so they are defects and not a rate.** The bottom six
fire one or two runs in five, which is 20–40% and is not "noise" either: the bar
this was measured against tolerates one in fifty *with the failure named*, and
none of these six has been looked at. **No entry here is a candidate for
`EXPECTED_FAILURES`** — an exemption names a defect and a write-up, and "fires
40% of the time for reasons nobody has looked at" is neither.

**`metal_sim_null_audio` and `hda_two_live_refused` are the first two off this
table**, closed when soundd stopped racing to present its null sink. The remaining nine stand.

**Six of the eleven are `Sched::Serial`, and until 2026-08-08 the harness re-ran
none of them**: the retry loop was written for the parallel phase and branched on
the *run's* width. Half the list had no second sample at all, which is most of
why the earlier lists looked like they rotated.

**Twelve names came off the list when the wall-clock work landed**, and the
previous write-up's samples predate it. Run `31252989653` was `ab7f5d6`, which
does not contain `wt/toyos-clock` (`5b6e192`, and `1cf7fee`, `c546335`,
`02a3bc9`, `d50a8c9` under it). `metal_sim_client_death`, `metal_sim_window_drag`,
`metal_sim_pointer_churn`, `metal_sim_compositor_stall`, `desktop_audio_client`,
`desktop_typing_damage`, `doom_sound_flood`, `i8042_health_cadence`,
`sshd_fail_closed`, `xhci_hotplug`, `xhci_hid_break` and `screen_pager_keys` are
all **0 of 5** now, and the "a guest stops making progress and pays its whole
ceiling" shape with them. Anything read off a run older than `31254054628` is
about a different tree.
