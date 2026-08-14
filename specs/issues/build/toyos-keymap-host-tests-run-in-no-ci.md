---
status: open
kind: defect
opened: 2026-08-14
---

# toyos-keymap's host tests run in no CI workflow

`toyos-keymap/` is its own workspace with four test files
(`tests/{compose,detect,tables,translate}.rs`), and its manifest says
`cargo test` runs there on the host — but
`.github/workflows/host-tests.yml`'s directory list omits it, so a red
in dead-key composition, layout tables or layout detection reaches no
gate. `bcachefs/` also carries unit tests and is on no list either.

The fix is one list entry each in `host-tests.yml`. `tests/CLAUDE.md`'s
host-suite roster names both as of the commit that files this.
