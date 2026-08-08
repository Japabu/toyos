---
status: open
kind: defect
opened: 2026-08-01
---

# Process isolation does not hold: what is still ungated

`be604ef` (2026-07-28) closed the headline. `SYS_PIPE_OPEN` now requires that the
caller created the pipe, already holds a descriptor for it, or holds a live
socket to the creator; `SYS_SOCKET_CREATE` must already hold both ends in the
right direction; `SYS_PIPE_MAP` is gated derivatively because it takes an fd
rather than an id. `tests/toyos-rust-tests/src/bin/abuse_pipe_owner.rs` is a real
exploit test — it sweeps ids 0..256 skipping its own, asserts every live foreign
pipe is refused, and asserts non-vacuity so it cannot pass by finding nothing.

**It is a relationship check, not a capability, and it has a stated residual:
a peer entitled to one of a creator's pipes is entitled to all of them.** A
compromised daemon can still walk its peer's other pipes. That is the part to
carry forward now that the alarming sentence is gone — a stopgap's known
remainder is exactly what gets forgotten once the headline is fixed.

Still ungated, in rough order of damage:

- `SYS_LISTEN` — no namespace, so the first process to claim a well-known name
  impersonates that service.
- `SYS_GRANT_SHARED` — **narrowed, not retired: an owner cannot withdraw a grant
  it has made.** Two of the three original clauses are closed by `e7d842f`:
  `grant` is owner-only (`shared_memory.rs:179-181` rejects a non-owner, so a
  grantee cannot re-grant) and the target must name a live process
  (`syscall.rs:1096-1102`). `abuse_shared_grant.rs` and `shm_release_reclaims.rs`
  cover both.

  **The "no revoke" clause survives, and this file briefly retired it by mistake
  — on `release`, which is a different operation.** `release` is a grantee
  dropping *its own* access: `sys_release_shared` passes
  `process::current_process()` (`syscall.rs:1128`) and no syscall lets a caller
  name another pid. `destroy` is owner-only but removes the region for everyone,
  which is not withdrawal of one grantee. **Nothing lets an owner revoke a
  specific grantee**, against its wishes, possibly while mapped.

  Deliberate, and currently sound: with `grant` owner-only, the set that can ever
  map is exactly the set the owner named, so revocation has no caller today.
  `specs/capability-handles-spec.md` §14.5 rejects unmap-others by name, and
  unmapping a running process's pages is a second instance of the
  `gpu::set_resolution` hazard — freeing memory while a consumer may hold
  pointers into it.

  **It stops being sound the moment the reachable set is no longer exactly what
  the owner named** — if delegation or re-grant is reintroduced, or when
  `SYS_HANDLE_SEND` makes a grant transferable. Revisit it then, not before.
- ~~`SYS_SET_KEYBOARD_LAYOUT`~~ — deleted. The kernel has no layout to gate:
  it delivers key transitions and userland translates.

`a88e4ee` gated the GPU present/cursor path, `SYS_AUDIO_SUBMIT`, the NIC
RX/TX path and `SYS_SET_RT_PRIORITY` on `device::is_owner`. Each of the above is
a one-line gate of the same shape, but they need a decision first: which of them
should instead fall out of capability handles
(`specs/capability-handles-spec.md`)? `device.rs` records five owner PIDs and,
until `device::is_owner` was added, nothing outside `release` ever read them —
this is a class, not an instance.

Those gates are exactly as strong as the claim and no stronger. `SYS_OPEN_DEVICE`
is itself first-come and ungated, so a process that beats the daemon to a device,
or claims it after the daemon dies, holds everything the claim unlocks — for
`DeviceType::Audio` that includes the RT band, which audio spec §9.4 wants to be
a privilege. "Gated" here does not mean "privileged".

What is no longer *also* true is that the claim leaks: a claim admits exactly one
descriptor now (`device::Claim`, `Descriptor::duplicate`), so racing the daemon
is the whole remaining attack rather than the cheap half of it.
