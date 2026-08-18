# Cranelift as ToyOS's codegen backend — what it costs today

Root `CLAUDE.md`'s self-hosting principle is "No LLVM dependency. Cranelift as
codegen backend." This is what that costs on 2026-08-10, measured rather than
estimated, so the next agent prices the north star instead of rediscovering it.

**The goal is unchanged.** The owner's ruling, 2026-08-10:

> "The north star may be far away but in the future we will contribute to
> cranelift so it has the features necessary for toyos. LLVM will be removed as
> a dependency. But later and not in this era."

So this document is a price list and a route, not an argument against the
destination. Nothing here is a reason to stop wanting it; all of it is a reason
not to schedule it yet.

## 1. The headline: cg_clif does not remove the LLVM dependency

`rustc_codegen_cranelift` assembles every `global_asm!` block by spawning
**itself with `-Zcodegen-backend=llvm`**. `compiler/rustc_codegen_cranelift/src/global_asm.rs::compile_global_asm`,
in the pinned fork at `rust/` — not upstream master, the code this tree would
actually build:

```rust
let mut child = Command::new(std::env::current_exe().unwrap())
    .arg("--target").arg(&config.target)
    .arg("--crate-type").arg("staticlib")
    .arg("--emit").arg("obj")
    .arg("-o").arg(&global_asm_object_file)
    .arg("-").arg("-Abad_asm_style")
    .arg("-Zcodegen-backend=llvm")
```

`asm!` and `naked_asm!` both reach the same place: `naked_asm!` is lowered to a
global-asm item, and `src/inline_asm.rs::generate_asm_wrapper` appends every
`asm!` block to the same buffer as an out-of-line stub. So **the LLVM assembler
is on the path of all 119 assembly sites in this tree** — 108 `asm!`,
10 `naked_asm!`, 1 `global_asm!`.

Verified rather than read. A deliberately invalid mnemonic in a `global_asm!`,
compiled with `-Zcodegen-backend=cranelift`, produces LLVM's integrated-assembler
diagnostic verbatim, wrapped in cg_clif's own failure message:

```
error: invalid instruction mnemonic 'this_is_not_a_valid_x86_instruction'
note: instantiated into assembly here
 --> <inline asm>:5:1
error: Failed to assemble `
       .intel_syntax noprefix
       this_is_not_a_valid_x86_instruction rax, rbx
       .att_syntax
```

The same file through the LLVM backend gives the identical first three lines.

### 1.1 The corollary — the hosted rustc we already built cannot compile ToyOS

`src/toolchain.rs::write_config` writes `codegen-backends = ["cranelift"]` under
`[target.x86_64-unknown-toyos]` when `with_hosted_rustc` is true, which makes the
rustc shipped in the initrd carry cranelift *instead of* LLVM. That artifact
exists: `rust/build/x86_64-unknown-toyos/stage2/lib/rustlib/x86_64-unknown-toyos/codegen-backends/librustc_codegen_cranelift-1.99.0-dev.so`,
21,865,400 bytes, built 2026-08-02.

A rustc with no LLVM backend cannot satisfy the subprocess above. So that
compiler can build Rust that contains no assembly, and nothing else — which
excludes the kernel, the bootloader, `toyos-abi`, and `userland/libc`.

**Inferred from the code path, not run in a guest.** Confirming it needs a boot
with that rustc in the initrd and a source file containing one `asm!`. Nobody
has done that, and this document should not be read as if somebody had.

## 2. What compiles, what does not, and what compiles wrongly

All of §2 and §3 were measured on the **system nightly**
(`rustc 1.96.0-nightly (d9563937f 2026-03-03)`, host `aarch64-apple-darwin`)
with its `rustc-codegen-cranelift-preview` component, in `/tmp`. Nothing touched
the shared sysroot, `bootstrap.toml`, the build lock or a host slot. The probe
crate carried the shapes this kernel uses: `cli`, `mov cr3`, a `gs:`-relative
per-CPU read, `out dx, al`, a naked IDT entry with a `sym` operand, a naked
function with a `const` operand, a `global_asm!` trampoline, and
`options(noreturn)`.

### 2.1 `sym` operands are a hard error

```
error: asm! and global_asm! sym operands are not yet supported
  --> src/main.rs:46:15
