# The toolchain correctness wave

`toyos-cc/`, `toyos-ld/`, their host suites, `tests/testcases/` and the C-test
plumbing in `tests/`. Nothing else — this branch runs alongside two architecture
pipelines and its boundary is disjointness.

Every number and every root cause below was reproduced on this branch at
`19c761e`. The commands are in §2 so the next reader can re-take them.

## 1. The charter line this plan is sorted by

`toyos-cc` is a *minimal* C compiler that exists to bootstrap tinycc and compile
doomgeneric. "Not meant to grow" cannot mean "not meant to be correct", so the
line that sorts every item is:

> **A bug in something toyos-cc already implements is in charter. A feature it
> does not implement is out unless doomgeneric or tinycc needs it. Silently
> producing different code from what the source asked for is in charter
> whatever the feature — that is the `__attribute__` ruling, and it does not
> stop at attributes.**

The corpus is a *proxy* for correctness, not a requirement. A corpus case that
only a feature we have declined would satisfy is declined with it, by name.

## 2. Reproducing

```
cd toyos-cc && cargo build --bins                       # host binary, 14 s cold
./toyos-cc/target/debug/toyos-cc -c --target x86_64-unknown-toyos \
    -I userland/libc/include -I tests/testcases/tinycc \
    tests/testcases/tinycc/<case>.c -o /tmp/x.o          # one case
readelf -sW /tmp/x.o                                     # what it did or did not define
```

`readelf` is a host tool for reading, used here and by nothing in the tree; the
symbol claims below can be re-taken any other way.

For the two Cranelift verifier cases, printing the function is three lines at
`codegen/mod.rs:609` — `eprintln!("{}", cl_ctx.func.display())` before the
`panic!`. It is not committed; the dumps below were taken with it.

Whole-corpus compile sweep, the measurement several rulings rest on: 156 `.c`
files, one process each, **1.9 s**.

## 3. What the corpus actually does today

156 `.c` files. **24 fail to compile**:

| where the case is declared | count | is the reason checked? |
|---|---|---|
| `C_DOES_NOT_BUILD` (`tests/toyos.rs:812`) | 8 | yes — the set is asserted both ways |
| `C_SKIP` (`tests/toyos.rs:774`) | 15 | **no — never attempted** |
| companion file, excluded by the `+` rule | 1 (`104+_inline`) | n/a |

And the mirror: **17 of the 32 `C_SKIP` entries compile fine today.** Their
stated reasons ("needs FILE\* APIs", "needs system headers we don't provide")
are claims about link or run time that nothing has tested since they were
written. `18_include` and `40_stdio` were checked by hand and both compile.

`C_DOES_NOT_BUILD` carries eleven names. Eight fail at compile; three
(`85_asm_outside_function`, `96_nodata_wanted`, `98_al_ax_extend`) compile with
exit 0. The link failure the fixture records for each was not re-run here — that
needs the toyos sysroot — but its cause is in the object: the symbol the link
says is undefined is undefined in the `.symtab` toyos-cc wrote (§4.3, §4.4).

## 4. Root causes

### 4.1 `78_vla_label` — a cached SSA value used from a block that does not dominate it

Nothing to do with VLAs. `f2` declares `int a[1 && 1]` — a *constant*-size array
— inside a compound statement and `goto start` jumps into the middle of it:

```
block0:  jump block1
block2:  v0 = stack_addr.i64 ss0        ; the declarations, never reached
         v1 = stack_addr.i64 ss1
         v2 = stack_addr.i64 ss2
         jump block1
block1:  v6 = iadd.i64 v0, v5           ; a[0] = 0
```

