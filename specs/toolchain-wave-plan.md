# The toolchain correctness wave

`toyos-cc/`, `toyos-ld/`, their host suites, `tests/testcases/` **except its
`system.toml`**, and the C-test plumbing in `tests/`. Nothing else — this branch
runs alongside two architecture pipelines and its boundary is disjointness.

Every number and every root cause below was reproduced on this branch at
`19c761e`. The commands are in §2 so the next reader can re-take them.

**Adversarially reviewed at `6aa006e`,** by re-taking every claim from a command
rather than re-reading it. Most held. Five did not and are rewritten against
what was measured: §4.1's demonstration (it miscompiles, and no gate here can
see that), §4.1's `expr.rs:1097` "runtime question" (it is a decided defect),
§4.6's safety sweep (one door of three), §5's preprocessor measurement (wrong
count, two wrong classifications) and T2's gate numbers (they assume an order
§7 did not state). Where the review used a host tool nothing in the tree uses —
`objdump`, `nm`, and the host `cc` as a reference preprocessor — the claim says
so and is re-takeable any other way.

**Two shared surfaces with `wt/toyos-endow`, which is implementing now.** Both
are avoidable and this plan avoids them by name:

- **`tests/testcases/system.toml` is not ours.** It is one of the eight test
  `system.toml` files that branch's chunk 8 rewrites (its §6.7a), and it sits
  inside the `tests/testcases/` boundary this plan claims. Nothing in this wave
  needs it — our work there is the `.c`/`.expect` corpus — so it is excluded by
  name. The rest of `tests/toyos.rs` is disjoint by line: this wave lives at
  `:774`–`:950`, that one at `:3863`–`:3886` and `:9074`.
- **The `CLAUDE.md` byte budget is one number and two branches spend it.**
  `TOTAL_BUDGET` is 80,000, the five files weigh **74,197** today, and chunk 9
  over there plans a root-file line against the same 5,803 bytes. So this
  wave's documentation change is a **replacement, not an addition**: root
  `CLAUDE.md`'s toyos-cc line says *"An attribute it does not implement is
  refused by name"*, and T3 widens that to a construct that changes layout or
  linkage. Same sentence, ~30 bytes, displaces nothing.

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

`readelf`, `nm` and `objdump` are host tools for reading, used here and by
nothing in the tree; every symbol and disassembly claim below can be re-taken
any other way. The host `cc` appears once, in §5, as a *reference preprocessor*
to settle what a conforming expansion is — it is a fact-check, never a
dependency, and nothing this wave builds may reach for it.

For the two Cranelift verifier cases, printing the function is three lines at
`codegen/mod.rs:609` — `eprintln!("{}", cl_ctx.func.display())` before the
`panic!`. It is not committed; the dumps below were taken with it.

Whole-corpus compile sweep, the measurement several rulings rest on: 156 `.c`
files, one process each. **Profile matters and the plan used to omit it** — the
harness builds `toyos-cc` at opt-level 3 (`[profile.dev.package.toyos-cc]` in
the root `Cargo.toml`), which is the number any test-time cost has to be priced
against. Re-taken at `6aa006e`, three reps each:

| | opt-level 3 | debug |
|---|---|---|
| all 156 | 1.47 / 1.71 / 2.12 s | 2.78 s |
| the 32 `C_SKIP` cases alone | 0.49 / 0.55 / 0.52 s | 1.09 / 1.03 / 1.01 s |

So the original **1.9 s** stands for the optimised sweep, and the cost of
attempting the 32 declined cases is **half a second** — at the harness's
profile, and only there. A debug binary prices it at twice that, which is what
the plan's first pass nearly rested on.

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

**One of the seventeen is a file, not a compile.** `compile_c` also compiles the
companion translation unit, and `104+_inline.c` panics at `parse/expr.rs:468` on
`__attribute__((weak))`. So `104_inline.c` compiles alone and `104_inline` fails
at the compile stage *as the harness reaches it* — 17 files, 16 harness passes.
T5 declares the stage, so it must declare this one against the companion.

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

**Demonstrated, and the demonstration is also the warning.** Storing
`Spilled(ss)` for fixed-size aggregates and teaching `expr.rs:177` to return the
address rather than load for an aggregate — two edits, twelve lines — makes
`78_vla_label` compile, keeps all 11 toyos-cc host tests green, and changes the
whole-corpus compile-failure set by exactly one name (24 → 23; the two lists are
otherwise identical). Re-taken at `6aa006e` and reverted; it is not on this
branch.

