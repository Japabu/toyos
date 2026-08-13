---
status: open
kind: defect
opened: 2026-08-01
---

# Process isolation: what is still ungated, and it is one question now

**Everything this entry listed is gone, and the one thing it deferred is due.**
Capability endowment deleted the registry, the pipe-id family, the shared-memory
ACL and the pid-keyed device gates: there is no `SYS_LISTEN` to squat, no name a
process can present, no `SYS_GRANT_SHARED` to be owner-only about, and no
`device::is_owner`. What is left is the clause this file wrote down as the thing
that would matter later, and later has arrived.

## Revocation

The old argument for having none was exact: *"with `grant` owner-only, the set
that can ever map is exactly the set the owner named, so revocation has no caller
today"* — and it named its own trigger: *"It stops being sound the moment the
reachable set is no longer exactly what the owner named — if delegation or
re-grant is reintroduced, or when `SYS_HANDLE_SEND` makes a grant transferable."*

`SYS_HANDLE_SEND` exists. A region sent to a peer arrives carrying
`Rights::TRANSFER`, because both move paths carry the source's rights unchanged
(`specs/issues/isolation/a-moved-handle-is-always-re-movable.md`), so the peer
may send it on and the owner is never told. The reachable set is no longer what
the owner named.

Two answers, and neither is this branch's to pick:

- **A right that does not carry.** A `MAP`-without-`TRANSFER` handle is what
  `specs/capability-endowment-spec.md` §6.3 already assumes soundd sends, and
  the rights model cannot express it because sending needs `TRANSFER`. That is a
  bound on delegation rather than a revocation, and it is the cheaper of the two.
- **Revocation proper**, which `specs/assessments/capability-handles-spec.md` §14.5 rejects
  by name: unmapping a running process's pages is the `gpu::set_resolution`
  hazard — freeing memory a consumer may hold pointers into. A capability system
  may refuse to hand out a new mapping; taking one back from a running process is
  a different thing.

## What is no longer here

`SYS_LISTEN`, `SYS_CONNECT`, `SYS_PIPE_OPEN`, `SYS_PIPE_ID`, `SYS_GRANT_SHARED`,
`SYS_RELEASE_SHARED`, `SYS_OPEN_DEVICE` and `SYS_SET_RT_PRIORITY` are retired
numbers. The RT band is `Rights::RT` on a `SysCap` the kernel mints once, so
"gated" and "privileged" are the same word now — which is what the last
paragraph of this entry used to say they were not.