`uses value v0 from non-dominating inst1`. `compile_local_decl`
(`codegen/stmt.rs:765-773`) emits one `stack_addr` at the declaration and caches
the resulting `Value` in `ctx.locals` as `LocalStorage::Ptr`. A `Value` is only
valid in blocks the defining instruction dominates; `LocalStorage::Spilled`,
which holds the `StackSlot` and re-materialises the address at each use
(`codegen/addr.rs:16`, `expr.rs:177`), has no such problem. The type is what
permits the bug: `Ptr(Value)` records a *result* where `Spilled(StackSlot)`
records an *origin*.

**Demonstrated.** Storing `Spilled(ss)` for fixed-size aggregates and teaching
`expr.rs:177` to return the address rather than load for an aggregate — two
edits, twelve lines — makes `78_vla_label` compile, keeps all 11 toyos-cc host
tests green, and changes the whole-corpus compile-failure set by exactly one
name (24 → 23; the two lists are otherwise identical). The patch was taken,
measured and reverted; it is not on this branch.

`LocalStorage::Ptr` has four producers and three of them have a
re-materialisable origin:

| producer | origin | re-materialisable |
|---|---|---|
| `stmt.rs:769` fixed-size aggregate | `StackSlot` | yes, `stack_addr` |
| `mod.rs:539` aggregate parameter copy | `StackSlot` | yes, `stack_addr` |
| `stmt.rs:854` static local | `DataId` | yes, `global_value` |
| `stmt.rs:744` VLA | a `malloc` return | **no** |

So the fix is a type split, not a patch at three call sites: name the origin and
the class stops being expressible. The one case that genuinely holds a computed
pointer is the VLA, and it gets an entry-block stack slot to live in — which is
what §4.2 needs anyway.

Two things the split has to decide deliberately rather than inherit:

- `expr.rs:1097` tests `matches!(ctx.locals.get(name), Some((LocalStorage::Ptr(_), _)))`
  and then loads *again* on top of what `compile_expr` already loaded, to call
  through a named function pointer. For a static-local `int (*fp)(void)` that
  reads as a double load. It compiles; whether it is right is a runtime question
  this host cannot answer without a guest. Once `Static(DataId)` is its own
  variant the test has to be rewritten, so this is the moment to answer it — with
  a C case and a boot, not by reading.
- Assignment (`expr.rs:820-855`) takes the `Spilled` arm for a scalar store and
  lets `Ptr` **fall through** to the memory path, which is what emits the memcpy
  for an aggregate. Merging the two variants without keeping that fall-through
  turns every struct assignment into a scalar store of the first word.

### 4.2 `79_vla_continue` — the same class, plus a VLA lifetime model that is wrong

```
block2:  v12 = call fn1(v11)            ; malloc, inside the loop body
         ...
block7:  call fn6(v12)                  ; free, after the loop
```

`uses value v12 from non-dominating inst13`. `ctx.vla_allocs` is a flat
`Vec<Value>` and `free_vlas` (`codegen/stmt.rs:320`) runs it at every `return`
and at the implicit one (`mod.rs:585`). Two defects in one dump:

1. **The IR is invalid** for the same reason as §4.1 — a `Value` from the loop
   body used after the loop. The type split fixes this half.
2. **The lifetime model is wrong.** The loop mallocs ten times and frees once,
   so nine allocations leak; and C says a VLA's lifetime ends when its block
   does, which `continue` is one way to reach. `free_vlas` is called from
   `compile_return` and from nowhere else — not from block exit, `break` or
   `continue`.

The test's own assertion is `addr[9] == addr[0]`: tcc reuses one stack address
per iteration. A heap VLA can only satisfy that by accident of the allocator.
Real C99 VLA semantics need stack allocation with a save/restore of the stack
pointer across the block, which Cranelift IR has no general `alloca` for.

**Ruling.** Half 1 is in — invalid IR is a compiler crash and the fix is free
with §4.1. Half 2 is out: neither doomgeneric nor tinycc needs VLAs, and
`122_vla_reuse` and `123_vla_bug` are already declined for the same subject. What
`79_vla_continue` does once half 1 lands is *measured* (§7, chunk T7) and the
case is declared against the measurement rather than the guess.

