---
status: open
kind: finding
opened: 2026-08-09
---

# Sixteen tinycc corpus cases build and nobody has asked whether they run

`NOT_RUN` (`tests/toyos.rs`) declares every corpus case the suite does not run,
and attempts each one to its declared stage on every run. Sixteen of them reach
`Stage::Built`: toyos-cc compiles them, toyos-ld links them, and the harness
then does not boot the result.

```
18_include  31_args  32_led  40_stdio  78_vla_label  79_vla_continue
103_implicit_memmove  107_stack_safe  109_float_struct_calling  112_backtrace
115_bound_setjmp  116_bound_setjmp2  122_vla_reuse  123_vla_bug
126_bound_global  132_bound_test
```

Fourteen are here because `C_SKIP` put them here, one at a time, each with a
stated reason that was a claim about link or run time — "needs FILE\* APIs",
"needs setjmp", "VLA codegen bug" — and **nothing in the tree had ever tested
any of those claims.** Several are now known to be wrong: `115_bound_setjmp`
and `123_vla_bug` were failing the Cranelift verifier rather than wanting a
feature, and both build since the `LocalStorage` split; `122_vla_reuse` built
before it.

**`78_vla_label` and `79_vla_continue` are the other two, and they are the two
worth answering first.** Both stopped the compiler until that same split and
both build now, so both are candidates to be tests the suite gains. There is a
prediction to check them against, and it differs: `78_vla_label`'s three
functions do nothing a heap VLA cannot do, so it should print its six lines and
pass. `79_vla_continue` asserts `addr[9] == addr[0]` five times over — tcc
reuses one stack address per iteration of a loop that declares a VLA — and a
heap VLA can satisfy that only by accident of the allocator, so `NOT OK` is the
expectation and the entry then becomes a decline with C99 VLA lifetime as its
reason. Neither was measured, because every guest boot on this host needs a
shared sysroot another worktree was holding.

What is not known for any of the sixteen is whether it *runs*. Each has an
`.expect` file, so the question is answerable: delete the entry and let the
harness discover it. The cost of being wrong is a guest slot and possibly a
hung lane, which is why this is filed rather than answered.

Answer it one case at a time. A case that runs and matches its `.expect` is a
test the suite gains; one that does not is an entry with a real reason at last.
