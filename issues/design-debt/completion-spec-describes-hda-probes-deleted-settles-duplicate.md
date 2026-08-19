---
status: open
kind: finding
opened: 2026-08-17
---

# `completion-architecture-spec.md` §4.3 describes `hda_probe.rs` as live; the whole file is gone

`completion-architecture-spec.md` §4.3 names three files that each hold a
byte-for-byte duplicate of a `settles` busy-wait — `xhci/wait/mod.rs`,
`hda.rs` and `hda_probe.rs` — and separately names `hda_probe.rs`'s
`spin_until_ns` as one of two Class S sites whose enclosing function name is not
`spin_ns`.

`hda_probe.rs` does not exist anywhere in the tree:

```
$ find . -name hda_probe.rs
$ git log --oneline --all -- '**/hda_probe.rs' | head -1
d7e9be5 Delete the HDA probe estate: H0's diagnostic has answered every question it existed to ask
```

Two further things moved with it. First, `hda.rs`'s own local `settles`
duplicate (the one the doc cites as being byte-for-byte identical to
`xhci/wait/mod.rs`'s) is also gone — `kernel/src/drivers/hda.rs` now calls the
shared `crate::clock::settles` directly at its four settle sites; only
`spin_ns` remains as a local function. Second, `xhci/wait/mod.rs`'s `settles`
doc comment now says it explicitly: *"which this file used to hold its own
byte-identical copy of; what stays here is the bound this driver waits to,
which is the only part that was ever the driver's own."*

So the "three duplicate copies" claim in §4.3 is not stale by a line number —
it is describing a shape (three files, three duplicate implementations) that no
longer exists at all. The current tree has one shared implementation
(`clock::settles`) called from at least two drivers, and zero duplicates.

Filed as a finding rather than a defect because nothing misbehaves; it is a
planning document's premise that has been overtaken by cleanup work landed
after the document was last checked against the tree.

Found 2026-08-17 during a citation-accuracy pass over
`completion-architecture-spec.md`; verified at the tree's tip that day.
