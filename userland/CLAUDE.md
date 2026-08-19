# Userland

The module header at the site owns its subject — surfaces, translators and the channel in `toyos/`'s surface modules, soundd whole under `userland/soundd/`, what a process holds and how it got it at `kernel/src/object/` and `/bin/init`. The compositor's decisions are `toyos-desktop/`, pure and host-tested; `userland/compositor/` is devices, fds, shared memory and the panel. POSIX lives in `userland/libc` — ours, not a fork; that layer may be ugly, the kernel may not.

**A server never blocks on a client** — the doctrine no single site owns. Accept and the first frame are two events; a frame is buffered until whole before anything acts on it; a write is one `try_send` whose refusal drops the peer by name; a blocking read or write of a pipe the client owns is the same bug. init, the compositor, netd, soundd and every surface host use `ipc::FrameRx`. filepicker violates it today.

## Caveats that bite every agent

- **Local time is recovered, not asked for** — `SYS_CLOCK_EPOCH` is UTC and `SYS_CLOCK_REALTIME` is `h:m:s`, so the zone comes from subtracting them, and `toyos-wallclock` refuses the UTC+12..+14 band where two real zones fit one reading a day apart.
- **Nothing composes against the scanout** — reads from it miss every cache, which is why `window::Screen` has no read path. WC is weakly ordered: a blit ends with an `sfence` or the last partial buffer stays off the panel.
- **A diagnostic line is several `write`s, and the kernel no longer goes into the gaps** — `eprintln!` is still one syscall per format fragment, but a console object buffers its holder's line and emits it whole under one backend lock, so neither a kernel record nor another process's half-line can land inside one. `say!` is now about the *count* of syscalls, not atomicity. The bound is `MAX_CONSOLE_LINE`, 1024 bytes; a longer line is still emitted in pieces of it.
