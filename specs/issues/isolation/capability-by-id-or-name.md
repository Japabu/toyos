---
status: open
kind: defect
opened: 2026-08-01
---

# THE CLASS: an id or a name treated as a capability

Three separate defects in this file are one defect. A `PipeId`, a service name
and a `SharedToken` are all *designations* — they say which object you mean. None
of them says you are allowed to have it. Where the kernel accepted a designation
as authority, guessing or outliving the designation was the entire attack:

- **`PipeId`** — dense sequential integers, so `for id in 0.. { pipe_open(id, 0) }`
  walked every live pipe. Gated at `be604ef` (below).
- **A service name** — **the instance that motivated this class.**
  `Descriptor::Listener` held the service *name*, and every operation re-resolved
  it through the global registry, so nothing tied a descriptor to the listener it
  was created for. `listen("compositor"); dup(fd); close(fd)` freed the name while
  leaving the dup live: the real compositor's `listen` then *succeeded*, its own
  "already running" check passing, and from that moment the attacker's stale fd
  took connections meant for it. Three calls, no race, no privilege. Closed at
  **`e42532f`** (2026-08-01) by storing a `ListenerId` — never reused, so a
  removed id names nothing forever — with `abuse_listener_hijack.rs` as a real
  exploit test.

  Not closed by `be604ef`, which this file briefly claimed: `Listener(String)`
  is an unchanged *context* line in that commit's own `fd.rs` hunk. See the
  postscript at the end of this section.

  **The setup is gone too** (tasks #61/#170): a listener is refcounted by the
  descriptors naming it (`listener::ListenerRef`), so the `close(fd)` in that
  three-call attack unregisters nothing and the real compositor's `listen` is
  *refused*. What is left is a squat, which is the "no namespace" bullet in the
  next entry and a different defect. `abuse_listener_hijack.rs` now asserts that
  refusal; the `ListenerId` half is the second line and is no longer reachable
  from userland at all, because nothing can produce a descriptor whose listener
  is gone.
- **`SharedToken`** — a bare `u32` with no RAII and no ownership, still open
  (`specs/issues/design-debt/`).
- **A device claim** — same shape, closed by the same tasks. `dup` cloned
  `Descriptor::Keyboard`/`Framebuffer`/… as a plain value while `close` released
  the class unconditionally, so `open_device(d); dup(fd); close(fd)` freed the
  device for anyone to take *and* left the caller a working descriptor: two
  processes composing to one scanout, or one reading another's keystrokes.
  `device::Claim` is now a non-`Clone` token whose `Drop` releases the class, so
  `Descriptor` cannot be `Clone` either and `Descriptor::duplicate` cannot
  answer `Some` for those five variants — `dup`, `dup2` and a spawn `fd_map` all
  say `PermissionDenied`. `device_claim_lifetime.rs` is the exploit test.

The adjacent failure, same root: **a reference that outlives the object it
names.** `FileBacking` after an unlink is the live instance (below) — the
reference stays valid-looking while the thing it designates is freed and reused
underneath it. Guessing a designation and outliving one are the two ways a name
gets you something you were never given.

`specs/capability-handles-spec.md` exists to make both unrepresentable: a handle
carries rights, so possession *is* the authority and there is no id left to
guess; and it is a refcount on a kernel object, so the object cannot be freed
while a handle can still reach it. Until then, every new syscall taking a raw id
needs the first question asked, and every cached reference to a filesystem or
device object needs the second. **This is here to predict the next instance, not
to summarise the last four.**

> **Postscript, worth more than the entry it is attached to.** On 2026-08-01 this file
> briefly recorded the listener defect as already closed by `be604ef`, citing a
> doc comment and a type that were sitting in the working tree — the isolation
> agent's fix, written twenty minutes earlier and not yet committed. Read against
> `git show HEAD:kernel/src/fd.rs`, the descriptor still held a `String` and the
> attack still ran.
>
> **In a tree with six agents committing, the working tree is somebody's
> uncommitted opinion. `git show HEAD:<path>` is the arbiter.** A finding has a
> shelf life in *both* directions: it can go stale because the bug got fixed, and
> it can look fixed because someone's work-in-progress is on disk. Both cost a
> wrong conclusion here in one day. Method: `specs/spec-staleness-sweep.md`.
