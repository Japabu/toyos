---
status: open
kind: defect
opened: 2026-08-10
---

# A process cannot ask what it received, and using it wrongly is fatal

`SYS_HANDLE_RECV` installs a peer's handles in the receiver's table and answers
only *how many*. There is no syscall that says what any of them names, and
`KObjectRef`'s kind is not reported anywhere.

Every typed use of a handle resolves through `HandleTable::get::<T>`, and
`HandleError::WrongType` is one of the three kinds `HandleError::refuse` treats
as a bug in the caller: it ends the process, exit 139
(`kernel/src/object/handle.rs`). The doc argues that *"asking a pipe to accept a
connection is not something a correct program can do"* — which is true for a
handle the process made or was endowed, and **false for one a peer sent**. A
correct program cannot know.

So: **any process that receives a handle over a connection and then uses it
typed can be ended by whoever sent it.** The sender needs nothing but
`Rights::TRANSFER`, which every handle it made carries.

## Where it is reachable today

- `/bin/init`'s launcher takes `extra` connectors from a client and hands them
  to `SYS_NAMESPACE_BUILD`. That one is closed by making that call's `add` arm
  answer `InvalidArgument` rather than fault, and `launcher_refusals` gates it —
  but that is one call site, not the class.
- An audio client receives two handles from soundd and calls `SYS_SHM_MAP` on
  the first (`toyos/src/audio.rs`). A hostile *server* ends every client.
- A window client receives its buffer the same way (`userland/window/src/lib.rs`).

Nothing in the tree is hostile today, so nothing fails. The property the
architecture claims — that a process cannot be harmed by what it was not given —
does not hold across a transfer.

## Three ways out, and none is free

1. **Report the kind.** `SYS_HANDLE_RECV` writes `(RawHandle, kind)` pairs
   instead of bare handles. One ABI shape change, in-tree only — no fork names
   `handle_recv` (`specs/assessments/dependency-audit-2026-08-08.md`'s estate was swept for
   this branch and `handle_recv` is not in it). The receiver then refuses by
   name and the fail-fast policy is untouched.
2. **A fifteenth syscall** answering the kind of a handle. Cheaper to write and
   worse: it is a second place to ask, and it makes "what is this" a round trip
   rather than part of the answer that produced it.
3. **`WrongType` stops being fatal.** Rejected: it is fatal for a reason, and
   three quarters of its call sites really are a bug in the caller.

(1) is the recommendation. It is not this review's to take — it is an ABI
change, and `specs/capability-endowment-spec.md` §5's new syscalls were the
owner's to approve.

## Ruled not a merge blocker, 2026-08-10

Judged while clearing PR #22's blockers, with the reasoning written down because
the next reader will ask why a class this wide was left open.

- **The one instance a hostile *client* can reach is closed.** `/bin/init`'s
  launcher takes `extra` connectors from anybody holding a `launcher` connector
  and hands them to `SYS_NAMESPACE_BUILD`, and that call answers
  `InvalidArgument` for a wrong type rather than ending the caller. It is the
  one handle argument in the ABI that routinely crosses a trust boundary,
  `kernel/CLAUDE.md` says so where the policy is stated, and `launcher_refusals`
  gates it.
- **Every other instance needs a hostile *server*** — soundd sending an audio
  client its region, the compositor sending a window its buffer. A client whose
  server is hostile has already lost: that server chooses what the client sees,
  when it is answered, and whether it is answered at all. Ending it with a
  `WrongType` is not a new capability.
- **The fix is an ABI shape change and the ABI was the owner's to approve.** It
  widens `SYS_HANDLE_RECV`'s answer from `n` to `n` pairs, which is one of
  §5's new syscalls changing shape after the fact.

So it stays open, and what it is waiting on is a decision rather than an
instrument.
