# What every third-party crate is for, 2026-08-08

The companion to `specs/dependency-audit-2026-08-08.md`. That document asked
whether the estate is *allowed*; this one asks what each crate is *for*. Its §8
passed 35 of 43 direct crates "without comment" — allowed, and nobody had
written down what any of them does. **A dependency nobody can justify is one
nobody will ever remove**, so the gap is closed here: one entry per direct
crate, with the file and function that calls it, and a price on taking it out.

**This recommends and removes nothing.** Every verdict below is the owner's to
accept or reject.

## The bar, restated

From CLAUDE.md and the owner's rulings: *"only general and often used rust
crates are allowed"*, and *"a crate specifically for a driver is not ok."*
Popularity is the evidence, not the rule — a well-maintained crate that exists
to do **our** job fails anyway. Download counts and reverse-dependency counts
below are the honest measure of "general and often used" and are quoted for
that reason, not as a verdict on their own.

## Method

Driven from `cargo metadata`, as the owner asked, not from grepping manifests.

- `cargo metadata --format-version 1` in each of the **29** workspace roots (the
  28 the audit counted, plus `toyos-fat32-check/`, which landed today). Three
  needed the network because their registry cache was cold; the rest ran
  `--offline`.
- Direct edges are `packages[] | select(id in workspace_members) |
  .dependencies[] | select(.source != null)` — every third-party dependency our
  own manifests name, with its kind, its requirement and whether it resolves to
  crates.io or a fork.
- Call sites are `grep -rn --include='*.rs' -E '(^|[^A-Za-z0-9_:])<ident>(::|\{| )'`
  over the owning package's directory, then read. Four names collide with our
  own modules and were separated by reading every hit: `elf` (`crate::elf` in
  the kernel), `gpt` (`kernel/src/gpt.rs`), `image` (`src/image.rs`) and
  `object` (a function in `kernel/src/user_ptr.rs`). A grep that skipped that
  step would have reported the `image` crate as having six call sites; it had
  one, and `src/wallpaper.rs` added the second later the same day.
- crates.io figures come from `curl` against `api/v1/crates/<name>`,
  `/reverse_dependencies?per_page=1` and `/owners`, fetched 2026-08-08. Nothing
  here is recalled.
- "Actually compiled" comes from the build artifacts of a real build:
  `userland/target/toyos/deps/`, distinct crate names, in the primary checkout.

### What came out

**43** distinct third-party crates — the same 43 the audit listed, confirmed
from the resolver rather than from manifest text — reached by **58** distinct
(our package → crate) edges across **21** of our own packages. `doom` names
ten, more than any other; `toyos-build` nine.

**One direct dependency is named in a manifest and called from nowhere**, found
by checking every one of the 58 edges against the owning package's sources:
`webpki-roots` in `userland/doom/Cargo.toml:18`. §8.4.

---

# Part I — the 43, by what names them

Each entry: where it comes from, whether it is alive, which manifest names it,
which of our code calls it, why we need it, and what removing it would cost.
Prices are judgments and say so; every count and date is measured.

## 1. The build system — `toyos-build` (root `Cargo.toml`)

Nine crates. The root workspace locks 134 packages.

### `fatfs` 0.3.6 — `Cargo.toml:8`

- **From** crates.io. Sole owner `rafalh`. **Last release 2023-01-17**, 49
  reverse dependents, 1,362,843 downloads. The oldest release in the estate.
- **Called at** `src/image.rs:131` — `fatfs::format_volume` with
  `FatType::Fat32` and a volume label, making the empty ESP and `/log` volumes.
  Also `tests/common/volumes.rs` (7 sites) and `tests/common/toybox.rs` (4),
  all `fatfs::FileSystem::new` + read, reading *contents* back out of volumes
  our own code wrote.
- **Why** two jobs, and only one is duplication. `toyos-fat32` has no format
  path by design, so `format_volume` is the only thing in the tree that can
  make an empty FAT32 volume. The test uses are the *outside judge* role
  `tests/common/volumes.rs:10-12` states explicitly.
- **Could it go?** The judge half should not: an independent implementation
  reading back our writer's output is worth more than the dependency costs.
  `toyos-fat32-check` (landed today) took over the *structural* judging, but not
  content reads; `tests/common/volumes.rs:10-12` names both as "implementations
  that are not the kernel's". The format half is a boot sector, two FATs and an
  empty root directory against fatgen103 — an afternoon — but retiring it alone saves
  nothing, because the test half keeps the crate in the lock. **Keep.**

### `gpt` 3.1.0 — `Cargo.toml:10`

- **From** crates.io. Owners `Quyzi`, `soerenmeier`. Latest **4.1.0**
  (2025-03-16); we pin `3.1.0`. 25 reverse dependents, 1,719,220 downloads.
- **Called at** `src/image.rs:348-394` — protective MBR, `GptConfig`, two
  `add_partition` calls (ESP and `ToyOS log`) and `bytes_start`. Read back at
  `tests/common/gpt.rs` (7 sites) and `tests/common/volumes.rs` (3).
- **Why** `toyos-gpt` parses and does not write. This is the writer for the boot
  image's partition table, and the outside judge for it.
- **Could it go?** Same shape as `fatfs`, same answer. A GPT writer is a
  protective MBR, two headers, an entry array and three CRC32s — call it a day —
  and again it buys nothing while the judge half stays. **Keep**, and note the
  major version we are behind on.

### `uuid` 1.22.0 — `Cargo.toml:12` (feature `v4`)

- **From** crates.io. `KodrAus` / rust-lang-nursery. 19,325 reverse dependents,
  713,362,889 downloads. First rank.
- **Called at** `src/image.rs:51,309,325` — `Uuid::new_v4()`, and `:247,336` as
  a parameter type. It mints the log partition's GUID, which the image build
  writes to `\toyos\log.guid` and the bootloader carries in `KernelArgs`.
- **Why** one random 128-bit value with the RFC 4122 version and variant bits
  set, plus its byte order. **Could it go?** Yes, for about twenty lines and a
  source of randomness the build system does not currently have. Not worth it —
  8-crate closure, and the crate is as general as they come. **Keep.**

### `toml` 0.8.23 — `Cargo.toml:11`

- **From** crates.io. `ehuss`, `epage`, toml-rs. 17,030 reverse dependents.
  Latest is `1.1.4+spec-1.1.0` (2026-07-28); we are on 0.8.
- **Called at** `src/build.rs:63` (`system.toml`), `src/build.rs:526`
  (`kernel/Cargo.toml`, for the `--kernel-feature` name check) and
  `tests/toyos.rs:1154` (`tests/audio-baseline.toml`).
- **Why** the only TOML parser for `system.toml`, which is what decides what
  gets built and booted. **Could it go?** No. **Keep.**

### `serde` 1.0.228 — `Cargo.toml:13` (feature `derive`)

