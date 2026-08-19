---
status: open
kind: defect
opened: 2026-08-05
---

# A `winit` app spins forever when its window is closed, and never exits

Separate from `total-freeze-reproduces-in-qemu`, in the fork rather than the
tree, and **decided by reading rather than by a reproduction.** `snake` (pid 23)
had a real window —
the compositor reports `windows=2` and 86 frames per 2 s batch from 883.7 s —
which went away at ~896 s, leaving `windows=1`. The process never exited, and
it did not exit when the compositor itself died at 952.8 s either.

`winit-toyos/src/event_loop.rs:483-547` polls the window in an inner loop whose
**only exit is `None`**:

```
loop {
    match win.poll_event(0) {
        Some(Event::Close) => app.window_event(.., CloseRequested),
        ...
        None => break,
    }
}
```

Once the compositor drops the connection the fd is permanently read-ready at
EOF, so `Window::poll_event` reports ready, `recv_event`'s `recv_header` fails,
and it returns `Some(Event::Close)` — **every call, forever**
(`userland/window/src/lib.rs:449-458`, `:443-447`). The loop never yields
`None`, so it never reaches the `exiting()` check at `:570` that `exit()`
inside `CloseRequested` was supposed to trip. The app spins on a core instead
of leaving, which is also why nothing in the log marks the moment.

**CLOSED in the tree, and the fix is the SDK's rather than the fork's.**
`Window::poll_event` latches: `Close` is the last element of the stream and is
delivered exactly once, so a caller that drains until `None` gets out. That
makes the fork's loop correct as written, and it fixes every other client that
drains the same way rather than winit alone. `desktop_window_child` and the
fifth case of `compositor_client_death` gate it from the client side. A break
on `Close` in the fork's poll loop is prepared but unpushed (owner task #150);
it is now belt-and-braces rather than the fix.

**The T14 confirms it.** In `boot8-snake.log` the owner closed snake's window
with the X button and snake exited `code=0` — where before the latch it never
exited at all. The `ControlFlow::Wait` and `WaitUntil` arms at `:637` and
`:707` are the ones snake actually runs (`userland/snake/src/main.rs:351`,
`:359`), and they are now tested rather than read: snake leaves on a close both
on his machine and in `desktop_window_child`, three rounds, one of them played.

**What that session leaves open is not snake.** 34 ms after snake exited the
shell exited too, and 13 ms after that the terminal — so the owner got no
prompt back. That is `desktop-window-child-freeze`.