**And it silently miscompiles every struct assignment, which none of that can
see.** `expr.rs:838`'s `Spilled` arm now catches an aggregate and does a scalar
store, exactly as the second bullet below warns. Measured on
`struct S { long a,b,c,d; } x, y; y = x;`:

```
baseline:   mov $0x20,%edx ; call *memcpy          (UND memcpy in .symtab)
patched:    mov %r8,0x20(%rsp)                     (no memcpy at all)
```

`%r8` is the *address of `x`*, so the patch stores a pointer into the first word
of `y` and drops the other 24 bytes. The corpus goes 24 → 23 across that, the
11 host tests stay green across it, and the determinism suite stays green
because it compares run against run in one process and encodes no expected
bytes. **So "no regression across 156 files" is a claim about compile-time
reachability and about nothing else**, and T2 does not get to rest on it: the
gates it needs are in §7.

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

- **`expr.rs:1097` is a live miscompilation, not an open question.** It tests
  `matches!(ctx.locals.get(name), Some((LocalStorage::Ptr(_), _)))` and then
  loads *again* on top of what `compile_expr` already loaded, to call through a
  named function pointer. The plan used to say a guest was needed to decide it.
  A disassembly decides it. `static int (*fp)(void); fp = g; return fp();`
  against the same call through an ordinary local:

  ```
  static local:  mov (%rdx),%r8 ; mov (%r8),%r8 ; call *%r8
  plain local:   mov <g@GOTPCREL>,%rdi ;          call *%rdi
  ```

  The second load dereferences the function's own address, so the call goes to
  whatever the first eight bytes of `g`'s code spell. Only a *static* local can
  be a scalar `Ptr` — the other three producers are aggregates, and no aggregate
  is callable — so the arm is wrong in every case it can reach. Deleting its ten
  lines makes the two identical and leaves the whole-corpus failure set
  **byte-identical, 24 names, the same names**: no case exercises it, which is
  why nothing caught it. That deletion is T2's, not a separate item.
- Assignment (`expr.rs:820-855`) takes the `Spilled` arm for a scalar store and
  lets `Ptr` **fall through** to the memory path, which is what emits the memcpy
  for an aggregate. Merging the two variants without keeping that fall-through
  turns every struct assignment into one 8-byte store — and, once `compile_expr`
  answers an aggregate with its address, a store of the *source's address* into
  the first word of the destination. That is the miscompilation measured above,
  and the reason T2's first gate is an emission gate rather than another compile.

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
| `89_nocode_wanted` | `codegen/expr_type.rs:27` `expr_type: unknown identifier 'i'` | **in, if cheap.** A bug in statement expressions, which toyos-cc implements: `expr_type` cannot see a local declared inside `({ … })` — `kb_wait_3`'s `int i = 1;`. Time-boxed in T7. **Do not carry the fixture's own reason forward**: `C_DOES_NOT_BUILD` says "under `sizeof` in dead code" and there is no `sizeof` anywhere in the file. |
| `94_generic` | `parse/expr.rs:420` `_Generic type dispatch is not implemented` | **out.** `03_struct` and `33_ternary_op` are already declined for the same missing feature; this one is in the other list for no reason but where it was noticed. |
| `95_bitfields` | `parse/attr.rs:134` `aligned … only to a struct or union definition` | **out.** A self-including bitfield torture test needing `#pragma pack`, `ms_struct`, `gcc_struct`, `aligned` on a declaration specifier and packed bitfields — every one of them a deliberate refusal. |
| `95_bitfields_ms` | the same file through a two-line wrapper | **out**, with it. |
| `99_fastcall` | `codegen/resolve.rs:665` `typeof: unhandled expression Unary(AddrOf, …)` | **out.** 32-bit x86: `pushl %esp`, `pusha`, `__attribute((fastcall))`. "No 32-bit" is a root principle; `73_arm64` is already declined the same way. **T3 moves this refusal** — see §4.6. |

**One of those six quotes goes stale the moment T3 lands.** `99_fastcall.c:26`
and `:103` are file-scope `asm(…)`, and a *parse*-time refusal precedes a
*codegen*-time `typeof` failure whatever the line order — so after T3 the case
stops on the asm and `codegen/resolve.rs:665` is no longer what it says. T5's
entry must quote the refusal that exists when T5 is written, not this table's.
`120_alias` is unaffected and was checked: it stops at `parse/attr.rs:122` on
`__attribute__((alias))` at line 9, before its `__asm__(_"target")` at 19.

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

#### The safety sweep, for all three doors