- **From** crates.io. `dtolnay`. **113,097 reverse dependents** — the most
  depended-on crate in the estate.
- **Called at** `src/build.rs:8` (`use serde::Deserialize`) and
  `tests/toyos.rs:1108,1123` (two `#[derive(serde::Deserialize)]`).
- **Why** `toml`'s deserializer needs it; it is not a separate choice.
  **Keep.**

### `serde_json` 1.0.149 — `Cargo.toml:14`

- **From** crates.io. `dtolnay`. 91,696 reverse dependents.
- **Called at** `src/libc.rs:65,78` — parsing cargo's
  `--message-format=json-render-diagnostics` stream to find the artifact
  `toyos-libc` built.
- **Why** the wire format is cargo's, not ours. **Keep.**

### `image` 0.25.10 — `Cargo.toml:17` (`default-features = false`, `jpeg`)

- **From** crates.io. image-rs org, five owners. 5,993 reverse dependents,
  164,361,596 downloads.
- **Called at** **two sites**: `src/assets.rs:266` —
  `image::load_from_memory_with_format(&jpg_data, image::ImageFormat::Jpeg)`,
  `.to_rgb8()`, written into the initrd as `share/<stem>.rgb` with a width and
  height prefix — and `src/wallpaper.rs`, which *encodes* the one file that
  decode reads. (The five `image::` hits in `src/build.rs` are our own
  `src/image.rs` module.)
- **Why** the loop at `src/assets.rs:262` decodes every `.jpg` in `assets/`, and
  `git ls-files '*.jpg'` says `assets/` holds exactly one: `wallpaper.jpg`.
- **Could it go? Asked and answered on 2026-08-08: no.** The removal was priced
  against replacing that file with a background drawn at runtime; the owner
  chose a generated *file* instead (audit §7f), and a file in the pipeline needs
  a decoder. The crate encoding as well as decoding is what let the generator be
  written with no new dependency at all — a noise crate, a colour crate and a
  PNG encoder were each one line away and none was taken. **Keep** — §13.3 is
  closed.

### `fontdue` 0.9.3 — `Cargo.toml:9`

- **From** crates.io. Sole owner `mooman219`. 184 reverse dependents, latest
  0.9.4 (2026-07-29) — alive.
- **Called at** `src/assets.rs:16` — `Font::from_bytes` +
  `horizontal_line_metrics` + `rasterize`, building the `share/fonts/*.font`
  8x16 cell tables every initrd carries. Also `userland/snake/build.rs:14`, the
  same call for snake's own table.
- **Why** the one TrueType rasterizer in the tree. `assets/JetBrainsMono-Regular.ttf`
  is a scalable outline and the console needs a bitmap grid; nothing else turns
  one into the other. **Keep.**

### `libc` 0.2.189 — `Cargo.toml:21`, **dev-dependency**

- **From** crates.io. rust-lang. **13,544 reverse dependents.**
- **Called at** **one site**: `tests/common/hostload.rs:105` —
  `libc::getloadavg`, for gate A's `host:` annotation.
- **Why** a host syscall with no `std` equivalent. **Keep** — and note for
  whoever picks up audit §5: this crate is *already in the lock and already a
  dev-dependency*, so replacing the `ps` and `df` subprocesses with FFI needs no
  new dependency at all. `hostload.rs` already has both shapes side by side, one
  line apart.

## 2. The kernel

Four crates. `kernel/Cargo.lock` locks 20 packages, of which `libc`,
`windows-sys`, `windows-link` and `cfg-if` are `dlmalloc`'s other-platform
entries, locked and never built.

### `dlmalloc` 0.2.13 — `kernel/Cargo.toml:452` and `userland/libc/Cargo.toml:14`

- **From** crates.io. `alexcrichton` / rust-lang libs. 32 reverse dependents.
  Small by download count (16,897,749) because almost nobody needs it — but it
  is what `std` itself uses on wasm and on unsupported targets, which is the
  relevant credential.
- **Called at** `kernel/src/mm/alloc.rs:29,71,84` — `unsafe impl
  dlmalloc::Allocator for KernelPageSource`, then `Dlmalloc::new_with_allocator`
  behind a `Lock`. And `userland/libc/src/lib.rs:81,120,124`, the same shape over
  an `mmap` page source.
- **Why** the kernel heap and libc's `malloc`. A general-purpose allocator over
  a page source we supply is exactly the shape this crate exists for, and
  writing one is not on the roadmap. **Keep.**

### `hashbrown` 0.16.1 — `kernel/Cargo.toml:451` (`default-hasher`)

- **From** crates.io. `Amanieu` / rust-lang. 2,123 reverse dependents,
  2,126,640,450 downloads. It *is* `std::collections::HashMap`.
- **Called at** ten kernel files — `vfs.rs`, `page_cache.rs`, `scheduler.rs`,
  `mm/paging.rs`, `id_map.rs`, `listener.rs`, `fat32_adapter.rs`,
  `bcachefs_adapter.rs`, `loader/symbols.rs`, `elf/reloc.rs`.
- **Why** `alloc` does not re-export a hash map; only `std` does. In a `no_std`
  kernel this is the standard-library container, reached directly because there
  is no other way to reach it. **Keep.**

### `rustc-demangle` 0.1.27 — `kernel/Cargo.toml:449`

- **From** crates.io. rust-lang, compiler team. 192 reverse dependents.
- **Called at** `kernel/src/symbols.rs:257,284` — `demangle(raw)` in the two
  `log!` lines that print a backtrace frame.
- **Why** the mangling scheme is `rustc`'s, and this is `rustc`'s own decoder.
  Reimplementing it means tracking a format we do not own. **Keep.**

### `elf` 0.8.0 — `kernel/Cargo.toml:450`

- **From** crates.io. Sole owner `cole14`. 84 reverse dependents, 9,943,352
  downloads, 0.8.0 released 2025-05-14.
- **Called at** **two lines**: `kernel/src/symbols.rs:4,5` import `ElfBytes` and
  `endian::AnyEndian`, used only inside `SymbolTable::from_elf` to
  `minimal_parse`, walk `section_headers()`, find the first `SHT_SYMTAB` and
  follow its `sh_link` to the string table. Everything after that is raw byte
  arithmetic in our own code (`SYM_SIZE = 24`, `read_unaligned`). Every other
  `elf::` in `kernel/src/` is our own `crate::elf` module.
- **Why** nothing that is not already answered. `toyos-elf` — which the kernel
  *also* depends on (`kernel/Cargo.toml:442`) — has
  `section::SectionTable::symbols(SHT_SYMTAB)`, whose doc comment says it
  returns "a symbol table and the string table it points at", refusing a table
  whose `sh_link` names nothing. That is the entire job, already written,
  already host-tested, already `forbid(unsafe_code)`.
- **Could it go?** Yes. §13.2.

## 3. The bootloader

