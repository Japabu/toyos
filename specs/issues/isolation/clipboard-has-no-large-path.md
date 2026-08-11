---
status: open
kind: defect
opened: 2026-08-08
---

# The clipboard protocol has three numbers and no working large path

Filed out of the SDK IPC-framing entry when that closed; the framing fix did not touch this.

`Window::set_clipboard` (`userland/window/src/lib.rs:547`) sends `text.as_bytes()`
with no bound, while the compositor keeps only `MAX_KEPT_PAYLOAD` = 116
(`userland/compositor/src/client.rs:39`). So text between 117 bytes and
`MAX_FRAME_LEN` is **silently truncated to 116**, and text past `MAX_FRAME_LEN`
is refused. Three numbers, no agreement between them, and a truncation the
sender is never told about.

**The shared-memory route it should move to cannot work in that direction at
all.** The free `clipboard_set` (`userland/window/src/lib.rs:324`) allocates the
region and sends the token but never grants it; `shared_memory::map` requires
membership in `allowed` and only the owner may `grant`; and no syscall tells a
client its peer's pid. The compositor's map is therefore `PermissionDenied` by
construction.

Both halves want one decision: either the receiver allocates and grants (which
is what the paste direction already does), or a socket learns its peer's pid.