The plan originally swept one door and T3 refuses three. Re-taken at `6aa006e`
over `userland/libc/{include,src}`, `userland/doom/{include,src}`, the pinned
doomgeneric tree (`fc60163`, 192 `.c`/`.h` files) and both corpora:

| door | outside the corpus | inside the corpus |
|---|---|---|
| `#pragma pack` | **none** | `95_bitfields.c:105,129` — already declined |
| file-scope `asm(…)` | **none** | `85:9`, `98:3`, `99:26,103` — all declined or moving there |
| declarator `asm("name")` | **none** | `120_alias.c:19,20` — already declined |

So every construct T3 refuses is absent from everything this project builds
today, and present in the corpus only in cases that are already declined. The
residual risk is a header outside the tree, which is what §9 says and all it
says. `_Pragma("pack(push,1)")` was checked as a fourth spelling of the first
door and is **not** a silent one: it is unimplemented and reaches the parser,
which stops.

**What T3 actually shipped is wider than this table, and the sweep was re-taken
over the wider set.** Two things moved. `_Pragma` stops, but on
`expected RParen, got StringLit` — a stop that names nothing, where the rule is
that a refusal names the construct; it is now refused in the preprocessor by
name, for five lines and no feature. And the pragma refusal is a list rather
than one entry: `pack` plus `ms_struct`, `gcc_struct`, `weak` and
`GCC visibility`, which is not an invented set — it is exactly the pragmas
whose `__attribute__` twin this compiler *already* refuses by name, and
refusing one spelling while dropping the other in silence is the very defect
§4.6 is about. Re-swept over the same trees plus `toyos-cc/include`, at T3:
**zero occurrences of any of the four extra names, and zero of `_Pragma`,
anywhere.** `#pragma comment(option, …)` has six occurrences — tcc's way of
passing itself compiler options — all in `95_bitfields` and
`60_errors_and_warnings`, both already declined; it is left ignored, because
what it changes is a *link input* and a missing one is a loud failure rather
than a silent miscompilation.

Every `asm` token in everything toyos-cc compiles, listed rather than counted:
`85:9`, `98:3`, `99:26,103`, `120_alias:19,20`, `127_asm_goto:3,12,29` — all
declined — and two in doomgeneric that are a Visual Studio project file and the
word "asm" in a comment.

**And the bootstrap target cannot be swept, which is not a reason to hold back.**
tinycc's own source is not in this tree and not in `forks.toml` — the
`bootstrap-cc` crate that once tried was deleted 2026-08-01, and compiling tcc
is stage 2 of `specs/posix-bootstrap-cost.md`, not started. If tcc's source does
use one of these three, a *named refusal* stops that bootstrap at the door with
the construct named. Today it silently drops the construct and the tcc that
comes out is miscompiled with no diagnostic anywhere. The refusal makes the
bootstrap fail better, so the unsweepable target argues **for** T3.

### 4.7 The preprocessor exits the process

Three `process::exit(1)` in the *library*: `#error` (`preprocess/mod.rs:309`)
and a missing include, system or otherwise (`:527`, `:530`). `toyos-cc/src` has
five in all; the other two are `main.rs:102` (a link error) and `main.rs:185`
(no input files), which are the binary's own and stay. A CLI exiting is not a
library denying its caller a choice.

The issue file this closes said "Every other error in the crate returns".
**That is wrong, and the fix depends on which it is:** `toyos-cc/src` has
**98 `panic!`s and zero `-> Result`**. The crate's
convention is to panic, the harness's mechanism is `catch_unwind`, and a
`process::exit` is the one error shape that defeats it. So the fix is to panic
like the other 98 — not to introduce a `Result` the rest of the crate does not
have.

This is load-bearing for the harness work. Three `C_SKIP` entries carry
"(calls process::exit, not catchable)" in their own comment —
`124_atomic_counter`, `125_atomic_misc`, `136_atomic_gcc_style`, all three
`#include <stdatomic.h>` — and any scheme that *attempts* the declined cases
kills the test process on the first one until this lands.

**Measured, both halves, rather than argued.** A throwaway host test that calls
`toyos_cc::compile` on all 32 declined cases under `catch_unwind`:

- before T1, the process dies at `124_atomic_counter` printing
  `fatal error: cannot find system include file: stdatomic.h` and the run reds
  with no verdict at all;
- after T1 — the three exits turned into `panic!` — all 32 are attempted and
  every failure is caught: **17 ok, 15 panicked**, at a **2 MiB** stack and at
  an **8 MiB** stack alike.

