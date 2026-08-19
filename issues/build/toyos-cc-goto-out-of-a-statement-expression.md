---
status: open
kind: defect
opened: 2026-08-09
---

# A `goto` inside a statement expression makes a block argument that does not dominate

`89_nocode_wanted` stops at `codegen/mod.rs`'s `define_function` with

```
failed to define function 'kb_wait_3':
  Verifier(... location: inst24, context: "jump block6(v16, v24)",
           message: "uses value v16 from non-dominating inst18")
```

`kb_wait_3` is

```c
(1 ? printf("timeout=%ld\n", timeout)
   : ({ int i = 1; goto label; i = i + 2; label: i = i + 3; }));
```

— a statement expression, in the dead arm of a conditional, containing a
forward `goto` over a use of its own local. `i` is a cranelift `Variable`, so
`cranelift-frontend` closes the SSA by giving the label's block a parameter,
and the value it passes comes from a block the jump does not dominate.

**Not the `LocalStorage` class.** That was a `Value` this compiler cached and
handed back in the wrong block, and it is fixed — `codegen/addr.rs`'s
`local_addr` re-materialises every address where it is asked for. This one is
an argument the *frontend* synthesized, from a control-flow graph we built, and
what is wrong is the graph.

The other half of this case is closed. `expr_type` could not see a local
declared inside a `({ … })` at all, because it answered a type question about
the block's value without ever entering the block; `Codegen::stmt_expr_scopes`
is that scope now, and `dominance.rs::a_statement_expression_types_its_own_locals`
is the gate. Do not carry the reason `C_DOES_NOT_BUILD` used to give — "under
`sizeof` in dead code" — forward: there is no `sizeof` anywhere in the file.

`NOT_RUN` (`tests/toyos.rs`) declares this case against the verifier error
above, so a third defect landing on it cannot hide under this one.