Three crates, `bootloader/Cargo.lock` locks 20.

### `uefi` 0.26.0 — `bootloader/Cargo.toml:11`

- **From** crates.io. rust-osdev, four owners. **Latest 0.39.0** (2026-07-11);
  we are on 0.26.0, thirteen minor versions behind. 29 reverse dependents.
- **Called at** `bootloader/src/main.rs:11` (a `use` block) and `:148`
  (`proto::device_path::media::HardDrive`), plus `SystemTable<Boot>` throughout.
- **Why** the firmware protocol tables, `cstr16!`, the file protocol and the
  memory map. Writing this ourselves is a large and pointless piece of work.
  **Keep.**

### `uefi-services` 0.23.0 — `bootloader/Cargo.toml:12` (`panic_handler`, `logger`)

- **From** crates.io. Same maintainers. **3 reverse dependents.** Its crates.io
  description, quoted verbatim from the API today, is:
  > Deprecated. Please migrate to `uefi::helpers`.
- **Called at** **two lines**: `bootloader/src/main.rs:20` (`use
  uefi_services::println`) and `:545` (`uefi_services::init(&mut system_table)`).
- **Why** a `println!` and a panic handler before `ExitBootServices`.
- **Could it go?** Yes — the publisher says where to. The migration target lives
  inside `uefi` itself, so it is not a dependency swap, it is a deletion. But it
  is coupled to a `uefi` upgrade whose API changed shape (the global system
  table replaced `SystemTable<Boot>` threading), so the price is the upgrade,
  not the two lines. §13.5.

### `elf` 0.7.4 — `bootloader/Cargo.toml:13`

- **From** crates.io, same crate as §2, **a major version older**.
- **Called at** `bootloader/src/main.rs:10` and `:234-344` — `minimal_parse`,
  `segments()` (the PT_LOAD sizing and copy loops), `section_headers()` (the
  `SHT_REL` refusal and the `SHT_RELA` walk), `section_data_as_relas()` and
  `ehdr.e_entry`.
- **Why** loading and relocating `kernel.elf`. **Could it go?** Yes — `toyos-elf`
  has `FileHeader::parse`, `Layout::parse` (which already enforces the
  `p_filesz <= p_memsz` invariant this code asserts by hand at `:245`),
  `SectionTable::iter` and `rela::RelaTable`. §13.2.

## 4. The toolchain — `toyos-ld` and `toyos-cc`

`toyos-ld/Cargo.lock` locks 26 packages, `toyos-cc/Cargo.lock` 62.

### `object` 0.38.1 — `toyos-ld/Cargo.toml:8` (read, write, elf, coff, pe, macho, std)

- **From** crates.io. `philipc`, `fitzgen` — the gimli project, which is what
  `rustc` and `wasmtime` read object files with. 395 reverse dependents,
  560,394,111 downloads. Latest 0.40.0 (2026-08-01).
- **Called at** `toyos-ld/src/collect.rs` (16 sites — reading input sections,
  symbols and relocations in three formats), `emit_elf.rs` (4), `main.rs` (2),
  `emit_pe.rs` (2), `tests/determinism.rs` (10), and
  `toyos-cc/src/emit.rs` (1).
- **Why** the linker's *input* side. Everything `toyos-ld` writes it writes
  itself; what it does not want to write four times is a reader for ELF, COFF
  and Mach-O object files produced by three other toolchains. **Keep.**

### `sha2` 0.10.9 — `toyos-ld/Cargo.toml:9`

- **From** crates.io. RustCrypto, `tarcieri`/`newpavlov`. 16,048 reverse
  dependents.
- **Called at** `toyos-ld/src/emit_macho.rs:4` — the `LC_CODE_SIGNATURE` ad-hoc
  signature.
- **Why** macOS arm64 refuses an unsigned binary, and the ad-hoc signature is a
  SHA-256 of each page. Reached through `toyos_ld::link_macho`, called from
  `toyos-cc/src/main.rs:89` when the target is Mach-O — which is every C program
  `toyos-cc` compiles *for the dev host*, i.e. its own test suite. Load-bearing
  today. **Keep.**

### `zerocopy` 0.8.42 — `toyos-ld/Cargo.toml:11` (feature `derive`)

- **From** crates.io. `joshlf`, `jswrenn` — Google. 746 reverse dependents,
  789,300,895 downloads.
- **Called at** `toyos-ld/src/emit_macho.rs:7,8,9` — `IntoBytes`, `FromZeros`,
  `Immutable` and the endian-typed integers for the Mach-O load commands.
- **Why** it makes a header struct's byte encoding a property of its type rather
  than of a hand-written serializer, which is the tree's stated preference
  (unrepresentable > checked > tested). **Keep.**

### `thiserror` 2.0.18 — `toyos-ld/Cargo.toml:10`

- **From** crates.io. `dtolnay`. 63,293 reverse dependents.
- **Called at** **one line**: `toyos-ld/src/lib.rs:28`,
  `#[derive(Debug, thiserror::Error)]` on `LinkError`.
- **Why** one derive. **Could it go?** Yes — a hand-written `Display` and
  `Error` impl. The proc-macro chain it shares with `zerocopy-derive`
  (`syn`, `quote`, `proc-macro2`, `unicode-ident`) stays either way, so the buy
  is exactly two crates. Barely worth the diff. **Keep**, noted for
  completeness.

### `cranelift-codegen` / `-frontend` / `-module` / `-object` 0.128.4 — `toyos-cc/Cargo.toml:8-11`

- **From** crates.io. Bytecode Alliance / wasmtime. codegen has 126 reverse
  dependents and 42,114,650 downloads; `-object` is the smallest at 35 and
  2,275,613. Latest 0.134.3 (2026-07-31); we pin 0.128.
- **Called at** `toyos-cc/src/codegen/mod.rs` (16 sites across the four),
  `toyos-cc/src/emit.rs` (5), and `tests/toyos-rust-tests/tls-cranelift/src/lib.rs`
  (9) — the guest test that proves Cranelift itself runs *inside* ToyOS.
- **Why** CLAUDE.md names it: *"No LLVM dependency. Cranelift as codegen
  backend."* This is the north star's backend, not a convenience. **Keep.**

### `target-lexicon` 0.13.5 — `toyos-cc/Cargo.toml:12`, root `Cargo.toml:51`, `tls-cranelift/Cargo.toml:12`

- **From** a **ToyOS fork**: `github.com/Japabu/target-lexicon`, branch `toyos`,
  base `bytecodealliance/target-lexicon`, delta `+7/-1` (`forks.toml:213`).
  Upstream has 225 reverse dependents and 326,212,468 downloads.
- **Called at** `toyos-cc/src/emit.rs:1`, `toyos-cc/src/codegen/mod.rs:1`,
  `tls-cranelift/src/lib.rs:1`.
