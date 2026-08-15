---
status: open
kind: defect
opened: 2026-08-15
---

# `toyos-cc` honours designated array indices for globals and silently ignores them for locals

`int a[6] = { [0] = 111, [5] = 222 };` compiles correctly at file scope and
incorrectly inside a function. There is no diagnostic.

The two aggregate-initializer paths diverge on exactly this. The global path,
`fill_init_aggregate` (`toyos-cc/src/codegen/init.rs:442`), reads the designator
before placing the element:

```rust
// toyos-cc/src/codegen/init.rs:451
if let Some(Designator::Index(expr)) = items[*cursor].designators.first() {
    if let Some(val) = self.eval_const(expr) {
        idx = val as usize;
    }
}
```

The local path, `compile_aggregate_init_cursor` (`:134`), has no such read in its
`CType::Array` arm (`:137-149`): `idx` starts at 0 and is incremented once per
item, so every initializer lands positionally. The `Designator::Index` matches at
`:177` and `:370` are in the *sub-member* designator chain and do not cover the
top-level array case. Struct and union `.field` designators **are** handled for
locals (`:159`), so it is the array-index case alone.

Demonstrated during the 2026-08-15 mechanism-consolidation audit by compiling
`int main() { int arr[6] = { [0] = 111, [5] = 222 }; return arr[5]; }` with
`--target x86_64-unknown-toyos -c` and reading `objdump -d`: `111` is stored at
stack offset 0 and `222` at offset **4** (index 1, the positional fallback)
instead of offset 20. The same declaration at file scope produces the correct
`.data` bytes — `0x6f` at offset 0, `0xde` at offset 20 — under `objdump -s -j .data`.

## Why it has not bitten

Nothing in the mandate corpus uses the construct on a local.
`rg -rnP '[{,]\s*\[' tests/testcases/tinycc userland/doom/doomgeneric userland/doom/src userland/libc/include userland/doom/include`
finds one true designated array initializer,
`tests/testcases/tinycc/130_large_argument.c:31-32` (`{ [200] = 1, [767] = 2 }`),
and it is a global, so it takes the correct path. The defect is dormant, not
absent.

## Why this is in charter

`toyos-cc` exists to bootstrap tinycc and compile doomgeneric and is not meant to
grow — but CLAUDE.md's rule for it is that a construct it does not implement is
**refused by name**, and *"dropping one silently is a miscompilation."* The
refusal discipline is otherwise strong here: `__attribute__`, `#pragma pack`,
file-scope and inline `asm`, and packed bitfields all panic by name with a host
test each, and `Designator::IndexRange` is refused by name at
`toyos-cc/src/codegen/init.rs:184` and `:494`. This is the one place a designator
is accepted and then dropped.

`specs/issues/build/toyos-cc-has-no-codegen-gate.md` is the standing entry for the
gap this lands in: four emitted shapes are asserted and *"everything else this
compiler emits is checked by nothing."* This is a third instance beside the two
miscompilations that issue already records.

## What a fix owes

Either honour the designator on the local path as the global path does, or refuse
it by name. Whichever is chosen, a fifth case in `toyos-cc/tests/emission.rs` —
the corpus happens not to exercise this, and determinism only compares a run
against itself, so neither would catch a regression.
