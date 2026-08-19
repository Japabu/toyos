---
status: open
kind: finding
opened: 2026-08-08
---

# `xhci_slow_connect` has a 1 ms margin, and it is what caught `one-rmw-per-log-line-cost-350ms`

`SLOW_CONNECT_NS` holds the ports empty for 0.3 s and the controller starts at
**0.296–0.311 s** on a quiet host, so the gate reds whenever anything moves boot
by ten milliseconds. That sensitivity is the reason the log-ring regression was
caught at all — no other gate in the suite noticed 350 ms — and it is also why
the test reds on a loaded host for no reason of its own. Its own message names
the fix (`widen SLOW_CONNECT_NS, not this gate`) and it belongs to whoever owns
`toyos_xhci`; recorded here from a landing gate, not fixed.

Distinct from `issues/build/parallel-tests-red-under-other-suites.md`'s
parallel-red class: that one is about *verdicts* that are
wall-clock margins on the **host** side, and re-running alone clears them. This
margin is inside the **guest's** boot, and running alone only moves it back
under the line by a few milliseconds.
