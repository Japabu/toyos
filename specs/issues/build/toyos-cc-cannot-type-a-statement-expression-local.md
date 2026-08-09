---
status: open
kind: defect
opened: 2026-08-09
---

# `expr_type` cannot see a local declared inside a statement expression

`89_nocode_wanted` stops at `codegen/expr_type.rs:27` with
`expr_type: unknown identifier 'i'`. The identifier is `kb_wait_3`'s
`int i = 1;`, declared inside a `({ … })` statement expression — a GNU
extension toyos-cc *does* implement, so this is a bug in something it has,
which puts it in charter.

`compile_expr` walks a `StmtExpr`'s items and registers each local as it goes.
`expr_type` does not: it answers a type question about the same expression
without ever entering the block, so a local the block declares is an unknown
identifier to it.

Do not carry the old reason forward. `C_DOES_NOT_BUILD` said "under `sizeof` in
dead code" and there is no `sizeof` anywhere in the file.

Fixing this is a fix inside `expr_type`'s handling of a statement expression's
own locals. Anything wider than that was declined by the toolchain wave, which
time-boxed it and did not get to it.
