---
status: open
kind: defect
opened: 2026-08-18
---

# `specs/plans/introspection-plan.md` allocates three syscall numbers that are all taken

Verified against `toyos-abi/src/syscall.rs` on 2026-08-18. The plan writes three
literals and every one of them collides:

| the plan says | the tree says |
|---|---|
| `SYS_QUERY = 97` | `SYS_DEVICE_REG_READ` |
| `SYS_LOG_READ = 98` | `SYS_DEVICE_REG_WRITE`; the real `SYS_LOG_READ` is **114** |
| `SYS_DISK_ADOPT = 99` | `SYS_ENDOWMENTS` |

It restates the three together in its own numbering section, so a reader who
checks one number and moves on finds the same three wrong in two places.

**The `SYS_LOG_READ` row is not just a stale number — its design is
superseded.** That plan's log cursor is a *byte* cursor with a span ring
tracking which bytes are kernel-origin, so that a follower's own output cannot
be fed back into what it reads. The call that shipped answers whole records off
a per-CPU ring, and userland output never enters that ring at all: there is no
origin to track and the loop the span ring exists to prevent is not expressible.
Its `log-follow` tool is still worth having and needs none of that machinery.

The plan was to be re-based when the log work landed and it was not. Whoever
opens it next re-bases the two surviving numbers against whatever is clean then,
and deletes the log cursor rather than re-basing it.