### 4.3 `85_asm_outside_function` and `98_al_ax_extend` — file-scope asm is silently discarded

Both compile with exit 0 and leave the symbols the asm defines undefined:

```
85: UND vide
98: UND _us  UND _ss  UND _uc  UND _sc
```

The path: `external_decl` gets no declaration specifiers, `direct_declarator`'s
catch-all arm (`parse/decl.rs:416`) turns the bare `asm` token into an anonymous
declarator, `skip_declarator_suffix` (`parse/decl.rs:360-371`) eats the
parenthesised text, and codegen skips a declarator with an empty name. Nothing
warns. `ast.rs:205` says as much in a comment — "parsed but inline asm not yet
emitted in codegen" — and inside a function body the same construct *does* stop
the compile (`codegen/stmt.rs:265`, `panic!("inline asm not implemented")`).

Emitting file-scope asm needs an x86-64 assembler. That is far outside the
charter and is not proposed. **The silence is the defect**, and refusing by name
is the fix — the same ruling `__attribute__` already got, applied at a door that
bypasses it.

### 4.4 `96_nodata_wanted` — not a compiler question at all

The whole file is `#if defined test_static_data_error / #elif …`, seven
configurations selected by a `-D` from tcc's own Makefile. With no `-D` the file
preprocesses to nothing: the object has two symbol-table entries, the null one
and the `FILE` one, and no `main`. Its `.expect` is a *concatenation* of seven
runs under `[section]` headers, four of which are expected **compiler
diagnostics** ("error: initializer element is not computable at load time").

The harness compiles one configuration per file and compares one stdout. No fix
to toyos-cc can make this case pass. It belongs with `60_errors_and_warnings`,
already declined as "meta-test for compiler errors". Declined.

### 4.5 The other six in the same fixture

Named for completeness — the task named five, the fixture carries eleven, and a
ruling that leaves six unsorted leaves the list mixed.

| case | first refusal | ruling |
|---|---|---|
| `83_utf8_in_identifiers` | `lex.rs:588` `unexpected character 'ï' (0xef)` | **out.** Non-ASCII *identifiers*. UTF-8 in strings and comments already works (checked). It stops on the byte it could not read, so nothing is silently dropped. |
| `89_nocode_wanted` | `codegen/expr_type.rs:27` `expr_type: unknown identifier 'i'` | **in, if cheap.** A bug in statement expressions, which toyos-cc implements: `expr_type` cannot see a local declared inside `({ … })`. Time-boxed in T7. |
| `94_generic` | `parse/expr.rs:420` `_Generic type dispatch is not implemented` | **out.** `03_struct` and `33_ternary_op` are already declined for the same missing feature; this one is in the other list for no reason but where it was noticed. |
| `95_bitfields` | `parse/attr.rs:134` `aligned … only to a struct or union definition` | **out.** A self-including bitfield torture test needing `#pragma pack`, `ms_struct`, `gcc_struct`, `aligned` on a declaration specifier and packed bitfields — every one of them a deliberate refusal. |
| `95_bitfields_ms` | the same file through a two-line wrapper | **out**, with it. |
| `99_fastcall` | `codegen/resolve.rs:665` `typeof: unhandled expression Unary(AddrOf, …)` | **out.** 32-bit x86: `pushl %esp`, `pusha`, `__attribute((fastcall))`. "No 32-bit" is a root principle; `73_arm64` is already declined the same way. |

### 4.6 Two more doors the `__attribute__` rule does not reach

Found while reproducing the above. Neither is in any issue file.

**`#pragma pack` is silently ignored.** `preprocess/mod.rs:339` handles
`push_macro`, `pop_macro` and `once`, and comments the rest away with
"Other pragmas ignored".

```c
#pragma pack(push,1)
struct s { char a; int b; };
#pragma pack(pop)
int sizes[sizeof(struct s)];
```

