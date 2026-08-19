---
status: open
kind: defect
opened: 2026-08-03
---

# A landing's gate is a full suite, and a second suite on the host can time a boot out

`cargo run -- --land` runs `cargo test` inside the integration lock, so a
landing is a 14-minute suite. Nothing serialises it against a suite in *another*
worktree — the lock only serialises landings, and the host is still one host.

Measured 2026-08-03, `--land`'s own landing, with another agent's suite running:
`screen_fatal_halt` failed with `[qemu] Boot timed out waiting for ===READY===`
after 11 s, in a run where 237 of 238 passed in 850 s. The same test alone
passes in 3.3 s, and it had passed in 3 s in the same worktree's previous full
run when the host was quieter. The tell for the contention is in the run itself:
`screen_console_panic` took 39 s against 13 s in the quieter run, the same
binary and the same tree.

So this is the cost `issues/build/` predicts, now with an instance. It is left as an
observation rather than a rule because the fix is the counting semaphore `issues/build/`
already describes and nothing yet hands out slots. Until then a landing that
goes red on a boot timeout is re-run — the isolated re-run is the evidence,
exactly as CLAUDE.md's re-run-in-isolation rule says.

**Second instance, 2026-08-04, and it is not a boot timeout — which widens what
this costs.** `late_storage_connect` failed a landing gate at 20 s with "the
boot scan bound a disk, so the port was not held empty and this gate is
measuring an ordinary boot", in a run where 238 of 240 passed in 693 s; alone it
passes in 5 s. That test stages its disk from the *host* at a moment chosen
relative to the guest's boot, so contention does not merely slow it — it moves
the guest past the window and the staging lands in the wrong place. The test
caught that itself and refused rather than measuring an ordinary boot, which is
the only reason it reads as a red instead of a vacuous green. **A host-staged
timing window is the shape to look for when triaging a landing red, alongside
the boot timeout**, and `Sched::Serial` does not protect it: serial is one guest
per *test process*, and the contention is between processes.

**Third shape, same session, and this one has no mechanism yet.** The very next
landing gate — same branch, same tree, 238 of 240 again — failed a *different*
pair: `usb_flush_optional` with "read the image: No such file or directory" and
`usb_transport_break` with the same `NotFound` out of `tests/common/usb.rs:127`.
Both pass alone (8 s and 4 s). Both are a staged disk image missing from the
lane directory that the same test wrote it to.

What is established: `lane::dir()` is `$TMPDIR/toyos-tests-{pid}[/lane-N]`, keyed
on the *test process* id, and **nothing in the tree removes a `toyos-tests-*`
directory** — grepped, one hit, the constructor. So a second suite cannot be
deleting the first's scratch by name, and the obvious explanation is wrong. The
three shapes so far are a boot timeout, a host-staged window the guest slid past,
and an artifact that is not there; only the first two have a mechanism. Worth an
hour from whoever builds `issues/build/`'s semaphore, because "re-run it" stops being an
adequate answer once the failure can be a missing file rather than a slow one.

Method note, cheap and it cost twenty minutes here: **`pgrep -f "toyos-build
--land"` matches the waiting shell's own command line**, and another agent's
waiter too, so a wait-until-it-exits loop written that way never exits. Match on
`cargo run -- --land`, or count `[q]emu-system`.
