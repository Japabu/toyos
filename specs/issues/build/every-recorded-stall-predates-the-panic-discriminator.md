---
status: open
kind: finding
opened: 2026-08-17
---

# Every stall this project recorded before 2026-08-17 was decided by a wait that could not see a kernel panic

Until `tests/common/serial.rs` held one vocabulary, `run_test_paced` ended a run
on `KERNEL PANIC` and nothing else and `await_guest` ended one on silence alone.
Neither could see a kernel Rust `panic!`, a `DOUBLE FAULT`, a `MACHINE CHECK` or
an IOMMU DMA fault — all four halt every CPU, all four leave the guest silent,
and all four were therefore reported as *the guest stopped answering*. **So the
doubt is a property of the whole class and not of any one row**: a `STALLED:` or
`timed out after Ns` verdict recorded before that date is a claim about a wait
and never a claim about the guest.

This file is the survey, not the adjudication. Nothing below is re-decided; the
rows stay in `src/redlist.rs` as they were measured. What is owed is that a
stall now suspected of being a panic is *named* rather than left to be
rediscovered.

## How much of it can still be settled

CI keeps a run's logs for about 90 days, so everything from 2026-05 onward can
be re-read; nothing before it can. The dev host uploads nothing at all, so no
dev-host row on this list has any capture to go back to.

The other half of "is there evidence" is whether the failure message printed the
capture. Many do — the harness pastes `result.serial` under the sentence — and a
few do not, and for those the log's silence about a panic proves nothing.
`specs/issues/build/a-failure-message-drops-the-lines-before-the-test-started.md`
is why a pre-marker death is never in there either way.

**18,070 log lines across five CI runs were read for this, and exactly one
kernel panic was found under a verdict that named a wait.**

## Decided: it was a kernel panic

- **`sched_check_build`**, run `31946183485`, job `95162423932`, 2026-08-16 —
  `invariant P` at 200569 ns, `schedule_no_return` halting every CPU 1 ms later,
  reported as `STALLED: 382s of guard expired`. Already adjudicated:
  `specs/issues/kernel/the-check-build-guest-stopped-answering-on-kvm-twice.md`.
- **`hda_client_stall`**, run `31247206462`, `guest (2)`, 2026-08-08 — **new
  here.** The verdict was `FAIL hda_client_stall: the ring arm: timed out after
  117s`, and 24 s *before* that wait gave up its own capture carried

  ```
  [kernel 93.157 cpu1] LOCK CONTENTION: 500M spins at src/drivers/xhci/mod.rs:1786:26, ticket=719 now=718
  [kernel 93.157 cpu1] !!! PANIC !!!: panicked at src/drivers/xhci/mod.rs:1786:26:
  DEADLOCK at src/drivers/xhci/mod.rs:1786:26: 500M spins, ticket=719 now=718
  ```

  with a backtrace through `sched::driver::idle_loop` → `log_file::poll` →
  `Vfs::flush_file` → `Fat32::alloc_cluster` → `UsbBlockDevice::write_blocks` →
  `xhci::with_disk` → `Lock<Vec<RingId>>::lock`. The idle loop's log-file flush
  deadlocked against the xHCI disk lock. `src/redlist.rs` carries that row as
  *"`the ring arm: timed out`, and `timed out after 9s` alone. The one of that
  run's four that is still standing"*, and its write-up
  `specs/issues/hardware/four-runner-reds-unclassified.md` says *"Nothing here is
  diagnosed"* — the diagnosis was in the capture the whole time. Confidence:
  high, the panic is in the same printed capture as the verdict.

  Note the spelling: `!!! PANIC !!!:` does not contain `PANIC:`, so neither wait
  would have matched it even with the obvious patch. The kernel says `PANIC:`
  and a kernel-prefixed `panicked at` today, and the table classifies both.

## Cleared: the capture is in the record and carries no death of any spelling

- `xhci_hid_break`, run `31422708833`, shard 10, 2026-08-10 —
  `STALLED: 133s of guard expired … said nothing for the last 131s of it`.
- `xhci_flap`, run `31246245541` (KVM), 2026-08-08 — `timed out after 164s`; the
  capture ends at 3.851 s and nothing follows it. This is the row
  `specs/issues/hardware/xhci-flap-wedges-under-kvm.md` is built on, and the
  clearing *strengthens* its reading: the guest went silent without saying why,
  which is a wedge and not a panic.
- `blocked_dump` (329 s), `metal_sim_pointer_churn` (303 s),
  `desktop_audio_client` (354 s), `sshd_fail_closed` (155 s), `i8042_mouse`
  (118 s), all run `31250706113`, 2026-08-08.
- `doom_sound_flood`, `xhci_hotplug`, `metal_sim_null_audio`, `i8042_mouse`,
  run `31247206462` — the only panic in that run's 5451 lines is the one above.

## Undecidable: the verdict printed no capture and none was kept

- **`sched_check_build`**, run `31890991692`, job `95027203184`, 2026-08-15 —
  `STALLED: 259s of guard expired, and the guest had said nothing for the last
  259s of it`. Silent for the *entire* window and no capture at all, because
  `in_test` never became true. Permanently undecidable, and the reason it is
  filed separately.
- `metal_sim_client_death` (`timed out after 364s`) and `metal_sim_window_drag`
  (`timed out after 163s`), run `31250706113` — the failure arm prints no
  capture. `netd_connection_caps` (132 s), `i8042_health_cadence` (61 s) and
  `metal_sim_null_audio` (61 s) in the same and the neighbouring run are the
  same shape.

## Suspect, with nothing left to read

- **`desktop_window_child` / #156**, `specs/issues/kernel/desktop-window-child-freeze.md`.
  Its stated signature is *"a total freeze of the guest … the guest emits
  nothing at all"*, judged by the compositor's periodic stats line ceasing —
  which separates a stopped machine from a running one and **not** a stopped
  machine from a panicked one. No cause has ever been established, every
  occurrence was on the dev host, and nothing was uploaded. This is the largest
  open thing on the list and it is now self-answering: `await_guest` names a
  kernel death instead of saying it went quiet, so the next occurrence decides
  itself.
- The dev-host load family in
  `specs/issues/build/parallel-tests-red-under-other-suites.md` and
  `specs/issues/audio/wide-phase-reds-under-load.md` — `screen_console_scroll`
  at 598 s, `screen_early_panic`, the `metal_sim_*` reds at 300 s and more.
  `ALONE: GREEN` is *not* evidence against a panic: a panic reached only under
  contention does not reproduce alone either.

## What would close this

Nothing in the tree. The instrument is fixed going forward, and what is owed is
that the next `STALL` is read as a stall because a wait that can see a panic
said so, rather than because nothing was looking.
