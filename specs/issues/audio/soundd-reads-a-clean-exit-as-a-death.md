---
status: open
kind: defect
opened: 2026-08-07
---

# Soundd reports a clean client exit as a death, 5 times in 44

Same capture. `tone` exits with `code=0` every time, and 5 of the 44 runs
produced:

```
[kernel 164.648 cpu6 tid=0] exit: tone pid=21 code=0 cpu=45ms
soundd: client 15 died, ramping down
soundd: client 15 removed
```

Clients 15, 17, 25, 34 and 39; the other 39 print only `removed`. The condition
is genuine — `signal_clients` (`main.rs:594-605`) got `NotFound` from the
signal pipe because the client process was gone — but it is a *race between
soundd's own two detection paths*, not a crash: whether the broken pipe or the
control thread's `RemoveClient` arrives first. Both set `pending_removal` and
start the same ramp, so no audio differs.

What is wrong is the word. §5.7's crash detector cannot distinguish a crash
from a clean exit that raced it, so "died" is a false positive at 11% of normal
disconnects — and it is the line an operator or a test would grep for. A
disconnect the control connection has *already* announced is knowable: the
control thread saw the peer close before the pipe broke.

Cosmetic today. It stops being cosmetic the moment anything gates on it.
