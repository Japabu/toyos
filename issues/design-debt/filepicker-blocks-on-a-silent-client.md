---
status: open
kind: defect
opened: 2026-08-14
---

# filepicker blocks on a silent client — the compositor-stall shape, in the one server that never adopted the fix

`userland/filepicker/src/main.rs:471-474`: the serve loop runs
`acceptor.accept().expect(..)` and then a blocking `recv_header` on the
fresh connection, single-threaded. A client that connects and sends
nothing — or fewer bytes than a header — parks the filepicker for every
other client. This is the exact pattern the doctrine bans ("a server
never blocks on a client"), the same shape the compositor and netd were
cured of, and `userland/init/src/main.rs:70-78` documents as the bug it
removed.

It is also the one server on `system.toml`'s `serves` list using neither
`ipc::FrameRx` nor an equivalent non-blocking buffer (soundd rolls its
own; init, the compositor, netd and every surface host use `FrameRx`).
The fix is the established one: accept and the first frame are two
events, and a frame is buffered until whole before anything acts on it.

Secondary, same file: the `accept().expect("accept failed")` turns an
accept-path error into a filepicker abort rather than a refused peer.