The second number closes a hazard nobody had priced: `main.rs` deliberately runs
on a 128 MiB stack "because TCC has deeply nested expressions", and the harness
calls the library on its own thread with no such stack. Identical results at
2 MiB and 8 MiB say no declined case is near that limit, so T5's R3 is safe on
the harness's thread. Two build details for whoever does T1: the message must
carry the file and line the `eprintln!` already prints, and `use std::{fs,
process}` becomes `use std::fs` or `[lints.rust] warnings = "deny"` reds the
crate.

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
never had — `___rust_alloc_error_handler` → `___rdl_alloc_error_handler`, 4 and
3 occurrences — which is the second thing a frozen literal list cannot tell you.
All three numbers re-taken at `6aa006e` against the same 30 rlibs.

Matching on the `___rustc` path and the function name with the disambiguator wild
is the fix the issue file already proposes, and it is right — **with the bound
the issue does not state.** The same sweep shows six more `___rustc` symbols that
are not alloc shims and must not grow a trampoline: `___rust_start_panic`,
`___rust_drop_panic`, `___rust_foreign_exception`, `___rust_panic_cleanup`,
`___rust_abort`, `___rust_probestack`. The wildcard belongs on the
disambiguator, never on the name — the pair table stays a closed list of five
and only its hash is loosened.

## 5. What the harness does and does not do

The C-test-visibility issue file is **half closed already** and does not say so: `check_c_build_fixture` (`tests/toyos.rs:907`) asserts
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
  `tests/testcases/LICENSE`, and named in no `.rs` anywhere in the tree.

Attempting every declined case costs **0.49–0.55 s**, three reps, at the
harness's own opt-level 3 (§2). Nothing here is bought with test time.

### 5.1 The preprocessor corpus, measured properly

