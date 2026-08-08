---
status: open
kind: defect
opened: 2026-08-07
---

# The syscall profile is 64 bins wide and the ABI reaches 96, so every audio, net, IPC and pipe call is missing from it

`ProcessData::syscall_counts` is `[u32; 64]` (`kernel/src/process.rs:552`) and
`syscall_dispatch` guards the bump with `if (num as usize) <
data.syscall_counts.len()` (`kernel/src/arch/syscall.rs:218`). `syscall_total`
at :217 is bumped unconditionally. `SYS_MMAP` is 63 — the last bin — and
`SYS_MUNMAP` is 64, the first one dropped. Everything above it goes with it:
`SYS_AUDIO_SUBMIT` 71 and `SYS_AUDIO_POLL` 84, `SYS_NIC_{RX_POLL,RX_DONE,TX}`
78-80, `SYS_LISTEN`/`ACCEPT`/`CONNECT` 85-87, `SYS_PIPE_{OPEN,ID,MAP}`,
`SYS_READ_NONBLOCK`/`WRITE_NONBLOCK`, `SYS_IO_URING_{SETUP,ENTER}`,
`SYS_EXIT`, `SYS_PROCESS_STATS`, `SYS_SET_RT_PRIORITY`.

The line therefore prints a total that is not the sum of its parts, silently.
From the 2026-08-07 desktop capture, doom:

```
syscalls: pid=6 total=33190 syscall_wall=3806ms 0=6129 1=4585 6=1 8=14727 9=4 10=3 13=891 14=3 38=4 39=1 40=2 41=2 49=1919 53=2 59=5 63=17
```

The bins sum to 28295 against `total=33190` — **4895 calls, 15% of the process,
invisible**. Every one of the 44 `tone` processes in the same capture reports
`total=28` with bins summing to 22; the six missing are its whole reason for
existing (connect to soundd, map the ring, exit).

Reproduce with any process that makes an audio or network call:

```
grep 'syscalls: pid=' <log> | awk '{t=0;s=0; for(i=1;i<=NF;i++){ if($i~/^total=/){split($i,a,"=");t=a[2]} else if($i~/^[0-9]+=[0-9]+$/){split($i,b,"=");s+=b[2]} } print t, s, t-s}'
```

This is the diagnostics roadmap's layer 1 — the layer that exists to answer
"where is time going" — and it cannot see the audio path at all. Whatever the
fix (size the array from the ABI's highest number, or make the array
`[u32; SYS_MAX]` with a compile-time bound), the silent `if <` is the defect:
a bin that cannot hold a number should refuse it by name or not exist.