- **Why** Cranelift's own triple type; seven added lines teach it the word
  `toyos`. Not a choice we made — it is `cranelift`'s interface. **Keep.**

## 5. Userland programs

`userland/Cargo.lock` locks **448** packages. A real build compiles **122** of
them (distinct crate names under `userland/target/toyos/deps/`).

### `smoltcp` 0.12.0 — `userland/netd/Cargo.toml:9`

- **From** crates.io. `whitequark` / smoltcp-rs. 92 reverse dependents. Latest
  0.13.1 (2026-04-30).
- **Called at** `userland/netd/src/main.rs:15-19` — `iface`, `phy`, `socket`,
  `time`, `wire`.
- **Why** the TCP/IP stack netd serves. Its own description is "designed for
  bare-metal, real-time systems without a heap", which is precisely the case.
  8-crate closure on the guest target. **Keep.**

### `rubato` 0.16.2 — `userland/soundd/Cargo.toml:10`

- **From** crates.io. Sole owner `HEnquist`. 189 reverse dependents, alive
  (latest **4.0.0**, 2026-07-09 — we are far behind).
- **Called at** `userland/soundd/src/main.rs:166` — `SincFixedOut` with sinc
  interpolation parameters and a window function.
- **Why** sample-rate conversion between a client's rate and the device's.
  Doing this badly is audible and the tree's rule is that audible quality is not
  traded without sign-off, so a hand-rolled resampler is the wrong economy.
  9-crate closure. **Keep**, with the version gap noted.

### `resvg` 0.47.0 — `userland/sprite/Cargo.toml:10`

- **From** crates.io. Five owners including `RazrFalcon` (author) and the Linebender
  group. 492 reverse dependents, 21,508,411 downloads. Latest 0.48.1.
- **Called at** `userland/sprite/src/lib.rs:1,2,19` — `usvg` parse,
  `resvg::render` into a `tiny_skia::Pixmap`.
- **Why** `assets/icons/*.svg` are SVG (Phosphor Icons, audit §7d) and the
  compositor's panel wants pixels. **Could it go?** Only by pre-rasterizing the
  icons at build time, which is what already happens to the fonts and the
  wallpaper — and which would move a 32-crate guest-side closure to the host, or
  delete it. Worth a look, but rasterizing at runtime is what makes an icon
  resolution-independent, so this is a design question rather than a cleanup.
  **Keep**, flagged in §13.7.

### `winit` 0.31.0-beta.2 — `userland/doom/Cargo.toml:10`, `userland/snake/Cargo.toml:10`

- **From** a **ToyOS fork**: `github.com/Japabu/winit`, branch `toyos`, base
  `c4afadbf`, delta `+1249`, tier `sibling` (`forks.toml:95`). Upstream:
  rust-windowing, 1,305 reverse dependents, **max stable 0.30.13** — we are on a
  **beta**.
- **Called at** `userland/doom/src/main.rs` (7), `userland/doom/src/input.rs`
  (2), `userland/snake/src/main.rs` (5).
- **Why** this is the point rather than a means: CLAUDE.md's ecosystem-fork rule
  says ToyOS aims to be *"a first-class platform in every important Rust
  ecosystem crate."* doom and snake are the proof that an unmodified `winit`
  program builds and runs here. 17-crate closure on the guest target — not the
  124 an all-platform `cargo tree` reports, because the other backends are
  locked and never compiled. **Keep.** The beta pin is worth a decision of its
  own.

### `softbuffer` 0.4.8 — `userland/doom/Cargo.toml:11`, `userland/snake/Cargo.toml:11`

- **From** a **ToyOS fork**: `Japabu/softbuffer`, base `f0d20b9`, delta `+155/-5`,
  tier `sibling` (`forks.toml:104`). Upstream 123 reverse dependents.
- **Called at** `userland/doom/src/main.rs` (3), `userland/snake/src/main.rs` (1).
- **Why** the same argument: `winit` gives a window, `softbuffer` gives pixels in
  it, and together they are the standard shape of a Rust program that draws.
  10-crate guest closure. **Keep.**

### `cpal` 0.18.0 — `userland/doom/Cargo.toml:9`, `userland/toybox/Cargo.toml:8`, `tests/toyos-rust-tests/Cargo.toml:12`

- **From** a **ToyOS fork**: `Japabu/cpal`, base `b4ddd28`, delta `+327`, tier
  `sibling` (`forks.toml:87`). Upstream RustAudio, 588 reverse dependents.
- **Called at** `userland/doom/src/sound.rs` (6), `userland/toybox/src/tone.rs`
  (3), `tests/toyos-rust-tests/src/tone.rs` (3),
  `tests/toyos-rust-tests/src/bin/hda_client_stall.rs` (3),
  `tests/common/audio.rs` (1).
- **Why** the audio analogue of `winit`, and gate A's instrument runs through it.
  4-crate guest closure. **Keep.**

### `rustysynth` 1.3.6 — `userland/doom/Cargo.toml:12`

- **From** crates.io. Sole owner `sinshu`. **84,145 downloads** — the lowest
  count in the estate, below even `rustls-rustcrypto`'s 126,966 — and 16 reverse
  dependents. Zero dependencies. Latest 1.3.6 (2025-08-10).
- **Called at** `userland/doom/src/sound.rs:8` and used at `:643,685,753-763` —
  reads a SoundFont, sequences a MIDI file, synthesises doom's music.
- **Why** doom's music. **And the repository ships no SoundFont**: `system.toml:28`
  declares `soundfont.sf2` under `untracked-assets`, `assets/` contains none, and
  `sound.rs:753` handles the missing file by printing *"playing without music"*.
  So on a clean clone this crate compiles into `/bin/doom` and never does
  anything.
- **Could it go?** It is the crate that most clearly fails *"general and often
  used"* on the numbers, and it serves a feature that is off by default. But it
  is zero-dependency and its removal deletes a capability rather than a
  duplication. **Owner's call** — §13.6.

### `russh` 0.60.0 — `userland/sshd/Cargo.toml:8` (`rustcrypto`, `flate2`, `rsa`)

- **From** a **ToyOS fork**: `Japabu/russh`, base per `forks.toml:79`, delta
  `+703/-231`, tier `fork`. Upstream `Eugeny`, 202 reverse dependents, latest
  0.62.5.
- **Called at** `userland/sshd/src/main.rs` (10 sites).
- **Why** SSH is a protocol with a large attack surface and a specification
  nobody should re-implement to get a shell on the machine.
- **Cost, measured.** `russh` is the single heaviest thing in the tree:
  **53 of the 122 crates a userland build actually compiles exist only for
  sshd** — the whole RustCrypto stack (`aes`, `aes-gcm`, `chacha20`, `poly1305`,
  `ghash`, `curve25519-dalek`, `ed25519-dalek`, `p256`, `p384`, `rsa`,
  `num-bigint-dig`, `ecdsa`, `pkcs1/5/8`, `spki`, `der`, …) plus `tokio`'s
  macros. That is 43% of the userland compile for a daemon CLAUDE.md says
  *"is started by nobody."*