**The plan's first pass at this was wrong in the count and in two of the four
classifications, and the invocation is why.** tcc's own pp harness runs from
inside the corpus directory and merges stderr into the compared stream
(`-E -P … 2>&1`, then `diff -bB`). Re-taken at `6aa006e` under exactly that
protocol: **25 cases, 20 pass, 5 fail** — `02`, `05`, `11`, `12`, `24`. Not
"21 pass, 4 fail". Case by case, and the reference column is the host `cc`
(§2's caveat: a fact-check, not a dependency):

| case | what it is | reference says |
|---|---|---|
| `02`, `05`, `24` | toyos-cc drops a space a reference preprocessor keeps — `2+(3,4)` for `2 +(3,4)`, and a lost leading space on a continued line | `cc` matches the `.expect` on all three, so this is **not** a tcc idiosyncrasy: toyos-cc's whitespace preservation differs from every reference. Inert for a preprocessor feeding our own lexer. **Decline, with that reason** |
| `11` | **the `.expect` is wrong, not the compiler.** `__NORETURN` is the Solaris `__sun_attr__` chain; C says it expands to `__attribute__((__noreturn__))`, `cc` emits exactly that, toyos-cc agrees, and the committed expect has nothing there. `tests/testcases/LICENSE` records `11.c` and `11.expect` as two of the files **this project modified** | **fix the expect.** The case then passes. Declining it as "token spacing" would have been a lie in a file whose whole job is to be checkable |
| `12` | the real defect, unchanged: `#define SRC(y...)` binds `y` to the first argument only, so `SRC(1: movw (%esi), %bx)` expands to `movw (%esi)` — **an argument lost with no diagnostic** — and `9999b` lexes as `9999 b` | `cc` reproduces `12.expect` character for character. §6.1 rules on it |
| `16` | **passes.** Its entire expected output is the diagnostic `16.c:3: warning: A redefined`, which toyos-cc writes to stderr | it is not "a blank line", and it is not a failure — it is a failure only if the harness looks at stdout alone |
| `23` | **passes**, and only because the source is named the way the expect names it: `23.expect` contains `40 "23.S"`, so `__FILE__` must be the basename | invoke it from the corpus directory, or name the source by its basename |

Three protocol requirements fall out, and T6 states them because **four of the
twenty-five verdicts move** if it gets them wrong — `16` and `23` on the first
two, `05` and `24` on the third: **capture the diagnostic stream with
stdout**, **name the source as the expect names it**, and **define the
normalisation rather than cite a tool.** That last one has teeth: `diff -bB`
means different things in different implementations, and the difference decides
`05` and `24`. Measured on this host's BSD `diff`, a leading space against no
space *is* a difference; GNU diffutils is not installed here, so the two cannot
be A/B'd and T6 must write the rule down instead of inheriting it.

## 6. Ruling, item by item

| item | in / out | why |
|---|---|---|
| `78_vla_label` | **in** | cached cross-block `Value`; demonstrated fix, no *compile* regression across 156 files — and §4.1 for what that does not cover |
| `expr.rs:1097`'s second load | **in** | a call through a static-local function pointer dereferences the function's own address (§4.1). Ten lines, deleted, corpus unmoved |
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
| `pp_tcc/11.expect` disagrees with the standard | **in** | §5.1: our own expect file, wrong; toyos-cc and a reference preprocessor agree against it |
| `12.S` — GNU `params...` | **out, refused by name** | §6.1 |
| `12.S` — `9999b` as one pp-number | **declared, not refused** | §6.1 |
| packed bitfields | **out** | §8 |
| `__GNUC__` | **out** | §8 |
| `debug = true` produces no debug info | **out** | §8 |
| std-fork batch (#179) | **out** | bumps the shared `rust/` tree another pipeline is about to move |

### 6.1 `12.S`, decided

The plan recorded this rather than deciding it. It is decided here, on a
measurement, and the two halves of `12.S` go opposite ways.

**GNU named-variadic `#define f(x...)` — refused by name.** Swept at `6aa006e`
for an identifier immediately followed by `...` in a macro parameter list, over
`userland/libc/{include,src}`, `userland/doom/{include,src}`, the pinned
doomgeneric tree (192 `.c`/`.h`), the 156-file tinycc corpus and `pp_tcc/`
itself. **One hit in the whole sweep: `pp_tcc/12.S:1`, the case that tests the
construct.** Zero consumers. The charter line this plan is sorted by then reads
straight off: a feature no consumer needs is out, and the *silence* — an
argument dropped with no diagnostic — is in whatever the feature. tinycc's own
source cannot be swept (§4.6), and that argues the same way: a named refusal
makes a bootstrap that needs it stop at the door, where today it would build a
miscompiled tcc. **"It is ten lines" is not an argument** — effort never is, and
here it cuts against, because "not meant to grow" is the crate's charter.

**`9999b` — declared, because "refuse by name" is not available.** A pp-number
is conforming C99 (6.4.8): `9999b` is one token and there is nothing to refuse.
Measured — `1e+5`, `0x1p-3` and `1.0f` all lex as one token, while `9999b` and a
pasted `12ab` split — so the lexer recognises numeric literals with known
suffixes rather than the pp-number production. Narrow: it can only surface where
the preprocessor's own text is the product, which for this compiler is a `.S`
file it never assembles. If the pp-number production turns out to be a small
lexer change, taking it is in charter — a bug in something already implemented.
If not, T6 declares it against the measured output with this reason. What is not
available is a third silent year.

## 7. Chunks

Each lands with the test that fails without it, at the layer that owns it. Host
tests throughout except where a guest is named — the whole wave is seconds of
test time.

**T1 — the preprocessor stops taking the process with it.**
The three *library* `process::exit(1)` → `panic!`, carrying the file and line
they already print; `main.rs`'s two stay (§4.7). Gate: a host test in
`toyos-cc/tests/` that `catch_unwind`s a `#error`, a missing `"quoted"` include
and a missing `<system>` include and asserts each message. Unblocks T5 and T6.
Smallest chunk here and everything else can proceed in parallel with it.

**T2 — a local's address is an origin, not a cached value.**
Split `LocalStorage::Ptr(Value)` into variants that name where the address comes
from: a stack slot, a static's `DataId`, and — for the VLA alone — a slot holding
the `malloc` result, cut in the entry block. `Spilled` merges into the slot
variant, branching on aggregate-ness where `Ptr` does today. Delete
`expr.rs:1097`'s second load rather than porting it — §4.1 settles what it does
and the corpus does not move.

Gates, and **the first of these is the one the demonstration proved was
missing**:

- **An emission gate, because the compile-failure set cannot see a
  miscompilation.** The demonstration patch turned every struct assignment into
  an 8-byte store of the source's address, and the corpus, the 11 host tests and
  the determinism suite were all green across it (§4.1). So T2 lands with host
  cases that assert on the *emitted object*, not on whether it compiled: a
  struct assignment still reaches `memcpy` with the struct's size, and a call
  through a static-local function pointer loads once. `toyos-ld` is already a
  dependency of `toyos-cc`, so reading back the symbols and the text of an
  object needs nothing new.
- A host case per shape — `goto` into a block past an aggregate declaration,
  `goto` past a VLA, a VLA declared in a loop body used after the loop, a static
  local used from a block that does not dominate its declaration — each of which
  must fail the Cranelift verifier before the change.
- The whole-corpus compile-failure set losing the names of this class and
  gaining nothing. Stated as a delta and no longer as "24 names to 22", because
  that absolute is only true if T2 lands before T3: T3 makes `85` and `98`
  refuse, which takes the set back up.

  **Measured when T2 landed: 24 → 20, and the prediction of two names was
  short by two.** `78_vla_label` and `79_vla_continue` went as expected, and
  so did `115_bound_setjmp` and `123_vla_bug` — both in `C_SKIP`, so neither
  the plan's sweep nor the harness had ever attributed their failure to
  anything, and both are the same `uses value vN from non-dominating instM`.
  Nothing was gained. That is the §5 hole arriving in person: a list nothing
  attempts cannot tell you what it is standing on.

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
carry the `___rust_alloc_error_handler` pair the frozen table never had. **The
wildcard goes on the disambiguator and never on the name** — the same sysroot
carries six `___rustc` symbols that are not alloc shims (§4.8) and none may grow
a trampoline; the pair table stays a closed list, now of five.
Gate: `toyos-ld/tests/` — a link whose only definition is `___rdl_alloc` under an
**invented** disambiguator, asserting the trampoline is synthesized, plus one arm
that a non-shim `___rustc` symbol gets none. A test that hardcodes today's hash
reproduces the bug it is testing for, so the invented hash is the point.
`ObjBuilder` builds exactly the object needed and is a private `struct` inside
`toyos-ld/tests/determinism.rs`, so a new test file cannot reach it: move it to a
shared module rather than writing a second one, or the two drift.
Closes the alloc-shim issue file, whose count of eleven was nine.

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
  — verified: `every_named_issue_file_resolves` walks `.rs` among its text
  extensions, so a path written into `tests/toyos.rs` is checked

**Where this sits against the `EXPECTED_FAILURES` idiom**, which is the house
answer to the same question and the thing to be judged against:

- R2/R4/R5/R6 are that idiom's `test`-name refusal, `Stale::OnAPass`, the
  both-direction assertion `check_c_build_fixture` already does, and `says`.
- **Every entry here is `OnAPass`-shaped and there is no place for
  `Stale::OnThisDate`.** That is the first thing a reviewer will ask and it has
  a reason: `OnThisDate` exists because one green of an *intermittent* test is
  one sample. A host compile is deterministic — 17 ok and 15 panicked came back
  identical across every re-run and across two stack sizes — so one green here
  is the whole population and R4 can be the strong form everywhere.
- `ExpectedFailure` requires a **`task`**, on the rule that "an expected failure
  nobody is assigned to is a disabled test". This list has no task field on
  purpose: R7's issue path answers the same question for the broken half, and a
  *declined* entry is not owed to anybody by construction. Say that in the code,
  so the omission reads as a decision.

**Stage `Run` is not attempted.** Seventeen declined cases compile today and
nobody has asked whether they link or run; turning one on is a guest slot and
possibly a hung lane. The wave closes the visibility hole and files that question
as its own issue rather than answering it here. Note `104_inline` when writing
its entry: the file compiles and the *stage* does not, because `compile_c` also
compiles the companion (§3).

**Cost, measured rather than assumed** (§2, §4.7): attempting all 32 costs
0.49–0.55 s at the harness's profile, all 32 are catchable once T1 lands, and
none is near the harness's stack. R3 is affordable and safe; both were open
questions before this review.

**What T5 found when it landed**, all of it invisible to the two lists it
replaces. The declared list is **41** entries and every one is attempted every
run: **22 refused at compile, 5 refused at link, 14 that build**. So the
"seventeen compile" of §3 was 17 before T2 and is 19 after it — `115_bound_setjmp`
and `123_vla_bug` were the same non-dominating-`Value` defect and neither said
so — of which 14 build and link and 5 do not. The five that do not link name
`main` twice (`60`, `96` — both meta-tests), `PTHREAD_PROCESS_SHARED`, `f_1`
and `__builtin_abort`, and no list in the tree had ever recorded any of that.
Several *stated reasons* were simply wrong: `03_struct` said `_Generic` and
stops on `__attribute__((cleanup))`, `123_vla_bug` said "VLA codegen bug" and
builds, `112_backtrace` said "needs tcc_backtrace" and builds.

The whole pass — 41 compiles and the links behind them — costs **0.43/0.44/0.43 s**
at opt-level 3, three reps, which is less than the plan's estimate for 32
compiles alone.

**T6 — the preprocessor corpus gets an owner.**
`toyos-cc/tests/` over `tests/testcases/pp_tcc/`, 25 cases, driven through the
library — no guest, no NOTICE change. Same declared-failure contract as T5.
§5.1 is the measurement it is written against, and three of its requirements are
protocol rather than policy:

- **The compared stream is stdout *and* the diagnostics.** `16`'s entire
  expected output is a warning; a test that reads stdout alone reds it forever
  for no defect.
- **The source is named as its `.expect` names it** — `23.expect` contains
  `40 "23.S"`, so `__FILE__` must be the basename.
- **The normalisation is written down, not cited.** "`diff -bB`" is a tool, and
  which tool decides `05` and `24`. State the rule: blank lines ignored, a run
  of interior whitespace equal to any other run, a run against nothing *not*
  equal. That is this host's BSD `diff` and it is what §5.1's verdicts were
  taken under.

Declared failures after that: `02`, `05`, `24`, as spacing against every
reference preprocessor and inert for one feeding our own lexer. `11.expect` is
**corrected**, not declined — it is our own file and it disagrees with the
standard (§5.1). `12.S` is §6.1's ruling: refuse `params...` by name, declare
`9999b`.

**One thing the normalisation rule had to settle that §5.1 did not name.** Its
three clauses decide `05` and `24` on a *leading* run against nothing, and
`16` on a blank line that moved — but `18` has one *trailing* space its
`.expect` does not, and BSD `diff -b` ignores that where the rule as stated
would not. So the rule carries a fourth clause: trailing whitespace is dropped,
because nothing downstream of a preprocessor can see it. With it: 20 pass and 5
fail before `11.expect` is corrected, which is §5.1's number, and 21 and 4
after.

**And the gate drives the binary rather than the library**, which the chunk
list did not intend and case `16` requires: the diagnostics that must be
compared alongside the output are `eprintln!` on the process's own stderr, and
capturing those in-process needs a file descriptor the library does not offer.
Still host-only, still under a second, and it is what makes "the source is
named as its `.expect` names it" expressible — the binary runs with the corpus
directory as its cwd. Closes the rest of the C-test-visibility issue file, which T5 had already
reduced to this half.

**T7 — measure, then declare.**
After T2: what does `79_vla_continue` do — build and print `OK`, build and print
`NOT OK`, or something else? Declare it against the measurement. Time-box
`89_nocode_wanted` (§4.5) to one session: a fix inside `expr_type`'s handling of
a statement-expression's own locals is in; anything wider is declined into the
T5 list with the reason. File the two new findings from §4.6 as issue files if
T3 does not close them, and file the "17 declined cases compile and nobody has
asked whether they run" question. **Two more the review found and this wave does
not close**: `9999b`, if T6 declares it rather than fixing it (§6.1), and the
fact that toyos-cc's host suite asserts on no emitted code at all (§9).

**What T7 did.** §4.6's two findings are closed by T3, so neither is filed. The
"nobody has asked whether they run" question was filed by T5, because its list
had to point at it. `9999b` and the emission-gate question are filed.

`89_nocode_wanted` was time-boxed and half of it is fixed: `expr_type` answered
a statement expression's type without ever entering the block, so every local
the block declared was an unknown identifier — `Codegen::stmt_expr_scopes` is
that scope now, and `dominance.rs` gates it. The case then stops one layer
further on, in the verifier, on a **block argument** `cranelift-frontend`
synthesized for a `goto` out of the statement expression. That is not the
`LocalStorage` class T2 closed — it is an argument the frontend made from a
control-flow graph we built — and it is wider than the box, so it is declined
into the T5 list against its new refusal, with its own issue file.

Two defects the wave found in passing and did not fix, both filed: every
parse-time diagnostic names the line *after* the one it is about — measured on
a one-line file — which matters more now that `NOT_RUN` quotes those refusals;
and `73_arm64` stops in the verifier on a variadic call passing an i32 where the
signature wants an i64, which is a codegen defect of ours reached through a case
declined for an unrelated reason.

**`79_vla_continue` is not measured and the wave could not measure it.** It
builds since T2 and is therefore discovered and run — but every guest boot on
this host needs the shared sysroot, which `wt/toyos-endow` holds for an ABI
change that has not landed. The host half of the suite is unaffected and green.
Its own `.expect` wants `OK` five times, which needs a VLA to reuse one stack
address per iteration, and a heap VLA can satisfy that only by accident of the
allocator — so the expected outcome is `NOT OK` and a declared entry. It must
be measured before it is declared.

Ordering: T1 first and alone (it is minutes, and T5/T6 cannot be written on top
of a compiler that exits). T4 is independent of everything — it is a different
crate. **T2 before T3**, which the plan used to leave open: T3 puts two names
into the compile-failure set, so T2's corpus gate reads differently on the other
side of it, and T3 rewrites the refusal `99_fastcall` gives (§4.5). T5 needs T1,
T2 and T3 to have settled which cases belong where *and* what each one says.
T6 needs T1. T7 last.

T2 and T3 both touch `codegen/stmt.rs` — T2 at the local-declaration path, T3 at
`Statement::Asm` — which is a shared file and not a dependency; one worktree,
one branch, so it costs nothing.

## 8. Declined, by name, with the reason

**Packed bitfields** (`specs/issues/build/toyos-cc-no-packed-bitfields.md`).
Measured over every `.c` and `.h` in the pinned doomgeneric tree (`fc60163`,
which is `forks.toml`'s `base` and the checkout's own `.toyos-commit`): of its
**15** `PACKEDATTR` structs none contains a bitfield, and the only bitfield
declarations in the whole checkout are the four in `struct color`
(`i_video.h:142-145`), which is not packed. So doomgeneric does not need this
even once `PACKEDATTR`
starts expanding. **The count is fifteen and not fourteen** — the plan inherited
fourteen from the issue file, which is also wrong; the conclusion does not move,
because the four-bitfield sweep proves it directly and does not depend on the
count at all. The demand the issue names is `specs/wlan-plan.md` §10's 635
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
change buys nothing today — the `PACKEDATTR` structs move **no field
offset** and one size (`pcx_t`, 130 → 129), and `WritePCXfile` never takes
`sizeof(pcx_t)`. **Re-file as `kind: rejected`, `status: none`** — it is an
answer, not work, and it is `kind: finding` / `status: open` today, so
`rg -l '^status: open'` counts it as unheld work.

Checked against the house model, because the two fields are not free of each
other: `specs/issues/README.md`'s table requires `kind: rejected` ⇒
`status: none`, and warns that "a ruling that declared a standing failure rather
than removing it deferred the work; it did not decline it, so the entry is a
`defect` and stays open". This is not that — the proposal is declined on the
merits and nothing is owed afterwards. One thing the rewrite must do: the body's
closing line ("Defining `__GNUC__` would be a much larger change than it looks")
reads as a cost estimate for doing it, which is work. It becomes the reason for
declining, or the file contradicts its own kind.

**`debug = true` produces no debug info**
(`specs/issues/build/debug-true-produces-no-debug-info.md`). Keeping `.debug_line`
through `toyos-ld` is a feature the issue already records as not planned, and
"There is no DWARF anywhere" is a stated property of the debugging story. Out.

**The std-fork batch (#179)** — out by the branch's boundary: it moves the shared
`rust/` tree.

## 9. For the owner

The four things the plan raised are all answered on measurements now, and none
of them blocks the implementer. This section is a readout, not a queue.

1. **Scope.** The task named five corpus cases; the fixture carries eleven and
   the corpus has 24 compile failures. §4.5 and §5 sort all of them, because a
   ruling that leaves six unsorted leaves the list mixed. If the intent was
   strictly the five, say so and T5 shrinks.
2. **The three refusal doors are safe.** `#pragma pack`, file-scope `asm(…)` and
   declarator `asm("name")` appear **nowhere** in `userland/libc`,
   `userland/doom` or the pinned doomgeneric tree, and in the corpora only in
   cases already declined (§4.6, all three doors swept — the plan had swept
   one). The residual is a header outside the tree, and it is the good kind of
   residual: the refusal is a named stop, where today the construct is dropped
   in silence.
3. **`__GNUC__` is re-filed `kind: rejected` / `status: none`** (§8), which the
   house model's own table requires once the ruling is a decline. Recorded here
   rather than asked: it is an answer, and leaving it `open` makes the estate's
   one query over-report.
4. **`12.S` is decided: refuse `params...` by name** (§6.1). The construct has
   exactly one occurrence in everything this project compiles — the test that
   tests it — and zero consumers. Its `9999b` half is declared instead, because
   a pp-number is conforming C99 and there is nothing there to refuse.

**One thing the owner may want to know that no chunk here fixes.** A compiler
this project ships had a call through a static-local function pointer jumping to
whatever the first eight bytes of the callee's code spell (§4.1), and a struct
assignment can be turned into an 8-byte store of the wrong thing with every gate
in the tree staying green (§4.1). Both were invisible because **nothing in
`toyos-cc`'s host suite asserts on emitted code** — the eleven tests are seven
about attribute refusals and four about determinism, and determinism compares a
run against another run. T2 adds the first emission assertions. Whether that
grows into a real codegen gate is a bigger question than this wave, and it is
the one the wave's findings actually point at.