```

`sym` sits behind cg_clif's `inline_asm_sym` cargo feature, which is inside its
`unstable-features` group and which bootstrap enables for nothing
(`src/bootstrap/src/core/build_steps/compile.rs`, `CraneliftCodegenBackend::run`
passes no `--features`). This tree has **17 `sym` operand sites**: every IDT
entry (`kernel/src/arch/idt/{mod,timer,tlb,nmi,device_irq}.rs`), the syscall
entry (`kernel/src/arch/syscall.rs`), the AP entry (`kernel/src/main.rs`), both
ring-3 trampolines (`kernel/src/loader/start.rs`) and `userland/libc/src/lib.rs`.

Also unsupported: `asm goto` (labels), and unwinding out of `asm!`
(`src/base.rs:544`, "cranelift doesn't support unwinding from inline assembly").

### 2.2 The soft-float guarantee is silently discarded

cg_clif reads no target features. `sess.opts.cg.target_feature` and
`sess.target.features` appear nowhere in its source; only `sess.target.cpu` and
`-Ctarget-cpu` reach the ISA builder (`src/lib.rs::build_isa`). Both
`x86_64-unknown-none` and `x86_64-unknown-uefi` set `rustc_abi: Some(RustcAbi::Softfloat)`
and `features: "-mmx,-sse,…,+soft-float"`. rustc notices and warns:

```
warning: target feature `soft-float` must be enabled to ensure that the ABI of
the current target can be implemented correctly
= note: … it will become a hard error in a future release! (rust#116344)
```

Count of XMM-register instructions in the linked binary, same source,
`opt-level = 2`:

| target | LLVM | Cranelift |
|---|---|---|
| `x86_64-unknown-none` (kernel) | **0** | **31** |
| `x86_64-unknown-uefi` (bootloader) | **0** | **32** |

Cranelift emits `mulsd`, `divsd`, `addsd`, `cvtss2sd`, `movdqu`, `xorpd`, and
passes `f64` in `%xmm0`/`%xmm1`. LLVM passes floats in general registers and
calls soft-float helpers.

Two consequences. `arch::entry`'s save/restore bracket is sound *only* because
Ring 0 never touches the FPU (`specs/user-machine-state.md` §8); Cranelift-built
kernel code would corrupt a userland process's FPU registers with no diagnostic.
And on UEFI the bootloader target disables SSE precisely because firmware is
known to leave it uninitialised, so those instructions fault at boot.

`build::assert_kernel_is_softfloat` would in fact refuse the build — but for the
wrong reason, and its second half would pass vacuously. cg_clif's
`target_config` returns a **hardcoded** list with a FIXME (`src/lib.rs:157`),
empty when `sess.target.os == Os::None`:

```
LLVM      --target x86_64-unknown-none:  target_feature="fxsr", target_feature="x87"
Cranelift --target x86_64-unknown-none:  (nothing)
```

The assertion fires on the missing `x87`. The assertion that matters —
`!target_feature="sse"` — passes on a binary full of SSE.

### 2.3 `asm!` is never inlined; it is an indirect call through the GOT

Every `asm!` block becomes a separate stub function called through a GOT slot,
with its own frame and an `rdi`-passed spill buffer
(`src/inline_asm.rs::generate_asm_wrapper`). Disassembled,
`x86_64-unknown-none`, `opt-level = 2`:

```
LLVM,      _start:                 Cranelift, _start:
  movq %cr3, %rcx                    callq  read_cr3        ← a call
  movq %gs:(%rdx), %rax              callq  percpu_read     ← a call
  outb %al, %dx                      callq  out8            ← a call
  cli                                callq  cli             ← a call

                                   cli:
                                     pushq %rbp
                                     movq  %rsp, %rbp
                                     leaq  (%rsp), %rdi
                                     movq  0x1213(%rip), %rax   ← GOT load
                                     callq *%rax                ← indirect call
                                     …
                                   cli__inline_asm_…:
                                     pushq %rbp
                                     movq  %rsp, %rbp
                                     pushq %rbx
                                     movq  %rdi, %rbx
                                     cli                        ← the instruction
                                     popq  %rbx
                                     popq  %rbp
                                     retq
```

`CARGO_INCREMENTAL=0` recovers the Rust-level `#[inline]` (see §4) but not this;
the stub call is structural. For this tree that lands on `kernel/src/preempt.rs`,
`kernel/src/arch/percpu.rs`, `kernel/src/arch/cpu.rs` and
`toyos-abi/src/syscall.rs` — preemption, every per-CPU access, and every
userland syscall.

### 2.4 Unwinding aborts

cg_clif's `unwinding` cargo feature is not in `unstable-features` ("Not yet
included … for performance reasons") and bootstrap enables no features. With it
off, `src/abi/mod.rs:847` rewrites every unwind edge to
`UnwindAction::Unreachable`. Run on the host, `panic = unwind`:

```
LLVM:      panicked at src/main.rs:8:9: boom
           drop ran: inner
           catch_unwind returned err = true
           SURVIVED                                  exit 0

Cranelift: panicked at src/main.rs:8:9: boom
           fatal runtime error: failed to initiate panic, error 5, aborting
                                                     exit 134 (SIGABRT)
```

No `Drop`, no `catch_unwind`. This hit two pieces of this project's own code
during the probe: a Cranelift-built `toyos-cc` aborts where the LLVM-built one
reports its error and exits 101, and a Cranelift-built `toyos-sched` test binary
**aborts on its first test** — libtest itself does not survive. That is
`std_unwind`, `std_unwind_so`, `userland/doom/src/ffi.rs`'s C-boundary catch,
and the "a panic kills the process, not the subsystem" model.

`.eh_frame` *is* emitted, and generously: 348 bytes against LLVM's 28 on the
probe, because cg_clif emits CFI for every function even at `panic=abort`. The
asm stubs carry none (`src/inline_asm.rs:191`, "FIXME add .eh_frame unwind info
directives").

### 2.5 What does work

Bare metal, `#![no_std] #![no_main]`, cross from an ARM Mac: `x86_64-unknown-none`
links a static-PIE ELF and `x86_64-unknown-uefi` a PE/COFF `.efi`. PIC is on
unconditionally, inline stack probes are enabled on x86_64, the TLS model is
chosen from the binary format, and JSON target specs are handled. macOS AArch64
is a fully supported cg_clif host. **Cross-compilation is not a blocker.**

`x86_64-unknown-toyos` specifically is untested here: it needs the toyos
toolchain, and it adds `dynamic_linking`, `.so` output and `has_thread_local`
that the probe could not exercise.

DWARF is emitted and line attribution resolves, but it is statement-granular,
carries no inlined-frame provenance, has **no local-variable locations** (no
`.debug_loc`; the only `DW_AT_location` cg_clif emits is for statics), is
partial on types (`src/debuginfo/types.rs` FIXMEs, wide pointers among them),
and is disabled entirely for `is_like_windows` targets — which includes
`x86_64-unknown-uefi` (`src/debuginfo/mod.rs:69`). It is also much fatter:

| section, bare-metal probe | LLVM | Cranelift | |
|---|---|---|---|
| `.debug_line` | 14,460 | 118,996 | 8.2× |
| `.debug_info` | 25,258 | 187,612 | 7.4× |
| `.debug_str` | 18,679 | 280,171 | 15.0× |
| `.debug_loc` | 467 | absent | — |

`core::simd` is fully supported. `core::arch` is partial: cg_clif implements 86
x86 LLVM intrinsics, and **`llvm.x86.sse.sfence` is not among them** —
`userland/window/src/framebuffer.rs` calls `_mm_sfence()`. An unsupported
intrinsic is a compile-time *warning* and a runtime trap
(`src/intrinsics/llvm_x86.rs:1352`), not an error.

## 3. The measurements

Instrument for every row: `toyos-cc` at this tree's `[profile.toyos]`
(`opt-level = 2`, debug info, debug-assertions, overflow-checks), 10,358 lines,
system nightly, host `aarch64-apple-darwin`, 14 cores, load average 1.6–4.7
across the session. Ratios are Cranelift against LLVM.

### 3.1 Compile time — the prize

| | LLVM | Cranelift | ratio |
|---|---|---|---|
| clean build incl. dep graph, wall | 25.88 s | 16.85 s | **1.54×** |
| clean build incl. dep graph, user CPU | 140.65 s | 36.80 s | **3.82×** |
| leaf recodegen, wall (3 runs, spread <1%) | 3.16 s | 0.93 s | **3.40×** |
| leaf recodegen, user CPU | 10.43 s | 1.21 s | **8.62×** |

`cargo rustc --lib … -- -Ztime-passes`, one rustc invocation, same crate:

| pass | LLVM | Cranelift |
|---|---|---|
| `expand_crate` | 0.025 s | 0.024 s |
| `type_check_crate` | 0.149 s | 0.148 s |
| `codegen_crate` | 0.619 s | 0.144 s |
| `LLVM_passes` | 0.692 s | — |
| `finish_ongoing_codegen` | 2.146 s | 0.004 s |
| `link` | 0.004 s | 0.002 s |
| **`total`** | **3.503 s** | **0.871 s** |

**The backend is 78.9% of a single-crate rustc invocation under LLVM** and 17.0%
under Cranelift. That is the ceiling on what *any* faster backend can save, and
it collapses at whole-build scale: the clean build including the dependency
graph, build scripts and linking improved by **1.54× wall**, because everything
that is not codegen dilutes it.

Caveat: this is the **host** target with the system nightly. The guest-target
number could not be taken — `toyos-ld` is not built in this worktree, the build
system strips `RUSTFLAGS` (`src/build.rs::cargo_build`), and another checkout
held the sysroot claim. The backend fraction is a property of rustc, LLVM and
the crate, so it transfers approximately and not exactly.

### 3.2 Runtime — the price

`toyos-cc` compiling 340,530 bytes / 12,004 lines of generated C,
`--target x86_64-unknown-toyos -O2`. The two binaries produce **byte-identical**
305,248-byte output, so this is codegen quality and nothing else:

| | LLVM-built | Cranelift-built | ratio |
|---|---|---|---|
| runs 1/2/3, wall | 0.15 / 0.13 / 0.14 s | 2.50 / 2.92 / 2.79 s | **17–22×, median ≈20×** |
| preprocess only (`-E`), pure lexing | 0.02 s | 0.12 s | **≈6×** |

Root `CLAUDE.md`'s bar is **at most 2× a production OS**. This is ~20× against
LLVM-built code of this project's own, on a workload — pointer chasing, hash
maps, strings, recursion — that is a fair proxy for kernel and daemon code.

### 3.3 Code size

Host `aarch64-apple-darwin`, same crate:

| | LLVM | Cranelift | ratio |
|---|---|---|---|
| `__TEXT` | 6,930,432 | 14,647,296 | **2.11×** |
| whole binary | 10,480,856 | 29,847,160 | **2.85×** |

Do not read the bare-metal probe's `.text` as a counter-example: Cranelift's was
smaller there only because it used hardware SSE where LLVM emitted soft-float
calls, which is §2.2 rather than a win.

## 4. One thing on our side of the line

`[profile.toyos]` says `inherits = "dev"` in every crate that declares it, and
nothing overrides `incremental`, so incremental is on. `rustc_mir_transform`'s
inliner enables itself only when
`(optimize == More|Aggressive) && incremental == None` (`inline.rs:49`), so
**rustc's MIR inliner is off in every guest build today.** Harmless under LLVM,
which inlines for itself. Load-bearing the moment a backend switch is attempted,
because cg_clif has no inliner of its own (§5) and MIR is then the only one
there is. Filed: `issues/build/profile-toyos-incremental-disables-mir-inliner.md`.

## 5. The blocking list, by the project that owns each item

The distinction matters and is easy to run together. Cranelift **the library**
is in reasonable shape; almost everything that blocks ToyOS lives in
`rustc_codegen_cranelift`, and two items are one-line changes we could make in
our own fork today.

### Ours, today — `rust/`, our fork of rust-lang/rust

| | change | effect |
|---|---|---|
| `inline_asm_sym` | build cg_clif with the feature enabled in bootstrap's `CraneliftCodegenBackend` step | turns §2.1's hard error into working `sym` operands |
| `unwinding` | same, one more feature | §2.4; upstream calls it experimental and unsupported on Windows and macOS (rustc_codegen_cranelift#1567), so ELF targets only |

Both unblock *compiling*. Neither touches §2.2 or §2.3, so neither makes a
Cranelift-built kernel correct.

### Upstream, real work — rust-lang/rustc_codegen_cranelift

| | what is missing |
|---|---|
| target features | cg_clif reads none. Until `+soft-float`/`-sse` reach the ISA, no kernel or UEFI image built with it is trustworthy (§2.2). Two standing FIXMEs, `src/lib.rs:157` and `:173` |
| adopting the inliner | Cranelift's inliner exists; cg_clif does not call it (§5, next table). Until it does, §3.2's 20× is the number |
| `asm!` without an out-of-line stub | §2.3. Structural in `generate_asm_wrapper` |
| `std::arch` coverage | rustc_codegen_cranelift#171, open since 2018; unsupported intrinsics become runtime traps |
| debug info | no local variables, none at all on Windows-like targets (§2.5) |

### Upstream, already done — bytecodealliance/wasmtime (Cranelift the library)

A **function inliner** landed in Wasmtime 36 and is off by default, still
baking, reachable via `-C inlining=y` (fitzgen, "A Function Inliner for Wasmtime
and Cranelift", 2025-11-19). Reported gains there: 3.69× on a synthetic
cross-module call loop, 1.26× on pulldown-cmark, ~20% fewer retired
instructions. **cg_clif does not reference it anywhere in its source.** So the
inliner is not a Cranelift gap to contribute; it is a cg_clif adoption task.

### The one with no upstream plan at all

A **native assembler**, so `global_asm!` stops re-entering LLVM (§1). Nothing
found upstream proposes one. This is the load-bearing blocker: every other item
on this page could land and LLVM would still be a build dependency.

## 6. The `toyos-as` route — verified, and it holds

The owner's suggestion was that this project already owns `toyos-ld` and
`toyos-cc`, cg_clif has a path that shells out to an external `as` instead of
LLVM, and so a `toyos-as` might close §5's last row with code we own. Asked to
check it rather than believe it. What is actually true:

**The env var is compile-time.** `global_asm.rs:227` is
`option_env!("CG_CLIF_FORCE_GNU_AS")`, which reads the environment when cg_clif
*itself* is compiled. So it is not a runtime switch — it is a cg_clif build-time
decision, the same class of change as `inline_asm_sym` and `unwinding` in §5,
and reachable from our own bootstrap.

**The interface is two arguments and a pipe.** The whole contract:

```rust
Command::new(&config.assembler).arg("-o").arg(&global_asm_object_file)
    .stdin(Stdio::piped())          // assembly text in
```

Assembly on stdin, one object file out, no other flags.

**The binary's name is derived from the linker's, and it derives to `toyos-as`.**
`src/toolchain.rs::get_toolchain_binary` takes the configured linker's file name
and replaces `ld`/`gcc`/`clang`/`cc` with the tool name. All three of this
project's targets name `toyos-ld` — the kernel and bootloader in their
`.cargo/config.toml`, userland through `bootstrap.toml` — and the derivation was
run rather than reasoned about:

```
  toyos-ld  ->  toyos-as
  rust-lld  ->  rust-las
    ld.lld  ->  as
```

So cg_clif would look for `toyos-as` **beside `toyos-ld`**, with no
configuration at all. The pattern is exactly `toyos-ld`'s and `toyos-cc`'s.

**What `toyos-as` would have to be, and this is the part that is not small.** It
must accept GNU-`as` source: cg_clif wraps each block in `.intel_syntax noprefix`
… `.att_syntax` (`codegen_global_asm_inner`), so **both syntaxes**, plus whatever
directives the tree's own `asm!` and `naked_asm!` text uses, plus every directive
`core`'s and `std`'s own assembly uses — the sysroot is compiled too, and it is
not ours. It must emit relocatable ELF with real relocations, because `sym`
operands arrive as symbol names in the text. cg_clif strips `//` comments before
handing the text over, because they are LLVM-style and GNU `as` rejects them, and
`src/inline_asm.rs:51` records at least one construct that "breaks with the GNU
assembler" — the two assemblers are not interchangeable in every case, so
"compatible with GNU `as`" is a moving target rather than a fixed one.

**Verdict: the route holds and is worth recording.** The mechanism is real, the
hook is a build-time flag we control, and the name falls out for free. The work
is an x86-64 assembler with two syntaxes, GNU directive coverage and ELF
relocations — larger than `toyos-cc`'s backend, comparable to `toyos-ld`, and
with an open-ended tail wherever upstream assembly disagrees with GNU `as`.
Nobody has written a line of it and no `toyos-as` interface has been tested
against cg_clif; that test is the first thing to do if this is ever picked up.

## 7. Where this leaves the north star

Not one step away, and switching backends is not the step. In order:

1. **A native assembler** (§6), or `global_asm!` keeps calling LLVM and an
   LLVM-free rustc cannot compile an OS.
2. **Target features in cg_clif** (§2.2), or the kernel and bootloader are
   silently miscompiled.
3. **cg_clif adopting Cranelift's inliner** (§5), or §3.2's 20× stands against a
   2× bar.
4. **Unwinding on by default** (§2.4), or userland has no panic model.

`inline_asm_sym` and `unwinding` are ours to flip and buy only the ability to
compile. The self-hosting artifact that already exists — a cranelift-only rustc
in the initrd — is real and useful for asm-free Rust, and is not a compiler for
this operating system.

## Reproducing any of this

Everything in §2 and §3 runs against the **system nightly** with its
`rustc-codegen-cranelift-preview` component and the `x86_64-unknown-none` and
`x86_64-unknown-uefi` targets, in a scratch directory:

```
RUSTFLAGS="-Zcodegen-backend=cranelift" cargo +nightly build --target x86_64-unknown-none --profile toyos
```

It needs no toolchain build, takes no build lock, claims no sysroot and boots no
guest, which is why a spike of this size cost the concurrent verification
nothing. Prefer it to reconfiguring `bootstrap.toml` for any follow-up.
