---
status: open
kind: defect
opened: 2026-08-06
---

# The total freeze now reproduces in QEMU, in about seven seconds

**This is the first reproduction of the freeze class.** Everything above was
read off T14 logs because nothing in the suite could stage it; `desktop_window_child`
(landed 2026-08-06 by the compositor track, for a different property) stages it
by accident and reliably.

`cargo test -- desktop_window_child`. `tests/desktopcase`, `Profile::Metal`,
`smp: 8`. Round 0 is clean: `/bin/snake` spawns, GUI+Q closes its window,
`exit: snake pid=7 code=0` and the shell answers `after-snake-0-zqjxk`. The next
round spawns `snake pid=9`, the shell echoes `/home/root> snake`, **and the
guest emits nothing further at all** — not the exit, not the shell, and not the
compositor's stats line, which had been arriving every ~2 s until that instant.
The harness drains every 200 ms for 20 s and appends nothing, which is why the
assertion's message body is empty: the emptiness *is* the evidence.

Ten attempts, ten reds, across four separate invocations; the round it dies in
alternates between 1 and 2, so what varies is the timing and not whether it
happens. The same tree was green once, on the landing gate that put the test in
(verified by ancestry: `ce3e09d` and `7a9e5c1` are both ancestors of that
branch's merge), so the single green is the outlier rather than the reds.

Why it matters beyond one red test: **the owner's unrecoverable freeze on
pulling the USB stick, where Ctrl+Alt+D produces nothing, is the same
signature** — no CPU reaching a scheduler pass. That one is only reachable on
his laptop and only through a photograph. This one is reachable under LLDB
(`gdb-remote 1234`, every CPU's state inspectable, `--debug` parks the guest),
which is a different order of cost. **Two cheap experiments nobody has run yet:**
press Ctrl+Alt+D at the frozen guest over QMP — that answers directly whether a
pass-dispatched dump can fire on a total freeze, which is the open design
question against the NMI proposal — and if it says nothing, attach and read the
eight RIPs.

It also blocks every landing until it is fixed, because it is on main and it is
not flaky.

**RESOLVED as a contradiction, 2026-08-07: the two agents were watching two
different failures, and the silence is no longer reachable through this test.**
`40ee9a6` found that `close_focused_window` waited on `windows=N` — a level
sampled every two seconds — with `serial_until`, which scans the whole capture,
so the previous probe's sample answered the wait instantly and the loop re-sent
GUI+Q at the speed of a QMP round trip. The second one closed the terminal's
window under the one it meant, and the three exits that followed were correct
behaviour. **That signature is three logged exits in 0.25 s; the one recorded
above is total silence with no exits at all**, so the fix did not explain the
silence, it removed whatever was being poked hard enough to produce it.

Ten runs of `cargo test -- desktop_window_child` alone on `8cfb6d8` after the
merge: **ten green, no silent guest**. So the silence is not reachable through
the shipped harness any more, and the LLDB-attachable reproduction the class
still needs is not this one. What was *not* tried, and is the cheap next step
for whoever wants it back: restore the old function's injection rate
deliberately — a burst of GUI+Q at QMP round-trip speed while windows are being
created and destroyed — as a test of its own rather than as a regression.