`sizes` comes out 32 bytes — `sizeof(struct s) == 8`, where the source asked for
5. A struct laid out differently from what the source says, with no diagnostic:
exactly what refusing `__attribute__((packed))` exists to prevent, through a door
that refusal does not watch.

Safe to refuse: `#pragma pack` appears **nowhere** in `userland/libc/include/`,
`userland/doom/include/` or doomgeneric, and in the corpus only in
`95_bitfields.c`, which is declined anyway.

**`asm("name")` after a declarator is silently dropped.** `skip_declarator_suffix`
eats it. `int alias_name(void) asm("real_name");` leaves `alias_name` undefined
and never mentions `real_name` — the rename is gone, so the link fails somewhere
else or resolves to the wrong symbol. `120_alias` is declined as "needs asm
aliases"; the silence is separate from the feature and is the part that is wrong.

Note the standard's asymmetry, which the refusal list has to respect: an
*unrecognised* `#pragma` is required to be ignored, so a hard error on every
unknown pragma is itself a defect. The rule is the attribute rule: refuse the
ones that change layout or linkage **by name**, accept a named inert set, leave
genuinely unknown pragmas alone.

### 4.7 The preprocessor exits the process

Three `process::exit(1)`: `#error` (`preprocess/mod.rs:309`) and a missing
include, system or otherwise (`:527`, `:530`).

`specs/issues/build/toyos-cc-preprocessor-exits-the-process.md` says "Every
other error in the crate returns". **That is wrong, and the fix depends on which
it is:** `toyos-cc/src` has **98 `panic!`s and zero `-> Result`**. The crate's
convention is to panic, the harness's mechanism is `catch_unwind`, and a
`process::exit` is the one error shape that defeats it. So the fix is to panic
like the other 98 — not to introduce a `Result` the rest of the crate does not
have.

This is load-bearing for the harness work. Three `C_SKIP` entries carry
"(calls process::exit, not catchable)" in their own comment —
`124_atomic_counter`, `125_atomic_misc`, `136_atomic_gcc_style`, all three
`#include <stdatomic.h>` — and any scheme that *attempts* the declined cases
kills the test process on the first one until this lands.

### 4.8 `toyos-ld`'s alloc-shim table is dead

`ALLOC_SHIMS` and `SHIM_NO_ALLOC_UNSTABLE` (`toyos-ld/src/collect.rs:1141-1161`)
are **nine** string literals carrying the rustc crate disambiguator
`Cs2fcwfXhWpkc` — four shim/target pairs and the `no_alloc_shim_is_unstable_v2`
name. The issue file says eleven; nine is what is there today.
Measured against the live sysroot, 30 rlibs of `x86_64-unknown-toyos`:

```
Cs2fcwfXhWpkc:   0 occurrences
CshVjSbrpHdcL: 108 occurrences
```

`synthesize_alloc_shims` therefore synthesizes nothing, and would go dead again
the next time `rust/` is rebuilt. The sweep also shows a **fifth** pair the table
never had — `___rust_alloc_error_handler` → `___rdl_alloc_error_handler` — which
is the second thing a frozen literal list cannot tell you.

Matching on the `___rustc` path and the function name with the disambiguator wild
is the fix the issue file already proposes, and it is right.

## 5. What the harness does and does not do

`c-test-compile-failure-is-skipped.md` is **half closed already** and the file
does not say so: `check_c_build_fixture` (`tests/toyos.rs:907`) asserts
`C_DOES_NOT_BUILD` in both directions and reds the run when the set moves. What
is left:

- **`C_SKIP` is asserted in neither direction and its 32 cases are never
  attempted.** 15 of them fail to compile and 17 compile; nothing knows which,
  nothing notices when one moves, and a name that no longer matches a file leaves
  a dead exemption behind.
- **The two lists mix the two questions.** `C_DOES_NOT_BUILD` holds five
  declines today (`83`, `94`, `95`, `95_ms`, `99`) and eight after T3, while
  `C_SKIP` holds decisions and defects side by side — so neither list answers
  "what is owed".
