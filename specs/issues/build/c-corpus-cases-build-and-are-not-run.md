---
status: open
kind: finding
opened: 2026-08-09
---

# Fourteen tinycc corpus cases build and nobody has asked whether they run

`NOT_RUN` (`tests/toyos.rs`) declares every corpus case the suite does not run,
and attempts each one to its declared stage on every run. Fourteen of them
reach `Stage::Built`: toyos-cc compiles them, toyos-ld links them, and the
harness then does not boot the result.

```
18_include  31_args  32_led  40_stdio  103_implicit_memmove  107_stack_safe
109_float_struct_calling  112_backtrace  115_bound_setjmp  116_bound_setjmp2
122_vla_reuse  123_vla_bug  126_bound_global  132_bound_test
```

They are here because `C_SKIP` put them here, one at a time, each with a stated
reason that was a claim about link or run time — "needs FILE\* APIs", "needs
setjmp", "VLA codegen bug" — and **nothing in the tree had ever tested any of
those claims.** Several are now known to be wrong: `115_bound_setjmp` and
`123_vla_bug` were failing the Cranelift verifier rather than wanting a feature,
and both build since the `LocalStorage` split; `122_vla_reuse` built before it.

What is not known is whether any of them *runs*. Each has an `.expect` file, so
the question is answerable: delete the entry and let the harness discover it.
The cost of being wrong is a guest slot and possibly a hung lane, which is why
this is filed rather than answered — the wave that found it closed the
visibility hole and stopped there.

Answer it one case at a time. A case that runs and matches its `.expect` is a
test the suite gains; one that does not is an entry with a real reason at last.