- **Could it go?** Not as a dependency — the alternative is writing it. It could
  become **opt-in at build time** (a `system.toml` entry that is off by default),
  which is a build-system change and not a dependency change. §13.8.

### `tokio` 1.51.1 — `userland/sshd/Cargo.toml:9`

- **From** a **ToyOS fork**: `Japabu/tokio`, delta `+118/-41`, tier `fork`
  (`forks.toml:69`). Upstream 67,379 reverse dependents.
- **Called at** `userland/sshd/src/main.rs` (6 sites).
- **Why** `russh`'s API is `async` and `tokio` is the runtime it is written
  against. Not an independent choice. 12-crate guest closure. **Keep** while
  `russh` stays.

### `rand` — three major versions

| Where | Manifest | Requirement | Resolved |
|---|---|---|---|
| `snake` | `userland/snake/Cargo.toml:9` | `0.8` | 0.8.5 |
| `toyos-sched/sim` | `toyos-sched/sim/Cargo.toml:12` | `0.9` (`small_rng`) | 0.9.5 |
| `toyos-xhci/sim` | `toyos-xhci/sim/Cargo.toml:11` | `0.9` (`small_rng`) | 0.9.5 |
| `sshd` | `userland/sshd/Cargo.toml:7` | `0.10` | 0.10.0 |

- **From** crates.io, rust-lang-nursery. **30,715 reverse dependents.**
- **Called at** `userland/snake/src/main.rs` (2), `userland/sshd/src/main.rs`
  (2), `toyos-sched/sim/src/choice.rs` (2), `toyos-xhci/sim/tests/explore.rs` (2).
- **Why** snake's food placement, sshd's host-key and nonce generation, and the
  two interleaving fuzzers' choice sources.
- **Note.** Only snake's 0.8 and sshd's 0.10 share a lockfile; the two sims are
  separate workspaces, so consolidating those buys nothing but consistency.
  §13.9.

## 6. Host test crates

### `loom` 0.7.2 — `kernel-loom/Cargo.toml:28`, `toyos-sched/loom/Cargo.toml:31`

- **From** crates.io. `carllerche` / tokio-rs. 442 reverse dependents. Last
  release 2024-04-23 — stable rather than abandoned; it models a memory model
  that does not change.
- **Called at** `kernel-loom/tests/tlb_shootdown.rs` (10),
  `kernel-loom/tests/ticket_lock.rs` (5), `kernel-loom/src/lib.rs` (2),
  `toyos-sched/loom/tests/*` (22 across four files),
  `toyos-sched/src/{sync,mailbox,waitq}.rs` (5, cfg-gated),
  `kernel/src/{sync,shootdown}.rs` (2, cfg-gated).
- **Why** CLAUDE.md states it plainly: *"kernel-loom is the only thing that
  checks a memory ordering"* — x86's TSO hides a missing acquire edge from every
  guest test. There is no substitute and no intention to write one. **Keep.**

### `libloading` 0.8.9 — `tests/toyos-rust-tests/Cargo.toml:11`

- **From** a **ToyOS fork**: `Japabu/rust_libloading`, delta `+341/-4`, tier
  `toolchain` (`forks.toml:131`). Upstream sole owner `nagisa`, 1,281 reverse
  dependents, 467,359,153 downloads.
- **Called at** six guest test binaries under
  `tests/toyos-rust-tests/src/bin/` — `std_tls_dlopen`, `abuse_elf_segments`,
  `std_unwind_so`, `std_tls_multi_crate`, `std_tls_cranelift`,
  `abuse_elf_loader`.
- **Why** the `dlopen`/`dlsym` gate: these tests prove ToyOS's dynamic loader is
  correct *through the crate the ecosystem actually uses*, which is worth more
  than calling our own syscall directly. The fork is `toolchain` tier because
  `std` consumes it too. **Keep.**

## 7. The std-workspace shim

### `rustc-std-workspace-core` 1.0.1 — `toyos/Cargo.toml:12`, `toyos-abi/Cargo.toml:11`

- **From** crates.io. rust-lang. Both declare it as
  `core = { version = "1.0.0", optional = true, package = "rustc-std-workspace-core" }`,
  behind a `rustc-dep-of-std` feature.
- **Called from nowhere by name, by design.** It is the alias rust-lang's build
  system requires of any crate that is compiled *into* `std`, so that `core`
  resolves to the in-tree `core` rather than to a second copy. `rust/library/std`
  depends on `toyos-abi` and `toyos`, which is why they need it.
- **Why** not a choice: it is the ticket of admission to the std workspace.
  **Keep.**

## 8. doom's build-time download

Six crates in `userland/doom/Cargo.toml`'s `[build-dependencies]`. One of them
is not about the download at all.

### 8.1 `cc` 1.4.0 — `userland/doom/Cargo.toml:15`

- **From** crates.io. rust-lang libs team. 3,804 reverse dependents,
  1,105,348,633 downloads. First rank by any measure.
- **Called at** `userland/doom/build.rs` — `cc::Build::new()`, then
  `.compiler(&toyos_cc)`, 84 `.file()` calls and `.compile("doomgeneric")`.
- **Why** *not* to invoke a C compiler — it invokes **ours**. What it supplies is
  the build-graph plumbing around that: target and host detection, `CFLAGS`
  precedence, the `cargo:rustc-link-lib`/`link-search` lines, and the archive
  step. **Keep** — but see §12, which is what that archive step actually does.
- **Note.** This one survives whatever happens to the download.

### 8.2 `ureq` 3.2.0 — `:16` (`rustls-no-provider`, `rustls-webpki-roots`)

- **From** crates.io. `algesten`, `jsha`. 3,189 reverse dependents,
  173,593,379 downloads. A first-rank crate.
- **Called at** `userland/doom/build.rs:...` — `http_agent()` builds an
  `ureq::Agent` with an explicit `TlsConfig`; `download_doomgeneric` does one
  `agent.get(...).call()`.
- **Why** one HTTPS GET of `github.com/ozkl/doomgeneric/archive/refs/heads/master.tar.gz`.

### 8.3 `rustls-rustcrypto` 0.0.2-alpha — `:17`

- **From** crates.io. RustCrypto org, `tarcieri` — so the *publisher* is
  reputable. The *crate* is not: **no stable version has ever been released**
  (`max_stable_version` is `null`), the only version is `0.0.2-alpha` from
  **2024-04-24**, and it has 28 reverse dependents against 126,966 downloads.
- **Called at** one line in `build.rs`:
  `.unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))`.
- **Why** it is what keeps `ring` — and therefore a C compiler — out of the
  build. §11.
- **Verdict** unchanged from audit §6c: fails the bar on its face.

