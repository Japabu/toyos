# Userland

The law lives in the specs: `specs/input-architecture.md` (surfaces, translators, the channel), `specs/audio-subsystem-spec.md` (soundd, whole), `specs/capability-endowment-spec.md` (what a process holds and how it got it). The compositor's decisions are `toyos-desktop/`, pure and host-tested; `userland/compositor/` is devices, fds, shared memory and the panel. POSIX lives in `userland/libc` — ours, not a fork; that layer may be ugly, the kernel may not.

**A server never blocks on a client** — the doctrine no spec owns. Accept and the first frame are two events; a frame is buffered until whole before anything acts on it; a write is one `try_send` whose refusal drops the peer by name; a blocking read or write of a pipe the client owns is the same bug. init, the compositor, netd and every surface host use `ipc::FrameRx`; soundd carries its own equivalent; filepicker violates it today (`specs/issues/design-debt/filepicker-blocks-on-a-silent-client.md`).

## Caveats that bite every agent

- **Nothing composes against the scanout** — reads from it miss every cache, which is why `window::Screen` has no read path. WC is weakly ordered: a blit ends with an `sfence` or the last partial buffer stays off the panel.
- **A diagnostic line is several `write`s, and the kernel's log goes into the gaps** — `eprintln!` is one syscall per format fragment. soundd, init and netd carry a one-`write_all` `say!`; the rest of userland does not (`specs/issues/diagnostics/serial-console-has-no-line-atomicity.md`).