- **No entry points at an issue file**, though `docs.rs::every_named_issue_file_resolves`
  would check the path for free.
- **`tests/testcases/pp_tcc/` is read by nothing** — 25 preprocessor cases with
  `.expect` files, already committed, already attributed in
  `tests/testcases/LICENSE`. Measured through `toyos-cc -E -P` under tcc's own
  comparison (`diff -bB`): **21 pass, 4 fail.** Two of the four
  (`02.c`, `11.c`) are token *spacing* against tcc's output, inert for a
  preprocessor feeding our own lexer. One (`16.c`) is a blank line. One is real:

  `12.S` uses the GNU named-variadic form `#define SRC(y...)`. toyos-cc binds `y`
  to the first argument only, so `SRC(1: movw (%esi), %bx)` expands to
  `movw (%esi)` — **an argument is lost with no diagnostic.** The same case shows
  `9999b` lexed as `9999 b`, where C99 6.4.8 makes it one pp-number.

Attempting every declined case costs **under a second** on the 1.9 s sweep
measured in §2, so nothing here is bought with test time.

## 6. Ruling, item by item

| item | in / out | why |
|---|---|---|
| `78_vla_label` | **in** | cached cross-block `Value`; demonstrated fix, no regression across 156 files |
| `79_vla_continue` — invalid IR | **in** | same class |
| `79_vla_continue` — VLA lifetime and address reuse | **out** | C99 VLA semantics; no consumer needs them; joins `122`/`123` |
| `85`, `98` — emit file-scope asm | **out** | needs an assembler |
| `85`, `98` — stop discarding it silently | **in** | the `__attribute__` ruling at a door it does not watch |
| `96_nodata_wanted` | **out** | seven-configuration meta-test of compiler diagnostics; joins `60` |
| `83`, `94`, `95`, `95_ms`, `99` | **out** | §4.5; each already stops the compile with its reason |
| `89_nocode_wanted` | **in, time-boxed** | a bug in a feature we implement |
| `#pragma pack` silently ignored | **in** | wrong struct layout, no diagnostic |
| declarator `asm("name")` silently dropped | **in** | wrong or missing symbol, no diagnostic |
| preprocessor `process::exit` | **in** | prerequisite for the harness work |
| `toyos-ld` alloc-shim hash | **in** | dead table, and the shape re-rots on the next `rust/` build |
| packed bitfields | **out** | §8 |
| `__GNUC__` | **out** | §8 |
| `debug = true` produces no debug info | **out** | §8 |
| std-fork batch (#179) | **out** | bumps the shared `rust/` tree another pipeline is about to move |

## 7. Chunks

Each lands with the test that fails without it, at the layer that owns it. Host
tests throughout except where a guest is named — the whole wave is seconds of
test time.

**T1 — the preprocessor stops taking the process with it.**
Three `process::exit(1)` → `panic!`, carrying the file and line they already
print. Gate: a host test in `toyos-cc/tests/` that `catch_unwind`s a `#error`, a
missing `"quoted"` include and a missing `<system>` include and asserts each
message. Unblocks T5 and T6. Smallest chunk here and everything else can proceed
in parallel with it.

**T2 — a local's address is an origin, not a cached value.**
Split `LocalStorage::Ptr(Value)` into variants that name where the address comes
from: a stack slot, a static's `DataId`, and — for the VLA alone — a slot holding
the `malloc` result, cut in the entry block. `Spilled` merges into the slot
variant, branching on aggregate-ness where `Ptr` does today. Answer the
`expr.rs:1097` double-load question (§4.1) rather than porting it.
Gates: a host case per shape — `goto` into a block past an aggregate
declaration, `goto` past a VLA, a VLA declared in a loop body used after the
loop, a static local used from a block that does not dominate its declaration —
each of which must fail the Cranelift verifier before the change; plus the
whole-corpus compile-failure set losing `78_vla_label` and `79_vla_continue`
and gaining nothing — 24 names to 22. Only the first of those two is
demonstrated (§4.1); the second is what T2 is predicted to buy and T7 is where
it is checked.

**T3 — every door that changes layout or linkage is refused by name.**
One rule, three doors: file-scope `asm(…)`, `asm("name")` after a declarator,
`#pragma pack`. Accept a named inert pragma set with the reason ignoring it is
the same as obeying it; leave unrecognised pragmas ignored, as the standard
requires. Gate: `toyos-cc/tests/` — the existing `attributes.rs` is the model and
its assertion style is the one to copy — one arm per door, each asserting the
refusal names the construct, plus one arm proving an inert pragma still compiles.
`85` and `98` stop linking-with-a-hole and start refusing; they leave
`C_DOES_NOT_BUILD` for the declined list.

**T4 — `toyos-ld` matches the shim by its name, not by a compiler's hash.**
Match the `___rustc` path plus the function name with the disambiguator wild, and
carry the `___rust_alloc_error_handler` pair the frozen table never had.
Gate: `toyos-ld/tests/` — `ObjBuilder` already builds exactly the object needed —
a link whose only definition is `___rdl_alloc` under an **invented**
disambiguator, asserting the trampoline is synthesized. A test that hardcodes
today's hash reproduces the bug it is testing for, so the invented hash is the
point. Closes `specs/issues/build/alloc-shim-names-a-dead-compiler-hash.md`.

**T5 — one declared list over the whole corpus, asserted in both directions.**
`C_SKIP` and `C_DOES_NOT_BUILD` become one list whose entries answer two separate
questions — *is this declined or broken*, and *how far is it expected to get*.
Requirements rather than a type, because the shape is the reviewer's to judge:

- R1 one entry per non-running case; no case in two lists
- R2 every entry names a corpus file that exists — a renamed file takes its entry with it
- R3 every entry is attempted as far as its declared stage, every run
- R4 getting further than declared reds the run: the fix arrived, delete the entry
- R5 getting less far reds the run: a regression
- R6 a compile-stage entry quotes the refusal, so a second defect landing on the
  same case cannot hide under the first — the `EXPECTED_FAILURES::says` discipline
- R7 a declined entry carries its reason; a broken entry carries a
  `specs/issues/<area>/<slug>.md` path, which `docs.rs` already resolves for free

**Stage `Run` is not attempted.** Seventeen declined cases compile today and
nobody has asked whether they link or run; turning one on is a guest slot and
possibly a hung lane. The wave closes the visibility hole and files that question
as its own issue rather than answering it here.

**T6 — the preprocessor corpus gets an owner.**
`toyos-cc/tests/` over `tests/testcases/pp_tcc/`, 25 cases, driven through the
library — no guest, no new files, no NOTICE change. Compare under tcc's own
normalisation (`diff -bB`: blank lines and whitespace runs), with the same
declared-failure contract as T5. `02.c` and `11.c` are declined as token spacing
against tcc's output with the reason stated. `12.S` is a defect: decide between
implementing GNU `params...` and refusing it by name — silently dropping an
argument is the one answer that is not available. Same for `9999b`. Closes the
rest of `specs/issues/build/c-test-compile-failure-is-skipped.md`.

**T7 — measure, then declare.**
After T2: what does `79_vla_continue` do — build and print `OK`, build and print
`NOT OK`, or something else? Declare it against the measurement. Time-box
`89_nocode_wanted` (§4.5) to one session: a fix inside `expr_type`'s handling of
a statement-expression's own locals is in; anything wider is declined into the
T5 list with the reason. File the two new findings from §4.6 as issue files if
T3 does not close them, and file the "17 declined cases compile and nobody has
asked whether they run" question.

Ordering: T1 first and alone (it is minutes, and T5/T6 cannot be written on top
of a compiler that exits). T2, T3 and T4 are independent of each other. T5 needs
T1, T2 and T3 to have settled which cases belong where. T6 needs T1. T7 last.

## 8. Declined, by name, with the reason

**Packed bitfields** (`specs/issues/build/toyos-cc-no-packed-bitfields.md`).
Measured over every `.c` and `.h` in the pinned doomgeneric tree: of its 14
`PACKEDATTR` structs **none contains a bitfield**, and the only bitfield
declarations in the whole checkout are the four in `struct color`
(`i_video.h:142-145`), which is not packed. So doomgeneric does not need this
even once `PACKEDATTR`
starts expanding. The demand the issue names is `specs/wlan-plan.md` §10's 635
`__packed` uses in the AX210 subset, which is W6 and behind the doom milestone;
the issue itself says how many of those carry bitfields is unknown. And the
current behaviour is *refusal by name*, which is the charter-correct answer for
an unimplemented feature. Nothing here is a lie, so nothing here is this wave's.
tinycc's own source could not be checked — it is not in the tree — and the plan
says so rather than guessing.

**`__GNUC__`** (`specs/issues/build/toyos-cc-does-not-define-gnuc.md`).
Declined on the merits, which is what the task asked for. Defining `__GNUC__` is
a claim to implement GNU C, and toyos-cc stops on a long list of GNU constructs.
Some of them stop well — `__attribute__((cleanup))` answers

> `__attribute__((cleanup))` is not implemented by toyos-cc. Attributes it
> implements: packed, aligned. Attributes it accepts and ignores: unused,
> maybe_unused, noinline, noreturn, format, stdcall, fastcall, cdecl.

and `((constructor))` and `((alias))` answer the same way; `asm goto` and
`_Alignas` stop as parse errors instead, which is worse reading but still a
stop. Others do not stop at all, which is what §4.3 and §4.6 are about. Seeding
the macro turns on every `#ifdef __GNUC__` block in every header at once and
hands all of that to a compiler that will refuse most of it and silently
mis-handle the rest. That is the attribute ruling read backwards: the defect is
claiming a capability you do not have. The issue's own measurement says the
change buys nothing today — the fourteen `PACKEDATTR` structs move **no field
offset** and one size (`pcx_t`, 130 → 129), and `WritePCXfile` never takes
`sizeof(pcx_t)`. **Recommend
the issue be re-filed as `kind: rejected`, `status: none`** — it is an answer,
not work, and leaving it `open` keeps `rg -l '^status: open'` over-reporting.
That reclassification is the owner's to confirm.

**`debug = true` produces no debug info**
(`specs/issues/build/debug-true-produces-no-debug-info.md`). Keeping `.debug_line`
through `toyos-ld` is a feature the issue already records as not planned, and
"There is no DWARF anywhere" is a stated property of the debugging story. Out.

**The std-fork batch (#179)** — out by the branch's boundary: it moves the shared
`rust/` tree.

## 9. For the reviewer and the owner

1. **The task named five corpus cases; the fixture carries eleven and the corpus
   has 24 compile failures.** §4.5 and §5 sort all of them. If the intent was
   strictly the five, T5 shrinks and the other six stay mis-sorted between two
   lists — say so and I will cut it.
2. **`#pragma pack` and declarator `asm("name")` are new** (§4.6). Neither is in
   `specs/issues/`. Refusing them is a behaviour change to a compiler that
   currently compiles everything in the tree; the pragma sweep says it is safe
   here, and the risk is a header outside the tree that we have not seen.
3. **The `toyos-cc-does-not-define-gnuc.md` reclassification** (§8) is a
   `kind: rejected` ruling and needs the owner's word.
4. **T6's `12.S` decision** — implement GNU `params...` or refuse it — is the one
   place in this plan where "refuse by name" and "it is ten lines" point in
   different directions. Recorded rather than pre-decided.
