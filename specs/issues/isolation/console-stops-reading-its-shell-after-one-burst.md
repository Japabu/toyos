---
status: open
kind: defect
opened: 2026-08-10
---

# `/bin/console` reads about seventeen lines of its shell's output and stops

Five tests are red on it and all five look like different bugs from the outside:
`screen_console_scroll`, `console_locale_detect`, `desktop_locale_detect`,
`desktop_audio_client` and `desktop_typing_damage`. They are one.

## What is measured

`screen_console_scroll` types `test_rs_test_screen_churn 0 100 7 240` at the
prompt. The guest's own accounting says the child did the whole job:

```
spawn: /bin/test_rs_test_screen_churn pid=3 …
syscalls: pid=3 total=24 syscall_wall=6ms 0=17 6=1 63=2 72=1 73=2 91=1
exit: test_rs_test_screen_churn pid=3 code=0 cpu=23ms
```

Seventeen `SYS_WRITE`s — the binary buffers into an 8 MiB `BufWriter` and flushes
every seven lines, so seventeen is exactly one hundred lines plus `CHURN-DONE` —
and it exited 0, so every `write_all` returned. The bytes are in the pipe.

The console painted `L0000` through `L0016`, **stopping in the middle of a
batch**, and then said nothing for the 21 seconds the liveness guard allows.
`CHURN-DONE 0 100` never arrived.

So the writer finished and the reader stopped. Seventeen lines is roughly one
ring's worth of the first flushes; what did not happen is the second wake.

## Why the other four are the same thing

- `console_locale_detect` and `desktop_locale_detect`: *"the console did not lend
  it the keyboard"* — the console stops answering its client's `surface` traffic.
- `desktop_typing_damage`: *"0 of the sixteen appearances the eight typed lines
  owe reached the console"*.
- `desktop_audio_client`: the same silence, on the boot that also runs a terminal.

Each is a compositor-or-console reader that serves one burst and then does not
wake again.

## Where to look

The reader side, not the writer. `toyos/src/poller.rs` and `kernel/src/io_uring.rs`
are what deliver a pipe's readiness, and chunk 6 of the endowment branch reworked
`io_uring::Source` — it holds an `Arc<PortShared>` and compares by `Arc::ptr_eq`
now, and `WatcherGuard` was deleted in favour of `io_uring::take_poll` as the one
removal path. An edge that is consumed and not re-armed is the shape that
produces exactly this.

**It is not the launcher and not the spawn path.** Before 2026-08-10 these five
failed earlier and differently — `test_rs_…: not found`, because `Command`
refused an undeclared program when the caller had transferred a connector — and
fixing that is what exposed this. The child now starts, runs and exits cleanly;
what does not happen is on the console's side of the pipe.

## How to drive it

`BootOptions { qmp: true }` on the console profile, type the churn command, and
interrogate the socket while the console is quiet: `human-monitor-command` with
`info registers -a` says whether the console's thread is halted-awaiting-interrupt
or spinning. Take that capture **before** injecting anything — a keystroke revives
a halted CPU and destroys the evidence.
