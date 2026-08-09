---
status: open
kind: defect
opened: 2026-08-09
---

# A userland line is several `write`s, so the kernel's log lands inside it

The console and the kernel's log ring are one byte stream. The kernel commits
each `log!` to it atomically and commits each userland `write()` atomically, so
the unit of interleaving is a **syscall** — and neither `eprintln!` nor
`println!` is one syscall.

- **`eprintln!` is one per format fragment.** Stderr is unbuffered by design, so
  `StderrLock`'s `write_fmt` is `core::fmt`'s default: a `write_str` per literal
  piece and per argument. `eprintln!("soundd: client {} removed", id)` is three.
- **`println!` is two.** `LineWriter` buffers the fragments, and the one
  carrying the newline makes it `flush_buf()` — the buffered prefix — and then
  `inner.write(lines)` for the rest (`library/alloc/src/io/buffered/linewritershim.rs`).

Measured on CI run `31271983043` (`main`, shard 10) and again on run
`31282019974` rep 10, the same line both times:

```
[kernel 11.410 cpu0 tid=0] memory: pid=6 peak=6MB allocs=3 frees=0
soundd: client [kernel 11.410 cpu0 tid=0] exit: tone pid=6 code=0 cpu=1ms
1 removed
```

**The collision is systematic and not unlucky.** soundd prints a client's
removal exactly when that client's process is exiting, which is exactly when the
kernel prints its four `syscalls:`/`memory:`/`exit:` accounting lines. Anything
that greps the console for `soundd: client N removed` then counts one of two —
`desktop_audio_client` waited out its whole 300 s liveness guard on it, at a
measured 1 run in 10.

`userland/soundd` writes its lines with one `write_all` of a whole line now, in
a local `say!`. **That is one crate of the 176 `eprintln!` sites in
`userland/`**, and every one of the rest can be split the same way — including
`compositor: ready` and `terminal: ready`, which are the ready markers the guest
suite waits on.

The fix belongs in `toyos/`, the userland SDK, because that is where "how a
program on this OS writes a diagnostic" is answered for everybody. It was not
put there here for a reason worth knowing: `toyos/src` is one of the four trees
in the content-addressed toolchain witness (`specs/ci-plan.md` §3), so touching
it rebuilds the sysroot — an hour on a runner, once — and `--pr` refuses a
branch that mixes it with other work.

Two things it is not. It is **not** Rust getting stderr wrong: unbuffered stderr
is what keeps a dying process's last words, and Rust is right about that on
every platform including this one. And it is **not** fixable in the kernel by
buffering the console per file descriptor — `/bin/shell` writes its prompt with
no trailing newline and expects to see it, so a line buffer down there would
hold the prompt forever.
