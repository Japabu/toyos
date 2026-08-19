---
status: open
kind: finding
opened: 2026-08-10
---

# `[profile.toyos]` leaves incremental on, which turns rustc's MIR inliner off

Every `[profile.toyos]` in the tree says `inherits = "dev"` and none overrides
`incremental`, so cargo's dev default applies and incremental compilation is on
for every guest binary.

`rustc_mir_transform`'s inliner enables itself only when

```rust
2 => (sess.opts.optimize == OptLevel::More || sess.opts.optimize == OptLevel::Aggressive)
     && sess.opts.incremental == None,
```

(`rust/compiler/rustc_mir_transform/src/inline.rs:49`; `mir_opt_level` is 2
because `opt-level = 2`). The second conjunct is false here, so **the MIR
inliner does not run on any guest crate.** Only `#[rustc_force_inline]` survives.

Harmless today: LLVM inlines for itself, and the shipping images are what they
have always been. It is recorded because it is invisible — nothing in the tree
says the inliner is off, and the profile reads as if `opt-level = 2` bought the
usual optimisation set.

It becomes load-bearing the moment a codegen backend other than LLVM is
attempted. `rustc_codegen_cranelift` has no inliner of its own — it does not
call the one Cranelift gained in Wasmtime 36 — so with cg_clif and incremental
on, nothing inlines anywhere. Measured on a bare-metal cg_clif probe:
`CARGO_INCREMENTAL=0` visibly changes the generated code, folding four
`#[inline]` helpers into their caller that were four real calls with it on.

Not fixed here, and not obviously worth fixing: turning incremental off would
trade dev-loop rebuild time for an inlining pass that LLVM already performs, and
nobody has measured what that trade costs. What is owed is the measurement, not
the edit.
