---
status: open
kind: defect
opened: 2026-07-31
---

# Every network client pays a second of boot retry on a machine with no NIC

`NetdConn::connect_blocking` (`toyos/src/net.rs:271`) retries `services::connect`
100 times at 10 ms. That is right when netd is merely slow to start and wrong
when it will never start: under metal-sim sshd sleeps 100 times, exits at
t=1.69 s on a boot that reached `Boot: complete` at 0.38 s, and its 100
`SYS_NANOSLEEP` calls are the whole of its accounting. Cheap fix: have netd
publish "no NIC" rather than not publishing at all, so the retry has something
to observe.