### 8.4 `webpki-roots` 1.0.6 — `:18` — **named, never called**

- **From** crates.io. rustls project, `ctz`/`djc`. 1,059 reverse dependents.
  Nothing wrong with the crate.
- **Called at** **nowhere.** `grep` for `webpki_roots` across every `.rs` file in
  `userland/doom/` returns nothing, and the systematic check of all 58 manifest
  edges flags this as the only third-party dependency in the tree with no
  reference in its owning package.
- **Why it appears to work:** `ureq`'s own manifest declares
  `rustls-webpki-roots = ["dep:webpki-roots"]`, and `build.rs` selects the roots
  through `ureq::tls::RootCerts::WebPki`. The crate is pulled in by the feature;
  our direct edge adds nothing.
- **Could it go?** Yes — delete line 18. §13.1.

### 8.5 `flate2` 1.1.9 — `:19` (`rust_backend`)

- **From** crates.io. `Byron`, `joshtriplett`, rust-lang. 6,022 reverse
  dependents, 604,681,743 downloads.
- **Called at** `build.rs` — `flate2::read::GzDecoder` over the response body.
- **Note, and it changes the arithmetic below:** `flate2` is **not** exclusive to
  the download. `userland/sshd/Cargo.toml:8` enables `russh`'s `flate2` feature.
  It stays either way.

### 8.6 `tar` 0.4.44 — `:20`

- **From** crates.io. `cgwalters`. 2,746 reverse dependents, 205,023,643
  downloads.
- **Called at** `build.rs` — `tar::Archive::new(gz)`, then the entry loop that
  strips `doomgeneric-master/doomgeneric/` and unpacks the rest.

### 8.7 Should the download exist once it is pinned?

The audit's §6 asked for the download to be pinned; another agent is doing that.
The question left over is whether pinning is enough. Measured, not argued:

- The download's transitive closure, intersected with the crates a userland
  build **actually compiles**, is **21 crates**: `ureq`, `ureq-proto`,
  `rustls`, `rustls-pki-types`, `rustls-rustcrypto`, `webpki-roots`,
  `untrusted`, `tar`, `filetime`, `xattr`, `http`, `httparse`,
  `percent-encoding`, `itoa`, `chacha20poly1305`, `x25519-dalek`, `ff`,
  `group`, `autocfg`, `semver`, `paste`. That is **17% of the 122 crates a
  userland build compiles**, existing to fetch a tarball.
- `flate2` and `cc` are not in that set and survive.

**A pinned download is still a download.** `forks.toml` already states the
sanctioned form for third-party source this project builds — a real repository
with a `toyos` branch on a pinned base — and the mechanism is used fourteen
times. doomgeneric is the only third-party *source* in the tree that is not
shaped that way. Making it one deletes the download, its four exclusive direct
crates and their seventeen transitive ones, and it deletes the alpha crate
without a separate decision. **Recommended** — §13.4.

---

# Part II — measurements

## 9. Where the 43 come from

