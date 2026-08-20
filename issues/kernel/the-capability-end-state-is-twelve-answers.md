---
status: open
kind: track
opened: 2026-08-20
---

# The capability end-state is twelve answers, written before APIs answer them accidentally

From the external review of 2026-08-20, adopted by the owner: the tree is
making strong capability commitments, and postponing the conceptual model
lets dozens of local API decisions harden into architecture by accident. The
end-state is written down NOW — as this tracked document, since the tree
keeps no spec corpus — with each answer either **committed** (the tree
already enforces it; cite the site) or **open** (an owner ruling is owed
before any interface constrains it). The twelve questions, verbatim:

1. What constitutes authority?
2. Is every authority ultimately derived from a handle/capability?
3. Are PIDs and TIDs identity-only, or can naming one confer authority?
4. Can a process enumerate objects it lacks authority over?
5. What ambient authority intentionally remains, if any?
6. Can rights ever increase after delegation?
7. What authority is implicit in process creation?
8. Which object types are transferable?
9. Are threads intended to become independently controllable first-class
   kernel objects?
10. How is device authority delegated?
11. Are namespaces capability objects or ambient process state?
12. Is CPU time eventually an explicit schedulable/budget authority, or is
    process-level fair scheduling sufficient (the existing
    `cpu-time-is-a-band-and-not-a-reservation` track holds this one)?

Several have de-facto answers in the tree already (the root CLAUDE.md's
Capabilities paragraph commits to no registry, no connect-by-name, no
pid-as-authority; rights only shrink at `dup_narrowed`; init builds every
namespace from `system.toml`). The first work item on this track: an audit
pass that answers each question FROM THE CODE with citations, marks the
genuinely open ones, and puts those before the owner as a single ruling set.
Until each answer exists, an interface change that would answer one silently
is stopped and the question surfaced instead.

The same review's adjacent standing rules live here too:
- **Kernel-resident workers are a control-flow boundary, not a memory
  boundary** — each one (`usbd`, `iod`, and any successor) is audited
  periodically, exists only where independent blocking progress, fault
  containment of execution flow, or latency isolation requires it, and a new
  one needs explicit architectural justification; work moves to userspace
  when the IPC/wait machinery makes that an isolation gain rather than
  overhead.
- **PID-backed pseudo-processes for kernel workers** are pragmatic today;
  the moment that representation leaks misleading user-process semantics
  into policy, observability, lifecycle or APIs, identity/accounting
  separates from user-process semantics rather than preserving the
  abstraction for convenience.
- **The adversarial handle-lifecycle suite** the review lists — stale-handle
  behavior, rights reduction and duplication, cross-process transfer, table
  exhaustion, teardown with references in flight, and every path where a
  numeric identifier might become authority — is measured against what
  `handle_kill_policy`, `abuse_handle_table`, `handle_lifetime` and the
  census arm already cover, and the gaps become tests.
- **Zero-handle hooks stay mechanically constrained** — the drain sites'
  "no hook may take a sleep lock" doc constraint wants enforcement the
  compiler or an assert can see, per the review's finalization-path point.
