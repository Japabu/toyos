---
status: open
kind: finding
opened: 2026-08-17
---

# `log-architecture-spec.md` references `specs/plans/introspection-plan.md`, which `specs/README.md` forbids

`specs/README.md`: *"No references into `plans/`, `assessments/`, `issues/`,
a `CLAUDE.md`, or the source tree — a sibling spec may be referenced, and a
plan references its spec, never the reverse."*

`specs/log-architecture-spec.md` §14 (syscall numbering) discusses
`specs/introspection-plan.md` by name, at length — *"`specs/introspection-plan.md`
is wrong on today's tree and must be re-based"* — and §16's L8 row repeats the
same reference. The file is `specs/plans/introspection-plan.md` in the actual
tree, not `specs/introspection-plan.md`; the citation's path was also stale,
independent of the reference-direction problem, and was corrected in passing
during the citations-only pass that found this (the path had to resolve for
the format conversion to mean anything — the plan's own content, oddly,
had not drifted at all: `SYS_QUERY`, `SYS_LOG_READ` and `SYS_DISK_ADOPT` are
still at the exact lines this document originally cited).

The deeper problem the path fix does not touch: this document, living in
`specs/` proper, has a plan as a load-bearing dependency of one of its own
sections rather than the other way around. That is what `specs/README.md`'s
rule exists to prevent — a plan is staged intention and gets deleted on
completion, so a spec that leans on one inherits a citation that goes stale
the day the plan does, exactly as this section's own text (`re-based`,
`L8 edits that plan`) already anticipates and works around rather than
avoiding.

Found 2026-08-17 during a citation-format pass over
`specs/log-architecture-spec.md`. Not fixed here — resolving it means
either duplicating the syscall-numbering fact into `specs/log-architecture-spec.md`
itself or reversing the reference so the plan cites the spec, and both are
content decisions outside a citations-only pass.
