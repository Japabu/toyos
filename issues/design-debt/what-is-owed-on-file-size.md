---
status: open
kind: finding
opened: 2026-08-08
---

# What is still owed on "this file is too big", with the numbers

Nine of the owner's review notes asked whether a large file could become a
crate, a host test, or both. Five have been answered and four have not; the
numbers below are 2026-08-08 except where a line dates itself, and exist so the
next reader does not re-measure.

**Answered:**

- `elf.rs` → `toyos-elf/` (1,604 lines, host-tested, crafted-input corpus),
  `e2c6a06`; the mapping half stayed as `kernel/src/elf/` (1,186), `42b29c9`.
- `compositor/main.rs` → `toyos-desktop/` (2,684 lines, pure), `763712b`; the
  effects shell is `userland/compositor/` at 2,085 over five files with a
  68-line `main.rs`, `72705d9`.
- `xhci/mod.rs` → the port machine is `toyos-xhci/` (2,082 lines with a host
  simulator), `2e81ae8`. `xhci/mod.rs` is still 1,825 lines, so the shell has
  not shrunk to match.
- `soundd/main.rs` → `toyos-mixer/` (2,739 lines over nine files, pure,
  `no_std`), 2026-08-20; the effects shell is `userland/soundd/` at 2,822 over
  eight files with a 261-line `main.rs`, from a 2,366-line one. What the note
  asked for is what it got: **the mixing is sample-exact and proven so.**
  `toyos-mixer/fixtures/mix-corpus.txt` — 924 lines, 56,600 bytes — was written
  by `soundd/src/main.rs` before a line of it moved, and
  `the_corpus_is_reproduced_bit_for_bit` holds the crate to it byte for byte.
  The transcript is compact because exhausted domains are digested rather than
  listed: all 65,536 i16 both ways, all 65,538 quantizer ties, 65,536 dither
  draws from each of six seeds, all 184,001 client rates against twelve device
  shapes. 48 tests where soundd had eight, of which six were the ones that
  moved. Keeping the crate `no_std` cost a rounding function — `core` has no
  `f32::round` — and that one is held to `std`'s over all 4,294,967,296 f32 bit
  patterns.
- `loader.rs` → `kernel/src/loader/` (1,397 over four files), `42b29c9`, with
  the pure decisions in `toyos-elf`. The plan/execute split — a `LoadPlan` an
  executor applies — is **not** built, and #159 changes what a mapping's
  protection is, so its shape is not settled.

**Still owed:**

- `arch/syscall.rs` — 2,061 lines at review, **2,248 now**. The biggest file in
  the kernel and the one holding three unrelated things (entry asm, ABI
  register mapping, every handler). The user-pointer decode being invisible
  inside it is where the cwd-accumulation and derived-allocation bug classes
  both lived.
- `process.rs` — 1,743 lines, no host model. #142 and #156 are the standing
  evidence that its bugs are interleaving bugs.
- `symbols.rs` — 293 lines, `core` + `alloc` only, no crate. The smallest and
  cheapest of the five; a real symbol blob is the fixture.
- `drivers/acpi.rs` — no `toyos-acpi`. Better than the note feared (typed
  `TableError`, named bounds, packed structs only for `offset_of!`), and it is
  stage 0 of the ACPI/AML track, whose interpreter is the most host-testable
  component this kernel will ever have.
