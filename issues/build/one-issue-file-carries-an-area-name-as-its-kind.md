---
status: open
kind: defect
opened: 2026-08-22
---

# `issues/diagnostics/a-console-tag-is-composed-by-replacing-a-bracket.md` carries `kind: design-debt`, which is not a `kind`

Found doing the census `issues/build/the-swarm-is-not-yet-falsifiable.md` asks
for: `rg -o "^kind: .*$" issues/ --no-filename | sort | uniq -c` turns up 361
files across five values of `kind` (`defect` 222, `finding` 97, `track` 33,
`rejected` 8) plus one file with `kind: design-debt` — a value `issues/README.md`'s
frontmatter table does not define. `design-debt` is one of the ten closed
`Areas` (a directory name — `rg -n design-debt src/ tests/ kernel/` finds it
used only as a path fragment in citations, never as a `kind`), so this reads as
the area name typed into the wrong field rather than a deliberate sixth value.

Nothing currently reads `kind:` programmatically that would refuse it — the
`src/redlist.rs` gate only checks `Red::source`, not `issues/` frontmatter
generally — so this sat unnoticed. The file's own content (a workaround for an
ABI-split constraint, with a fix staged for whoever next opens `toyos-abi`)
reads as `kind: finding` under the README's own test — noticed in passing,
not yet urgent — but picking between `finding` and `defect` is a one-line
content judgment for whoever fixes this, not a bookkeeping default.