| Source | Count | Which |
|---|---|---|
| crates.io, unforked | **35** | everything not listed below |
| ToyOS fork, via `[patch.crates-io]` | **5** | `cpal`, `russh`, `softbuffer`, `tokio`, `winit` |
| ToyOS fork, named directly as a git dependency | **2** | `libloading`, `target-lexicon` |
| Not really third-party | **1** | `rustc-std-workspace-core` (rust-lang's std-workspace alias) |

The five patched forks are worth stating plainly because the manifests do not
say so: `userland/doom/Cargo.toml:10` reads `winit = "0.31.0-beta.2"`, and what
resolves is
`git+https://github.com/Japabu/winit?branch=toyos#faf99eb…`, redirected by
`userland/Cargo.toml`'s `[patch.crates-io]` block. The version requirement is
the upstream contract; the source is ours.

## 10. Weight

| Lockfile | Locked | Notes |
|---|---|---|
| `userland/Cargo.lock` | 448 | **122** actually compiled |
| root `Cargo.lock` | 134 | |
| `tests/toyos-rust-tests/Cargo.lock` | 100 | |
| `toyos-cc/Cargo.lock` | 62 | Cranelift |
| `toyos-sched/Cargo.lock` | 47 | loom |
| `tls-cranelift` | 38 | |
| `kernel-loom` | 31 | |
| `toyos-ld` | 26 | |
| `kernel`, `bootloader` | 20 each | |
| `userland/snake/Cargo.lock` | 18 | **dead — §10.1** |
| everything else | ≤ 8 | eleven of them lock exactly 1 |

**472** distinct package names across the 29 lockfiles (471 before
`toyos-fat32-check` landed today).

Of the 122 crates a userland build actually compiles:

- **53** exist only for `sshd` (§5, `russh`).
- **21** exist only for doom's download (§8.7).
- The remaining **48** carry everything else: doom, the compositor, the
  terminal, netd, soundd, the shell, snake, sprite and toybox.

### 10.1 `userland/snake/Cargo.lock` is dead

`snake` is a member of the `userland` workspace (`userland/Cargo.toml:17`), so
cargo resolves it against `userland/Cargo.lock` and never reads this file.
`cargo metadata` run inside `userland/snake/` returns the 448-package userland
graph, not this one. It holds **18** packages, was last touched in the initial
squashed commit `52eb78e` (2026-07-27), and is tracked by git. It is one of the
29 lockfiles this and the previous audit counted, and it describes nothing.

**Recommendation:** delete the file. Price: one `git rm`.

## 11. `ring` is still not built, and what enforcing that would cost

Confirmed independently of the audit:

- `ring` appears once, at `userland/Cargo.lock:2693`, pulled by
  `rustls-webpki` 0.102.8 and 0.103.9.
- **Neither `ring` nor `rustls-webpki` appears in the 122 crates a real build
  compiles.** The set was taken from build artifacts, not from a resolver.
- The arrangement that keeps it out is `userland/doom/Cargo.toml:16,17` —
  `ureq`'s `rustls-no-provider` feature declines to select a crypto provider,
  and `rustls-rustcrypto` supplies a pure-Rust one instead.

**What enforcing it would cost.** `ring` builds C and assembly through the `cc`
crate, so it would put a real C compiler on the guest-userland build path — the
thing §4 of the audit says only *host* binaries currently need. A check that
this never happens is cheap and offline: `ring` must not appear in the set of
compiled crates. But a check over build *artifacts* is a check over a directory
some other run populated, which is not a gate. The honest version is a check
over the resolved feature graph — `cargo metadata` with the guest target, assert
no node named `ring` — which costs a resolve (seconds, no network, no guest) and
is exactly the shape audit §11.1 proposes for the crate ledger. **The cheapest
enforcement is therefore not a new mechanism: it is one named row in that
ledger, marked "must not resolve".** If §8.7 is taken, the whole `rustls`
subtree leaves the lock and the question disappears.

## 12. `ar`: a sixth external binary, reached through a crate

Audit §11.4 said no offline mechanism can see what a third-party build
dependency shells out to, and named `ring` as the example. Here is a live one.

`cc::Build::compile()` ends by building a static archive. `cc`'s
`get_base_archiver_variant("AR", "ar")` consults `AR`, `TARGET_AR`,
`AR_x86_64-unknown-toyos`, `AR_x86_64_unknown_toyos`, `CROSS_COMPILE` and
`ARFLAGS` — the doom build script's own output records all six as **`None`**:

```
AR_x86_64-unknown-toyos = None
TARGET_AR = None
AR = None
CROSS_COMPILE = None
```

So it falls back to whatever `ar` is first on `PATH`. On this host that is
`/opt/homebrew/opt/binutils/bin/ar`, **GNU ar 2.47 from Homebrew** — verified
two ways: `which ar`, and the archive it produced
(`…/build/doom-*/out/libdoomgeneric.a`) whose first member is named `/`, the
SysV symbol-table convention. macOS's own `/usr/bin/ar` is BSD `ar` and writes
`__.SYMDEF` instead.

Three things follow:

1. **`ar` belongs on audit §1's table.** It is not Rust and not QEMU, it is on
   the build path of every image that contains `/bin/doom`, and it is reached
   through no `Command::new` in our tree — so neither the existing preflight nor
   the binary ledger of §11.2 would ever see it.
2. **Which `ar` runs is whatever the developer's `PATH` says.** This host
   happens to have Homebrew binutils; a machine with only the Command Line Tools
   gets BSD `ar` and a differently-formatted archive. Nothing in the tree
   asserts which.
3. **It is fixable without removing anything.** `cc::Build::archiver()` takes a
   path. `toyos-ld` already reads archives; if it grows a `--archive` mode, the
   doom build can point `cc` at our own tool and the binary leaves the path
   entirely — which is the self-hosting answer rather than a workaround.

Not filed in `specs/known-issues.md`: audit §12 is populating §6 of that file
and another agent owns it. This should be cross-filed by whoever lands next
there.

---

# Part III — the actionable part

## 13. Deletion and consolidation candidates, ranked

The numbered entries below are grouped by subject and are **not** in rank order —
the table at the end of this section is the ranking, by *buy divided by price*.
Prices are engineering judgments and are marked as such; every count, date and
version in them is measured.

### 13.1 Delete `webpki-roots` from `userland/doom/Cargo.toml:18`

- **Price: one line.** Nothing else changes — `ureq`'s `rustls-webpki-roots`
  feature already declares `dep:webpki-roots`, so the crate keeps resolving and
  the build keeps working.
- **Buy:** zero bytes and one true statement. The manifest currently claims doom's
  build code uses a crate it has never named. That is the smallest possible
  instance of the thing this whole audit exists to prevent — something arrived
  and nobody was asked — and it is free to fix.
- **Risk:** none. Verified: `webpki_roots` appears in no `.rs` file under
  `userland/doom/`.

### 13.2 Retire the `elf` crate, both versions, in favour of `toyos-elf`

The clearest duplication in the tree, as the audit said. Two halves with very
different prices.

**Kernel half — an afternoon.** `kernel/Cargo.toml:450` (`elf = "0.8"`) is
reached from exactly two lines, `kernel/src/symbols.rs:4,5`, and used in one
function, `SymbolTable::from_elf`, to do one thing: find the first `SHT_SYMTAB`
section and follow its `sh_link` to the string table. `toyos-elf` — which the
kernel already depends on — has that function today:
`section::SectionTable::symbols(SHT_SYMTAB)`, returning `(symbols, strings)` and
refusing a table whose `sh_link` names nothing. The `from_elf` body shrinks;
everything after the section lookup is already our own raw arithmetic.

**Bootloader half — call it a day, plus a boot.** `bootloader/Cargo.toml:13`
(`elf = "0.7.4"`) is used for the PT_LOAD walk, the `SHT_REL` refusal, the
`SHT_RELA` walk and `e_entry`. `toyos-elf` has all four
(`FileHeader::parse`, `Layout::parse`, `SectionTable::iter`,
`rela::RelaTable`), and `Layout::parse` already enforces the
`p_filesz <= p_memsz` invariant the bootloader asserts by hand at
`main.rs:249`. The open question — the one thing not verified here — is whether
`Layout::parse`'s refusals, written for userland PIE, accept the kernel image's
shape unchanged. That is what the day is for.

- **Buy:** two ELF parsers leave the tree, one of them from the boot path.
  `toyos-elf` is `no_std`, `forbid(unsafe_code)` and host-tested; the crate it
  replaces is a single-maintainer crate at two incompatible major versions for
  one job. No closure saving — `elf` has no dependencies — so the buy is
  entirely "one parser, ours, tested."
- **Do the kernel half first.** It is the cheap one and it is independently
  landable.

### 13.3 Delete `image` — CLOSED 2026-08-08, and it stays

The price above was real and the gate resolved the other way. The decision this
item was downstream of got made: the wallpaper's provenance is answered by
generating the file (audit §7f), not by removing the file, so `src/assets.rs`
still decodes a JPEG at build time and the 9-crate closure stays where it is.

The gate is worth recording because it is the one this document got wrong about
its own leverage: **the cheapest removal in the tree was never a dependency
question.** It was contingent on how somebody chose to answer a licence
question, and the answer that settled the licence permanently — a picture with
source in the repository — is the one that keeps the decoder. `image` also
encodes, so it paid for itself twice in the same commit: no new dependency was
needed to write the generator.

### 13.4 Make doomgeneric a fork, and delete the download

- **Price: half a day, plus the fork repo.** Create
  `Japabu/doomgeneric` off a pinned `ozkl/doomgeneric` commit with a `toyos`
  branch, add the `forks.toml` entry, consume it the way fourteen other forks
  are consumed, delete `download_doomgeneric` and four lines of
  `[build-dependencies]`.
- **Buy, measured:** **21 of the 122 crates a userland build compiles**
  disappear (§8.7). `rustls-rustcrypto 0.0.2-alpha` — the one crate that fails
  the bar outright — goes with them, with no separate decision needed.
  doomgeneric becomes reproducible: today what you build depends on when you
  first cloned, and two developers can be building different Doom sources with
  no way to tell. The build stops touching the network, which also closes the
  race audit §6d measured against the suite.
- **Not removed:** `cc` (it drives `toyos-cc`) and `flate2` (`russh` enables it).
- **Note:** another agent is pinning the download right now. Pinning and this
  are not alternatives — pinning makes the build reproducible, this makes it
  offline and deletes 21 crates. If pinning lands first, this is a smaller
  change, not a redundant one.

### 13.5 Migrate off `uefi-services`

- **Price: a day or two, and it is really a `uefi` upgrade.** The crate's own
  crates.io description is *"Deprecated. Please migrate to `uefi::helpers`."*
  Our two call sites (`bootloader/src/main.rs:20,545`) are trivial; the
  migration target lives in a `uefi` newer than the 0.26.0 we pin, and the
  intervening releases changed how the system table is reached. That upgrade is
  the whole job.
- **Buy:** one dependency deleted rather than replaced (its successor is inside a
  crate we already have), thirteen minor versions of firmware-interface fixes,
  and the tree stops depending on a crate with **3** reverse dependents that its
  own publisher has told people to leave.
- **Risk:** it is the bootloader. Every boot on every machine goes through it,
  and the `--diag-boot` and metal paths have no serial fallback if it regresses.
  Worth doing, worth doing carefully, not worth doing in a hurry.

### 13.6 Decide about `rustysynth`

Stated rather than recommended, because it deletes a capability.

- **The facts:** 84,145 downloads and 16 reverse dependents — the weakest
  download count in the estate against the owner's *"general and often used"*
  bar; only `uefi-services` (§3) has fewer reverse dependents. One maintainer. Zero dependencies, so it costs nothing transitively. It exists for
  doom's music, and this repository ships no SoundFont
  (`system.toml:28`, `assets/` has none), so on a clean clone the code path runs
  once, fails to open `/share/soundfont.sf2`, prints *"playing without music"*
  and never runs again.
- **Price to remove:** delete `sound.rs`'s music thread and the `[dependencies]`
  line. Half a day. doom keeps its sound effects.
- **Price to keep:** one crate, no transitive weight, and a feature that works
  the moment the owner drops a file in `assets/`.
- **This is a capability question, not a hygiene one**, and it is the owner's.

### 13.7 Consider pre-rasterizing the icons

- **Price: an afternoon**, and it is the same shape as what already happens to
  the fonts and the wallpaper — `src/assets.rs` rasterizes at build time and
  ships the result in the initrd.
- **Buy:** `resvg`'s 32-crate guest-target closure leaves `userland/`.
- **Against:** a rasterized icon is a fixed size. The compositor's panel is not
  currently resolution-independent in any way that would notice, but choosing to
  make it so later is harder afterwards. **Design question, not cleanup** —
  raised so it is on the list, not pushed.

### 13.8 Make `sshd` opt-in at build time

- **Not a dependency change** — `russh` and `tokio` are the right answer to the
  problem sshd solves, and the alternative is writing an SSH implementation.
- **The measurement is the point:** 53 of the 122 crates a userland build
  compiles exist only for a daemon that CLAUDE.md says *"is started by
  nobody"*. Every build in every worktree pays for it, and the landing gate pays
  for it twelve times over.
- **Price: a `system.toml` entry**, off by default, and a decision about which
  test configurations turn it on. Call it an afternoon.
- **Buy:** 43% of the userland compile, on every build that does not want SSH.
  This is the largest single build-time saving available anywhere in this
  document, and it removes nothing.

### 13.9 Consolidate `rand`

- **Price: an hour.** `snake` 0.8 → 0.10 is `thread_rng`→`rng` and
  `gen_range`→`random_range`. The two sims are on 0.9 with `small_rng`, which
  0.10 still has.
- **Buy:** within `userland/Cargo.lock`, one `rand`/`rand_core`/`rand_chacha`
  triple instead of two. The sims live in their own lockfiles, so consolidating
  those buys consistency only.
- **Lowest priority item here.** Listed because three major versions of one
  crate is the kind of thing that is invisible until somebody counts.

### 13.10 Delete `userland/snake/Cargo.lock`

- **Price: one `git rm`.** §10.1 — `snake` is a workspace member, cargo never
  reads this file, it holds 18 stale packages from the initial commit, and it
  inflates every lockfile count anyone takes of this tree.

### Summary

| # | Candidate | Price | Buy |
|---|---|---|---|
| 13.1 | `webpki-roots` line in doom | one line | a manifest that stops lying |
| 13.10 | dead `userland/snake/Cargo.lock` | one `git rm` | 18 phantom packages |
| 13.8 | `sshd` opt-in at build time | an afternoon | **53 of 122** compiled crates |
| 13.4 | doomgeneric as a fork | half a day + a repo | **21 of 122**, plus the alpha crate, plus reproducibility |
| 13.2a | `elf` out of the kernel | an afternoon | one ELF parser fewer |
| 13.2b | `elf` out of the bootloader | a day + a boot | the second ELF parser |
| 13.9 | `rand` to one major | an hour | one triple in one lock |
| 13.5 | off `uefi-services` | a day or two | a deprecated dep, 13 versions of fixes |
| 13.6 | `rustysynth` | half a day | owner's call — deletes a capability |
| 13.7 | pre-rasterize icons | an afternoon | 32-crate closure — design question |

§13.3 (`image`, 9 crates) was on this table and is off it: the wallpaper
decision it was gated on went the other way, and the entry now records why.

**Explicitly recommended to keep**, with reasons, so the next reader does not
re-propose them: `image` (§13.3), `fatfs` and `gpt` (each an *independent judge* of our own
writer's output — see §1; only the format/write halves are duplication, and
retiring those alone saves nothing), `libc` (one call, and the answer to audit
§5's `ps` and `df` without a new dependency), `thiserror` (removal buys exactly
two crates), `sha2` and `zerocopy` (load-bearing for `toyos-cc`'s host tests
through `toyos_ld::link_macho`), and `cc` (it drives *our* compiler, and
survives §13.4).

## 14. What I could not verify

Stated so nothing here reads as more certain than it is.

1. **Whether `toyos-elf::Layout::parse` accepts `kernel.elf` unchanged.** It was
   written for userland PIE; the bootloader's segment walk is hand-rolled. §13.2
   prices a day for that half largely because of this. Reading the two side by
   side is not the same as running it.
2. **Which `uefi` version first shipped `uefi::helpers`.** The deprecation
   notice is verified verbatim from crates.io; the target module's first release
   is not, and §13.5's price assumes an upgrade is needed rather than confirming
   the exact floor.
3. **The `ar` `cc` would pick on a machine without Homebrew binutils.** The
   archive on *this* host is SysV-format and `which ar` names GNU ar; that a BSD
   `ar` archive would still satisfy `toyos-ld` is untested, and no test in the
   tree covers it.
4. **Prices are judgments.** Every count, date, download figure and version in
   this document came from a command that was run. Every "an afternoon" and "a
   day" did not.
5. **The 122-crate figure is one build's artifacts**, taken from the primary
   checkout's `userland/target/toyos/deps/`. It is ground truth for a build that
   happened, not a guarantee about a build with different features. The
   derived splits (53 for sshd, 21 for the download) intersect that set with
   `cargo tree` closures and inherit the same caveat.
6. **No suite was run for this document.** It changes no code. Reading the code
   is the gate here, and saying so is more honest than a green run that proves
   nothing about a `.md` file.
