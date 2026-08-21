---
status: open
kind: finding
opened: 2026-08-15
---

# klogd_hosted pays two boots for one fast verdict, and only half of it is slow

`klogd_hosted` measured 11,805 ms on CI KVM — over `FAST_CEILING_MS`'s
hard ten-second line — and was relegated to Nightly by cost on the day
its `UNMEASURED` marker came back priced. But the verdict is two boots
glued together, and only the second is expensive: the first boots the
ordinary kernel and asserts klogd spawned with a process-table row that
`ps` and the census can name; the second boots the `klogd-panic`
actuator and asserts the deliberate panic halts the machine instead of
being recovered off a stale `syscall_rip`.

The spawn half alone is one cheap headless boot and belongs in the fast
tier: it is the per-PR signal that the kernel-thread machinery still
works, and relegating it as a rider of the panic half is exactly the
collateral `Why::RidesTheBootOf` exists to name — except here the two
halves share a registration, not a boot, so the split is free of the
group constraint.

The fix: two registrations — `klogd_hosted` (spawn + naming, Fast,
one boot) and `klogd_panic_halts` (the actuator boot, Nightly by cost).
The new fast registration needs the `UNMEASURED` bootstrap dance one
more time; the nightly half keeps the measured 11,805 minus whatever
the fast half turns out to cost. Until then, what still runs per pull
request: every boot's console output is klogd's drain, so the thread
starving or dying is visible in any test that reads a line.
