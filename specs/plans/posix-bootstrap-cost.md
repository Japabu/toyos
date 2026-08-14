# Bootstrap GCC — Plan

Build GCC for ToyOS using only tools we bootstrapped from Rust.
No pre-generated files. No host make/sed/awk/grep.

## Philosophy

Every tool in the chain must be compiled from Rust or from C via our own
Rust-bootstrapped C compiler. The host system provides nothing except a
Rust toolchain.

## Bootstrap Chain

### Already done
1. **toyos-cc** — Our Rust-based C compiler (cross-compiles C to ToyOS).
   Exercised against TinyCC's own test corpus: `tests/testcases/tinycc`, 155
   cases compiled for `x86_64-unknown-toyos` and executed in the guest by
   `cargo test`. That covers "toyos-cc handles TinyCC's C", not "toyos-cc
   builds the TinyCC compiler".

### Needed (in order)

2. **toyos-cc compiles TCC itself** — the step this plan used to claim was
   done. A `bootstrap-cc` crate existed for it and was deleted 2026-08-01:
   nothing built it, and it targeted the *host* (Mach-O, `-e _main`, macOS SDK
   headers, `panic!` on any other OS), so its output could never run on ToyOS.
   It also failed at stage 1, inside Apple's `__darwin_mcontext64` — repairing
   it meant teaching toyos-cc macOS header internals, which buys ToyOS nothing
   and contradicts "toyos-cc is not meant to grow". Whoever restarts this:
   target `x86_64-unknown-toyos` against toyos-libc from the first line, and
   pin a stable release tarball with a verified hash rather than a cgit
   snapshot.

3. **TCC compiles dash** — Minimal POSIX shell (~15k lines of C, very
   portable). Needed because GCC's `./configure` is a massive autoconf-
   generated shell script. No Rust POSIX shell is mature enough to run it.

4. **TCC compiles GNU make** — Configure scripts invoke make. GNU make is
   ~30k lines of C. Alternatively, look at posixutils-rs (has a Rust make)
   but GNU compat is uncertain.

5. **TCC compiles binutils subset** — `ar`, `ranlib`, `nm`. Needed for
   building static libraries during GCC's build. We already have a Rust
   `ar` implementation (was in the old main.rs) — could revive that as a
   standalone tool.

6. **TCC compiles sed + awk + grep** — Configure scripts use these heavily.
   Alternatives: sed-rs (Rust, claims GNU compat), ripgrep (not drop-in),
   uutils coreutils (covers many but not sed/awk/grep).

7. **TCC compiles coreutils subset** — `cat`, `mkdir`, `rm`, `mv`, `cp`,
   `chmod`, `ln`, `test`, `basename`, `dirname`, `touch`, `sort`, `tr`,
   `head`, `tail`, `wc`, `ls`, `pwd`, `echo`, `printf`, `find`, `xargs`,
   `expr`, `cmp`, `diff`. Alternative: uutils/coreutils (Rust) covers most.

8. **TCC compiles flex + bison** — GCC's parsers need these. Could
   potentially skip if we use a GCC release tarball (which ships
   pre-generated parser files).

9. **Configure + build GCC** — With all the above on PATH:
   `./configure --target=x86_64-unknown-toyos ... && make`

## Shortcuts to Investigate

- **GCC release tarballs** ship pre-generated flex/bison output, so we
  might not need flex/bison at all.
- **uutils/coreutils** (Rust) could replace step 7 entirely — compile it
  for ToyOS instead of using GNU coreutils.
- **posixutils-rs** has Rust implementations of make, awk, lex, yacc, m4,
  diff. If these are GNU-compatible enough, they could replace steps 4, 6,
  and 8.
- A minimal `ar` in Rust (we had one) could replace binutils for our needs.
- Consider whether we need GCC at all vs. just TCC + musl for C, or
  pursuing Clang/LLVM instead.

## GCC Version

Target latest stable GCC. Download from GNU mirrors:
`https://ftp.gnu.org/gnu/gcc/gcc-<version>/gcc-<version>.tar.xz`

## Implementation Notes

- This crate should orchestrate the entire chain: download sources, build
  each dependency in order, then configure + build GCC.
- Use ureq + tar + flate2 for downloading and extracting.
- All intermediate build artifacts go in `out/` (gitignored).
- Toolchain wrappers go in `toolchain/` (gitignored).
- Nothing pre-generated is committed to git.

---

## Status (2026-07-27)

Recovered from the abandoned `unix_compat_try` branch before it was deleted.
Kept as a **cost map, not a roadmap**: it targets building GCC, which sits
awkwardly against the "no LLVM dependency, Cranelift as codegen backend"
north star. The dependency-chain analysis (autoconf needs a POSIX shell →
dash → make → ar/ranlib/nm → sed/awk/grep → coreutils → flex/bison) is still
an accurate estimate of what running autotools software on ToyOS costs.
