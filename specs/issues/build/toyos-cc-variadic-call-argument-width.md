---
status: open
kind: defect
opened: 2026-08-09
---

# A variadic call can pass an i32 where the signature wants an i64

`73_arm64` stops in the Cranelift verifier rather than by name:

```
failed to define function 'myprintf':
  Verifier(... context: "v96 = call fn9(v81, v94, v95)  ; v95 = 4",
           message: "arg 1 (v94) has type i32, expected i64")
```

The case is declined for a different reason — it is aarch64-specific and this
target is x86-64 — so nothing is owed on its account. The *stop* is ours: a
call whose argument was not coerced to the width its signature declares is a
codegen defect, and it is only visible here because this is the one corpus case
that reaches it.

`NOT_RUN` (`tests/toyos.rs`) declares `73_arm64` against that message, so
whatever fixes it will red the run and take the entry with it.

Unmeasured: whether any *supported* source can reach the same path. It is a
variadic call, and `myprintf` is one — the corpus's other variadic cases
(`159_va_list`, `160_global_variadic`, `200_variadic_float`) all build and run.
