# Dependency audit, 2026-08-08

Everything this project depends on that it did not write, judged against the
owner's bar. **This is an inventory and a set of recommendations. Nothing here
was removed or replaced** — that is the owner's call, item by item.

## What has since been closed

The owner ruled on the licence and provenance half the same day, and it was
built. Every finding below is left standing rather than deleted, because the
evidence is what makes the fix reviewable; each section now carries its state.

| Finding | State |
|---|---|
| §6a doomgeneric fetched from a moving branch | **CLOSED** — pinned to `fc60163`, and in `forks.toml` |
| §6b, §6d the SoundFont and its race with the suite | **CLOSED** by another agent before this work started (`95f78f3`, `b8b0749`) |
| §7a `46_grep.c`, the DECUS "not for profit" clause | **CLOSED** — deleted |
| §7a the corpus carrying no attribution | **CLOSED** — `tests/testcases/LICENSE` |
| §7b, §7c, §7d, §7e the WAD, the font, the icons, the firmware | **CLOSED** — `NOTICE` and `licenses/` |
| §7f `assets/wallpaper.jpg` | **OPEN, and only the owner can close it** — recorded as unknown rather than guessed |
| §3 Python, §4 `cc`, §5 `df`/`ps`/`find` | **DECLARED**, not removed — README and preflight |
| §2 `dosfstools` in a committed workflow | **OPEN** — the CI agent owns that file |
| §1 the four macOS FAT tools | **OPEN** — being scoped elsewhere |
| §8 the crate observations, §11's three ledgers | **OPEN** — decisions, not defects |

## Why it exists

Two violations surfaced by accident within one day rather than by looking.
`fsck_msdos` — a macOS binary, and §1 measures its reach as larger than the nine
tests it was reported as — turned up only when CI could not run it. The doom
SoundFont's GPLv2 licence turned up only because the owner happened to ask.
Neither was found by any check the tree runs on itself, and the tree is too large
to keep discovering these by collision. §11 is the answer to "how does the tree
check its own bar", which is the question this document exists to make
unnecessary next time.

## The bar

From CLAUDE.md's Dependencies section and the owner's rulings, verbatim where it
matters:

- **Binaries: only what ships with Rust and QEMU.** *"hard no if its a macos
  binary. no dependencies on binaries that dont come with rust or qemu."* "It's
  only for tests" does not soften it — a test is part of the project.
- **Crates: general-purpose and widely used only.** *"only general and often
  used rust crates are allowed."* A crate that exists to do **our** specific job
  is something we write ourselves: *"a crate specifically for a driver is not
  ok."*
- **No Python anywhere.**
- **Forks are the sanctioned form of dependency** — not vendored, each a real
  repo with a `toyos` branch, upstream-mergeable, tracked in `forks.toml`.
- The repo declares **MIT OR Apache-2.0**.

The question that catches the subtle cases: **could this ever run inside ToyOS?**
Self-hosting is the north star, so a binary dependency is a permanent hole in
it. That is why this project wrote its own linker and its own C compiler.

## Method, and what it cannot see

Read against `wt/toyos-deps` off `543c7b0`. What was enumerated: every
`Command::new` and `std::process` site in every non-`rust/` Rust source; all
**52** `Cargo.toml` and **28** `Cargo.lock` files; all **8** CI workflows; every
`build.rs`; the **14** fork clones under `/Users/jan/Dev/jan/forks/` and the
**15** checkouts under `~/.cargo/git/checkouts/`; every file git tracks that is
binary. Provenance was established by content — hashes, embedded name tables,
byte comparison against upstream sources — because git history cannot help:
`assets/` and `ovmf/` both entered in the initial squashed commit `52eb78e`
("ToyOS") and carry no attribution.

Four things this audit **cannot** see, stated so nobody reads it as complete:

1. **What a third-party crate's own build script shells out to.** `ring` is in
   `userland/Cargo.lock` and needs a C compiler; it is not built today (verified:
   no `ring-*` build directory exists anywhere under any `target/`), but nothing
   in the tree would notice if a feature flip made it so.
2. **The `rust/` submodule's own build dependencies** beyond the ones §3 names.
   It is upstream's tree with 24 ToyOS-named files in it.
3. **Whether a licence statement is true**, only whether one exists.
4. **A binary invoked through a variable rather than a literal.** Three of the
   four macOS tools are reached that way (`Command::new(tool)`), which is
   precisely why a naive grep would have missed them.

---

# Part I — FAILURES

Grouped by verdict, worst first.

## 1. External binaries that are not Rust and not QEMU

The build system's own preflight (`src/main.rs:7`) checks exactly three tools:
`git`, `rustup`, `qemu-system-x86_64`. The real set is larger. Every row below
is a call site that exists today.

**Since closed, as a declaration and not a removal.** That preflight is now two
lists: `REQUIRED`, which exits — `git`, `rustup`, `qemu-system-x86_64` and
`cc` — and `ALSO_USED`, which names what is missing and continues: a Python
(any of the five `rust/x` searches for), `df`, `ps`, `find`. It is a `PATH`
scan rather than a `--version` run, because `py` on macOS opens the Command
Line Tools installer, which is the reason `rust/x` searches `python3` ahead of
it. The four FAT tools are not in either list: the agent scoping their
replacement owns that row.

| Binary | Where | Reached by | Verdict |
|---|---|---|---|
| `git` | `src/lib.rs:40,75,100`, `src/assets.rs:179,322`, `src/buildlock.rs:720,1104`, `src/land.rs:692,743`, `src/main.rs:10`, `src/toolchain.rs:101,198,214,225,988`, `src/worktree.rs:155` | every build | **PASS** — declared prerequisite, and the image's asset list is a function of git's index by design |
| `cargo`, `rustc`, `rustup` | `src/build.rs:135,222`, `src/libc.rs:44`, `src/toolchain.rs:414,868,884,900,935`, `tests/common/compile.rs:40` | every build | **PASS** — is Rust |
| `qemu-system-x86_64` | `src/main.rs:29`, `src/qemu.rs:76`, `tests/common/qemu.rs:2047` | every boot | **PASS** — is QEMU |
| **`sh` + `python3`** | `src/toolchain.rs:749,785,799` — `./x`, a `/bin/sh` script that searches `python3 python py python2 uv` and runs `x.py` → `src/bootstrap/bootstrap.py` (55,550 bytes) | **every toolchain build** | **FAIL** — §3 |
| **`cc`** | not ours: `rustc` invokes `"cc"` for every host-target link. Measured with `rustup run toyos rustc --print link-args`, which also sets `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk` | the build system itself, `toyos-ld`, `toyos-cc`, the harness, `rustc` stage2 | **FAIL** — §4 |
| **`df`** | `src/worktree.rs:141` | `cargo run -- --worktree add` | **FAIL** — §5 |
| **`ps`** | `tests/common/hostload.rs:111` (`ps -Ao comm=`) | gate A's host-conditions line | **FAIL** — §5 |
| **`/sbin/fsck_msdos`** | `src/image.rs:304`, `tests/common/volumes.rs:257,267`, `toyos-fat32/tests/common/mod.rs:320` | 5 guest tests, 2 `cargo test --lib` tests, all 59 `toyos-fat32` host tests | **FAIL** — known, being scoped elsewhere |
| **`/sbin/newfs_msdos`** | `toyos-fat32/tests/common/mod.rs:260` | all 59 `toyos-fat32` host tests | **FAIL** — known |
| **`/usr/bin/hdiutil`** | `toyos-fat32/tests/common/mod.rs:357,377,383` | all 59 `toyos-fat32` host tests | **FAIL** — known |
| **`find`** | `toyos-fat32/tests/common/mod.rs:301` — `find <mount> -name '._*' -delete`, sweeping macOS resource forks after a mount | all 59 `toyos-fat32` host tests | **FAIL** — §5, and **not on the known list** |

Precise reach of the FAT32 tools, since "nine tests" understates it:

- `fsck_complaints` (`tests/common/volumes.rs:256`) is reached by five entries in
  `MACHINE_TESTS`: `esp_filesystem`, `toybox_cp_volume`, `kernel_log_file`,
  `log_partition_layout`, `log_partition_identity` (`tests/toyos.rs:451-474`).
- `src/image.rs`'s own `fsck` (`:299`) is called by two `#[test]`s at `:348` and
  `:361` — these run under `cargo test --lib`, which is part of the landing gate.
- `toyos-fat32/tests/` holds **59** `#[test]` functions across three files, and
  all three import `common::Image`, whose construction is `newfs_msdos` →
  `hdiutil attach` → macOS writes → `find -delete` → `hdiutil detach` →
  `fsck_msdos`. None of them can run without all four.

**Recommendation.** Record `find` alongside the three already-known tools; it is
the same finding and the same fix. Nothing here proposes a replacement — that
work belongs to the agent scoping it. Note only that `dosfstools`/`fsck.vfat`
was proposed and **refused**, and appears nowhere in this document as a
recommendation.

## 2. `dosfstools` is installed by a committed CI workflow

`.github/workflows/probe-toolchain.yml:39` installs `dosfstools` alongside
`qemu-system-x86 ovmf`. It is the package the owner refused. The other seven
workflows do not.

It is a probe workflow (`on: push: branches: ['ci/probe-toolchain']`), so it
does not run on `main` — but it is committed, and a refused dependency sitting
in the tree is how it comes back. `specs/assessments/ci-plan-assessment-2026-08.md`
§6.1 discusses `fsck.vfat` as an option; the refusal is not recorded there.

**Recommendation.** Delete the word from that workflow; the refusal is now
recorded in `issues/build/dosfstools-installed-by-a-workflow.md`.

## 3. Python is a hard dependency of every toolchain build

`src/toolchain.rs:749` (and `:785`, `:799`):

```rust
let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
```

`rust/x` exists in the checkout. It is a `/bin/sh` script whose only job is to
find a Python:

```sh
*) SEARCH="python3 python py python2 uv";;
```

It then runs `rust/x.py`, which is a shim over `src/bootstrap/bootstrap.py`
(55,550 bytes). So **`cargo run` on a fresh clone requires Python 3**, and the
README's *"Rust and QEMU, one command"* is not true of the first run.

This is upstream's bootstrap, not ours, which is why it is stated as a fact
rather than as a defect of our code. But the bar does not have an upstream
exemption, and the self-hosting question answers itself: `bootstrap.py` can
never run inside ToyOS.

**Recommendation.** This is the largest single hole and the one with no cheap
fix — rust-lang's bootstrap has no Python-free entry point. Two honest options,
both the owner's call:

- **Declare it.** Say in the README and in the preflight that the first build
  needs Python 3, so the claim matches the machine. Costs nothing and stops the
  README being wrong.
- **Remove it**, which means a Rust `bootstrap` in the `rust/` fork — a large
  piece of work that would also be upstream-relevant, and squarely inside the
  self-hosting north star rather than a detour from it.

**DECLARED 2026-08-08**, the owner's ruling being *"its required by rusts
toolchain i guess we can be transparent about that."* The README's Prerequisites
section names it, the opening no longer claims Rust and QEMU are the whole
setup, and the preflight names it when it is absent. Declaring is not removing:
the hole is the same size and the second option is still open.

Named the way `rust/x` looks for it rather than as `python3` alone — the script
searches `python3 python py python2 uv` and then falls back to `bash -c
"compgen -c python"`, so a machine with only `python` satisfies it and a
preflight demanding `python3` would refuse a machine that builds.

Note that `--build-only` on a machine with a warm `rust/build/` does *not* re-run
`x`, so this bites a clean clone and a toolchain change, not every build.

## 4. The host C toolchain

Measured, not assumed: `rustc --print link-args` on a trivial host binary shows
`"cc"` as the linker driver, with `SDKROOT` pointing at the Xcode Command Line
Tools SDK. `rustup` does not install either. On Linux the equivalent is
`build-essential`, which `.github/workflows/toolchain.yml:60` installs
explicitly.

Scope, stated precisely so it is not overstated: **every host binary** — the
build system, the harness, `toyos-ld`, `toyos-cc`, `rustc` stage2 — links through
`cc`. **No guest binary does**: `bootloader/.cargo/config.toml` and
`kernel/.cargo/config.toml` both set `linker = "toyos-ld"`, and userland
cross-compiles through the same. The thing that boots is untouched by it.

**Verdict.** A dependency Rust itself has on every platform, that rustup does not
satisfy. It fails the letter of the bar and is not removable while `rustc`
targets the host through a C linker driver.

**Recommendation.** Declare it, as in §3. Do not chase it.

**DECLARED 2026-08-08.** README Prerequisites, and `REQUIRED` in the preflight.
Stated there with its scope attached both times, because *"ToyOS needs a C
compiler"* is false and reads as a much larger claim than the truth: no guest
binary links through it, and no image contains a C toolchain.

One property of the check is worth knowing before anyone trusts it: the binary
running the preflight was itself linked by `cc`, so the only way this arm can
fire is `cc` disappearing between two builds. It is there to be read, more than
to catch anything.

## 5. `df`, `ps` and `find`

Three Unix binaries, none of which comes with Rust or QEMU.

- **`df`** — `src/worktree.rs:141`, called from `worktree add` (`:74`) to report
  free space, `.expect("run df")`. A hard failure if absent. The number it wants
  is available from the platform without a subprocess.
- **`ps`** — `tests/common/hostload.rs:111`, counting concurrent `qemu` and
  `toyos-build` processes for gate A's `host:` annotation. Degrades correctly
  (`.ok()?` → `None`), so its absence loses a diagnostic rather than a run. Its
  neighbour in the same file, `getloadavg`, is already called through the `libc`
  crate rather than by shelling out — which is the shape.
- **`find`** — `toyos-fat32/tests/common/mod.rs:301`, deleting macOS `._*`
  resource forks from a freshly-populated volume. It is macOS-specific in intent
  even though the binary is not.

**Recommendation.** All three are small and none needs a new dependency. `ps` has
the answer beside it in the same file. `find`'s job is a directory walk. `df`'s
is one syscall. Judged against the bar these are unambiguous failures with cheap
fixes, and they are the least contentious items in this document.

**DECLARED 2026-08-08, not fixed.** All three are in the preflight's `ALSO_USED`
list, which names what is absent and continues, because each costs one feature
rather than the build. Declaring them is not the fix and does not close them —
the recommendation above still stands and is still cheap.

## 6. Doom's build downloads two unpinned things over the network

`userland/doom/build.rs` fetches, at build time:

| URL | What | Pin |
|---|---|---|
| `https://github.com/ozkl/doomgeneric/archive/refs/heads/master.tar.gz` | the doomgeneric C sources — everything `/bin/doom` is | **none.** A moving branch head. No commit, no tag, no hash check |
| `https://github.com/craffel/pretty-midi/raw/main/pretty_midi/TimGM6mb.sf2` | the GPLv2 SoundFont | **none.** `main` of a third party's unrelated repository |

Three separate findings here.

**a. It is a fork that is not a fork.** doomgeneric is a third-party C codebase
this project builds and ships. `forks.toml` says the sanctioned form is a real
repo with a pinned base and a `toyos` branch; doomgeneric has none. It is not in
`forks.toml` at all. The only pin is that `download_doomgeneric` runs once, when
`userland/doom/doomgeneric` does not exist — so what you build depends on **when
you first cloned**, and two developers can be building different Doom sources
with no way to tell. That is the reproducibility property `toyos-ld`'s
determinism tests exist to protect, lost at the source.

**b. The SoundFont is being fetched from a stranger's repository.** `craffel/
pretty-midi` is a Python MIDI library that happens to carry the file. Its `main`
branch is not ours to rely on, and the download is unauthenticated by anything
except TLS.

**c. Five crates exist only to serve this.** `ureq`, `rustls-rustcrypto`,
`webpki-roots`, `flate2` and `tar` are `userland/doom`'s build-dependencies and
have no other purpose in the tree. `rustls-rustcrypto` is pinned at
`"0.0.2-alpha"` — the manifest says so — which is hard to reconcile with *"only
general and often used rust crates"* on any reading.

**d. Measured today, by accident: the download races the suite.** The first
landing gate on this branch — a fresh worktree, which has no `assets/timgm6mb.sf2`
because it is gitignored — red on three tests with *"assets/timgm6mb.sf2 is
declared in `untracked-assets` and is not there"*: `metal_sim_compositor`,
`metal_sim_pointer_churn`, `boot_partition_identity`. By the end of that run the
file was on disk with an mtime inside it, put there by `download_soundfont`, and
all three passed alone. So the suite's greenness in a new worktree depends on
whether doom's build script has finished a network fetch before the initrd
builders of three configs ask for its output. Nothing sequences the two, and the
failure reads like a missing asset rather than like a race.

**Recommendation.** The SoundFont half is already being removed by another agent;
this entry records the class rather than duplicating that work. The doomgeneric
half is untouched and is the larger of the two: it should become a fork in
`forks.toml` like every other third-party source this project builds, at which
point the download and its five crates go with it. Nothing new is needed to do
that — the mechanism already exists and is used fourteen times.

### CLOSED 2026-08-08

**b and d were closed by the other agent** and are no longer in the tree:
`b8b0749` deleted `download_soundfont`, `95f78f3` deleted the four
`untracked-assets` declarations from configs that do not build doom and made a
remaining one non-fatal. Nothing writes into `assets/` during a build any more,
so nothing races the initrd builders. Verified by reading `build.rs` and
`src/assets.rs` on `b36cf64`, not by re-running the gate.

**a is closed by a pin rather than by a fork**, which the owner chose:

`DOOMGENERIC_COMMIT` in `userland/doom/build.rs` is
`fc601639494e089702a1ada082eb51aaafc03722`, the URL is that sha's archive, and
`forks.toml` `[doomgeneric]` records it under a new `source` tier — the manifest
had no shape for a third-party tree that is not a crate, which is part of why
this one was never in it.

**Which commit, and how it was established.** Not "whatever master is today":
the checkout that produced every doom measurement on record was hashed file by
file and compared against every commit on `ozkl/doomgeneric`'s master. 197 of
its 202 files match `fc60163`'s `doomgeneric/` tree exactly, and it is the
newest commit for which that holds. So the pin is what this project has been
building, and pinning changed nothing about the binary.

**The five files that did not match are the finding underneath the finding.**
Three (`i_sound.c`, `d_main.c`, `wi_stuff.c`) have later mtimes and identical
content — edited and reverted. Two carry real local edits, in a gitignored
directory, on one machine, in nobody's history:

```
m_controls.c:172   -int key_menu_incscreen = KEY_EQUALS;
                   +int key_menu_incscreen = '+';
doomgeneric.c:27   one trailing blank line removed
```

Nothing in the tree reads either — `grep -rn "incscreen\|KEY_EQUALS" tests/
userland/doom/` finds nothing — so the pin drops them, and a fresh clone never
had them anyway. **That is the shape a fetch cannot fix**: there is no `toyos`
branch for a ToyOS patch to the C to live on, so a patch lives in an untracked
directory or nowhere. `forks.toml`'s `followup` records the end state (a
`Japabu/doomgeneric` submodule, which deletes the fetch and the five
build-dependencies with it); it needs a repo the owner creates.

**The pin binds a checkout that already has one.** `doomgeneric/.toyos-commit`
records what is on disk, and a mismatch replaces the tree with a
`cargo:warning` naming both commits. Testing only whether the directory exists
*is* the original defect: without the stamp, this pin would have bound fresh
clones and left every existing machine exactly as it was.

## 7. Licences and provenance

The repository declares **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`,
README). The README declares two exceptions: `userland/doom` is GPL-2.0, and
third-party crates keep their own licences. It declares nothing else.

**Since 2026-08-08 it declares all of them.** `NOTICE` at the repository root is
the list, item by item, with `licenses/` holding four third-party licence texts
and `tests/testcases/LICENSE` the corpus's. The README's licence section points
at it and states the one term that constrains a build. Both of this repository's
own licence texts were checked while writing that and are present and complete:
`LICENSE-MIT` (25 lines) and `LICENSE-APACHE` (176 lines, the full terms
through "END OF TERMS AND CONDITIONS"; the optional "how to apply" appendix is
not carried, which is the usual Rust-project form).

Ten binary files are committed, totalling **14,634,080 bytes**. Four of them are
ours (§7g). The other **six — 11,094,369 bytes** — are third-party, and so is one
committed source corpus. Every provenance below was established by inspecting
content, because git history has none.

### 7a. `tests/testcases/tinycc/` — TinyCC's corpus, and one non-free file

**314 files** (157 `.c` plus their `.expect` files), plus **51** in
`tests/testcases/pp_tcc/`. These are TinyCC's `tests/tests2` and `tests/pp`
suites — confirmed by the `NN_name.c`/`.expect` convention and by
`tests/testcases/pp_tcc/19.c:84` citing `lists.nongnu.org/.../tinycc-devel`. The
README says so plainly: *"It also takes cases from TinyCC's own tests2 corpus"*.

**TinyCC is LGPL-2.1.** No `LICENSE`, `COPYING` or `README` accompanies the
corpus — `find tests/testcases -iname 'licen*' -o -iname 'copying*' -o -iname
'readme*'` returns nothing.

Worse, and the reason this is first: `tests/testcases/tinycc/46_grep.c` carries
its own header:

```
 *      Copyright (C) 1980, DECUS
 *
 * General permission to copy or modify, but not for profit,  is
 * hereby  granted, ...
```

**"but not for profit"** is a non-free restriction. It is incompatible with
MIT OR Apache-2.0, and it is committed in a repository that offers itself under
both. This is exactly the class the audit exists to find, and it was found by
grepping the corpus for the word "licen", which nothing in the tree does.

**Recommendation.** Establish what the corpus's licence actually permits and
either carry it with its notice or replace the cases. `46_grep.c` needs a
decision of its own regardless of what happens to the rest.

#### CLOSED 2026-08-08 — deleted, and the rest attributed

`46_grep.c` and `46_grep.expect` are gone, on the owner's ruling, and
`"46_grep"` is out of `C_SKIP` with them. It had never been run here — it needs
`argc`/`argv` and `FILE*`, neither of which `userland/libc` provides — so
nothing but the file count changed.

The rest is now attributed by `tests/testcases/LICENSE`. Two things there that
the audit did not have:

**Upstream ships the attribution and we had dropped it.** `tests/tests2/LICENSE`
exists in tinycc and says the corpus descends from **picoc** — so the terms are
LGPL-2.1 over a BSD-3-Clause base (Copyright (c) 2009-2011, Zik Saleeba), not
LGPL-2.1 alone. It is carried verbatim. LGPL-2.1 itself is confirmed from
tinycc's own `COPYING` and README; its `RELICENSING` file is an incomplete
effort with at least one author declining, so it does not change the answer.

**A third of the corpus is ours.** Every file was compared byte for byte against
tinycc at `64552b3` (2026-08-05), allowing for this project's `-` → `_`
filename renames:

| | `tinycc/` (312 after the deletion) | `pp_tcc/` (51) |
|---|---|---|
| byte-identical to upstream | 239 | 46 |
| upstream, modified here | 17 | 2 |
| not upstream at all | 56 | 3 |

One of those 56 is not a test: `tinycc/fred.txt` is 12 bytes of output
`40_stdio.c` writes when it runs, committed by accident. Left alone here —
`tests/` belongs to another agent this week — and worth deleting.

### 7b. `assets/DOOM1.WAD` — 4,196,020 bytes of id Software shareware

`IWAD` magic, 1,264 lumps, `sha256
1d7d43be501e67d927e415e0b8f3e29c3bf33075e859721816f652a526cac771`. This is the
Doom shareware IWAD. It ships in every image the project builds.

id Software's shareware licence permits redistribution under conditions
(unmodified, not sold, and so on). It is emphatically not MIT OR Apache-2.0, and
the README's GPL exception does not cover it — that exception is about
`userland/doom`'s *code*, and the WAD is data with a different licence again.

**Recommendation.** Name it in the README's licence section, or stop committing
it. Which one is the owner's call; the finding is that the repository currently
redistributes it while saying it redistributes only MIT/Apache material.

#### CLOSED 2026-08-08 — named, with the terms carried and cited

`NOTICE` leads with it and `licenses/id-software-shareware-DOOM1.txt` carries
id's Limited Use Software License Agreement in full, obtained from Debian's
`doom-wad-shareware` copyright file — which is a citable source rather than a
recollection, and reproduces both the agreement and Carmack's 1999 email
(*"The DOOM shareware wad is freely distributable."*).

The two clauses that bind are §1 (no modification, no derivative works) and §2
(copies may be given to other persons; no consideration may be charged or
received for them without id's prior written consent). **An image carrying this
file may not be sold**, and NOTICE and the README both say so.

Corrected while writing it: it does **not** ship in "every image". `assets` is
declared by `system.toml`, `console/system.toml` and three test configs, and
`diag/system.toml` declares none — so `bootable-diag.img` carries no WAD, no
wallpaper, no font and no icons.

The entry is written to be deleted. The owner, 2026-08-08: *"we can make doom
optional later. its not a required part of toyos and the future vision is to
download it via package manager."* When the WAD stops being committed the
section goes; it must not decay into a permanent-sounding notice for a file
that is no longer there.

### 7c. `assets/JetBrainsMono-Regular.ttf` — OFL 1.1, notice not carried

Read out of the font's own name table:

> Copyright 2020 The JetBrains Mono Project Authors …
> This Font Software is licensed under the SIL Open Font License, Version 1.1.

Version 2.305. The OFL requires the copyright notice and licence to travel with
the Font Software. No OFL text is in the repository.

Two derived artifacts inherit the question:
`kernel/src/drivers/panic_console/font8x16.bin` (1,520 bytes, committed) and the
`share/fonts/*.font` tables `src/assets.rs` rasterizes into every initrd. Whether
a 1-bit rasterization is "the Font Software" is a real question and not one to
guess at.

**Recommendation.** Carry the OFL text. It is a file.

#### CLOSED 2026-08-08

`licenses/OFL-1.1-JetBrainsMono.txt`, taken from the JetBrains Mono
distribution so it arrives with their copyright line above the licence, which
is the form the OFL asks for. `NOTICE` names the font and both derived
artifacts. The rasterization question is recorded there and not answered —
answering it is not an agent's to do. The OFL's reserved-name clause is not
engaged either way: neither artifact is distributed as a font under the name.

### 7d. `assets/icons/*.svg` — Phosphor Icons, MIT, attribution not carried

All eight are **byte-identical** to files in the Phosphor Icons distribution:
`crosshair-simple-bold`, `minus-bold`, `square-bold`, `x-bold` match
`SVGs/bold/`; `arrow-down-right-bold`, `cursor-bold`, `file-bold`, `folder-bold`
match `SVGs Flat/bold/`. Verified with `cmp` against
`/Users/jan/Dev/jan/toyos/phosphor-icons/`, which holds `LICENSE`: MIT,
"Copyright (c) 2020-2024 Phosphor Icons".

That directory is **gitignored** (`.gitignore` line 7, `phosphor-icons`), so the
licence text is on the owner's disk and not in the repository. MIT requires the
notice to be included in copies. The icons ship in every initrd as
`share/icons/*.svg`.

**Recommendation.** Carry the MIT notice for them. Same shape as 7c.

#### CLOSED 2026-08-08

`licenses/MIT-PhosphorIcons.txt`, and `NOTICE` names all eight with the
subdirectory each came from. The byte-identity was re-verified rather than
taken from this document: four match `SVGs/bold/` and four `SVGs Flat/bold/`,
which is what the audit found and what a second `cmp` sweep confirms.

### 7e. `ovmf/*.fd` — third-party firmware blobs, 6,291,456 bytes

Three committed files, and they are load-bearing: `src/qemu.rs:101,103` and
`tests/common/qemu.rs:2090,2095` point QEMU's pflash at
`ovmf/OVMF_CODE-pure-efi.fd` and `ovmf/OVMF_VARS-pure-efi.fd`, so every boot on
the dev host uses them.

Provenance, read out of the binary: built from EDK II at `edk2-gf0064ac3af` on a
Jenkins worker, path `edk2/rpms/build/edk2-gf0064ac3af.EOL.no.nore.updates/`.
That is a distro/third-party build of Tianocore EDK II, which is
BSD-2-Clause-Patent. No `LICENSE`, no version record, no build recipe.

An oddity worth flagging to the CI agent rather than acting on: every workflow
installs the `ovmf` apt package, and the harness then ignores it and uses the
committed blobs.

**Recommendation.** Either treat these as a declared third-party artifact with a
recorded upstream, version and licence, or take the firmware from QEMU's own
installation — which would put it inside the "comes with QEMU" allowance and
delete 6.3 MB from the repository. The second reading is available because
firmware is QEMU's input, not ours.

#### CLOSED as attribution 2026-08-08; the two questions stay open

`licenses/BSD-2-Clause-Patent-EDK2.txt` is EDK II's own `License.txt`, and
`NOTICE` records each file's size, hash and the build path read out of it.
Neither of the recommendations above is taken — both are the owner's, and
declaring the artifact is what makes either of them a decision rather than a
discovery.

**One correction to the paragraph above: "three committed files, and they are
load-bearing" is wrong about the third.** `ovmf/DEBUGX64_OVMF.fd` is
4,194,304 bytes that nothing in the tree references — `grep -rn 'ovmf\|OVMF'`
over every `.rs`, `.toml` and `.yml` outside `target/` and `rust/` finds
`OVMF_CODE-pure-efi.fd` and `OVMF_VARS-pure-efi.fd` at `src/qemu.rs:101,103`
and `tests/common/qemu.rs:2207,2212`, and no reader of the third at all. It is
also a *different* build from the other two: its embedded path is `/src/edk2`,
not the Jenkins worker's. So it is a third of the repository's firmware bytes
serving nothing that has been found.

### 7f. `assets/wallpaper.jpg` — CLOSED 2026-08-08: the file is now generated

The finding was that the old 336,669-byte file's provenance could not be
determined from its content: JFIF, an EXIF block holding four tags with **no
`Artist` and no `Copyright`**, a Photoshop 3.0 IRB, nothing in git history and
no mention in the README.

**The owner recalled the source, and it made the answer worse rather than
better.** peakpx.com: a page that names no author and no copyright holder, says
the image was *"uploaded by our users"*, and states *"License: Wallpaper use
only, DMCA"*. So there was a documented restriction, granted by an aggregator
with a takedown form and no standing to grant one — and "wallpaper use only" is
precisely the clause a redistributable OS image violates. A named licence from
a party who cannot license is not an improvement on an unknown.

**His decision was to generate one.** `src/wallpaper.rs` draws the background as
a pure function of the target size — a night sky, a glow at the horizon and
three ridges, composited in linear light and dithered before the 8-bit encode —
and `cargo run -- --regen-wallpaper` writes it, the same shape `--regen-font`
already had for the panic console's table. The pipeline is untouched:
`src/assets.rs` still pre-decodes the JPEG into `share/wallpaper.rgb` and the
compositor still scales and blits it, so no code outside this file changed.

**Nothing else in the tree changed to accommodate it, including `image`.** The
crate's removal was priced in `specs/assessments/dependency-purpose-2026-08-08.md` at nine
crates against a wallpaper drawn at runtime; the owner chose a file, and a file
in the pipeline needs a decoder. It also encodes, which is why generating one
needed no new dependency.

**What keeps this closed** is `the_committed_wallpaper_is_the_one_this_file
_describes`, which compares the committed bytes against the generator's output
on every `cargo test`. Someone dropping a picture at that path reopens the
finding and the build says so; there is no path back to an asset nobody can
account for. Its sibling `the_wallpaper_neither_bands_nor_blocks` is the quality
half — see 7f.1.

#### 7f.1. Why the encoder settings are what they are

Measured on the finished drawing, one session, `cargo test --lib`. `bytes` is
the JPEG; `run` is the longest column of one unchanging green value in the
*decoded* image, which is what banding is; `block` is the mean step across an
8×8 block boundary over the mean step inside one, which is what JPEG blocking
is. A picture with neither artifact sits at `block ≈ 1.0` and `run ≈ 20`, which
is where the dithered source itself sits.

| quality | bytes | max err | block | run |
|---|---|---|---|---|
| source | 6,220,800 | — | 1.005 | 20 |
| 92 | 97,623 | 18 | 3.656 | 112 |
| 95 | 158,860 | 14 | 2.470 | 59 |
| 97 | 199,955 | 10 | 2.378 | 80 |
| 98 | 278,284 | 10 | 2.004 | 44 |
| **99** | **761,404** | **6** | **0.938** | **21** |
| 100 | 762,751 | 9 | 1.123 | 29 |

The cliff between 98 and 99 is the quantization table: libjpeg's scaling leaves
every entry at 1 by quality 99, and `image`'s encoder never subsamples chroma,
so the round trip becomes the DCT's own rounding. Below that the DC coefficient
alone is quantized in steps of two or three levels — an 8×8 staircase across a
field that only sweeps a few dozen levels in total. 100 is the same size to
within 0.2% and worse on all three measures, so 99 it is.

The dither is load-bearing and its amplitude was measured the same way, at
quality 99:

| grain (levels) | bytes | run |
|---|---|---|
| 0.0 | 154,294 | 158 |
| 0.8 | 367,078 | 106 |
| 1.2 | 567,102 | 37 |
| **1.6** | **761,404** | **21** |
| 2.4 | 1,080,183 | 14 |

At 1.6 the encoded artifact is back at the source's own noise floor, which is
the stopping point: more grain buys nothing the eye can see and costs bits
linearly. Undithered, the same picture has 158-row flat bands — that is the
defect these numbers exist to keep out, and it is what the test's bound of 40
is set against.

The artifact costs 424,735 bytes more in git than the file it replaces and
**nothing at all in the image**: the initrd carries the decoded
`share/wallpaper.rgb` either way, 6,220,808 bytes.

#### STILL OPEN, and deliberately — 2026-08-08

Re-checked rather than repeated: `strings` finds JFIF, an Exif block and a
Photoshop 3.0 IRB and no credit of any kind; there is no IPTC block; `mdls`
reports only filesystem dates and an sRGB profile. No `Artist`, no `Copyright`,
no author string anywhere in the file.

`NOTICE` carries it as an entry that names the gap in those words. That is the
whole of what an agent may do here: **inventing a licence for it would be worse
than the open question**, because the next reader would have no reason to doubt
it. It ships as `share/wallpaper.rgb` in the default and console images and
therefore in anything the owner hands to anyone. The decision — recall its
origin, or replace it — is his, and it is the one item in §7 that is still live.

### 7g. Clean by inspection

- `doom.jpg`, `first-boot.jpg` — the owner's own photographs, README figures.
- `toyos-elf/tests/fixtures/toyos-ld-headers.bin` — output of our own linker.
- `toyos-hda/fixtures/alc257-t14.txt`, `qemu-intel-hda.txt` — dumps taken by our
  own H0 probe from the owner's T14 and from QEMU.
- `assets/hello.rs` — 55 bytes, ours.

---

# Part II — WHAT PASSES

## 8. Crates

**43** distinct third-party crates are named directly in our own **52**
manifests (measured by extracting non-`path` entries from every
`[dependencies]`, `[dev-dependencies]` and `[build-dependencies]` section):

```
cc  core(rustc-std-workspace-core)  cpal  cranelift-codegen  cranelift-frontend
cranelift-module  cranelift-object  dlmalloc  elf  fatfs  flate2  fontdue  gpt
hashbrown  image  libc  libloading  loom  object  rand  resvg  rubato  russh
rustc-demangle  rustls-rustcrypto  rustysynth  serde  serde_json  sha2  smoltcp
softbuffer  tar  target-lexicon  thiserror  tokio  toml  uefi  uefi-services
ureq  uuid  webpki-roots  winit  zerocopy
```

Judged against *"general and often used"*:

- **35 pass without comment.** Every one is a first-rank crates.io crate doing a
  general job: serialization, compression, cryptographic hashes, ELF/object
  decoding, a code generator, a TCP/IP stack, a windowing abstraction, an
  allocator, a resampler, an SVG renderer.
- **1 fails on its face**: `rustls-rustcrypto = "0.0.2-alpha"`. §6c.
- **4 exist only to serve the download** in §6: `ureq`, `webpki-roots`, `flate2`,
  `tar`. Each is a perfectly ordinary crate; the finding is what they are for.
- **3 duplicate code this project already owns.** Not violations — observations
  the owner may want:
  - `fatfs` — we have `toyos-fat32`, read and write, and `src/image.rs` already
    writes the ESP with it. `fatfs` now only *formats* the empty volume
    (`src/image.rs:131`). It also serves a second and better purpose: it is an
    independent implementation reading back volumes our own code wrote
    (`tests/common/volumes.rs:11` says so explicitly). A retirement candidate for
    the format path, a keeper for the judge path.
  - `gpt` — we have `toyos-gpt`. Same split: `src/image.rs:386-432` builds the
    partition table with it, `tests/common/gpt.rs` and `volumes.rs` use it as an
    outside judge.
  - `elf` — we have `toyos-elf`, which CLAUDE.md describes as doing headers,
    `PT_DYNAMIC`, relocations, **symbols**, `.gnu.hash` and TLS arithmetic. The
    `elf` crate is used anyway in `kernel/src/symbols.rs` and
    `bootloader/src/main.rs:10`, at **two different major versions** — `0.8` in
    the kernel, `0.7.4` in the bootloader — for one job. Two ELF parsers in one
    kernel image is the clearest duplication in the list.

**Nothing in the direct set is a driver crate or a wrapper around one tool.**
The estate is clean on the rule the owner stated most sharply.

### Transitive weight

**471** distinct packages appear across the 28 lockfiles; `userland/Cargo.lock`
alone locks **448**. A separate observation, not a verdict — the bar applies to
what we chose. Two things are worth saying about it:

- Most of the tail is `cpal` and `winit`'s other platform backends, locked and
  never compiled: `alsa`/`alsa-sys`, `coreaudio-rs`, the `objc2-*` family,
  `jni`/`ndk`, the `windows-*` family, `orbclient`/`redox_syscall`,
  `wasm-bindgen`/`web-sys`. They are lockfile entries, not builds.
- `ring` is in there via `rustls-webpki`, and `ring` needs a C compiler. **It is
  not built** — verified, no `ring-*` directory exists under any `target/`. The
  `rustls-no-provider` + `rustls-rustcrypto` arrangement at
  `userland/doom/Cargo.toml:16,18` is what keeps it out. That is a real property
  worth knowing, and nothing enforces it.

## 9. The fork estate

**14** clones under `/Users/jan/Dev/jan/forks/`, **15** under
`~/.cargo/git/checkouts/`, **14** entries in `forks.toml` plus `rust/` as a
submodule. Enumerated because `issues/` records that the estate is outside
every check the tree runs on itself.

**Result: clean on both bars.** Every fork's manifest delta against its pinned
base was read (`git diff <base>..toyos -- '*Cargo.toml'`). What the forks add:

| Fork | Adds |
|---|---|
| cpal, socket2, mio, getrandom ×3, stacker, libloading | `toyos-abi` / `toyos`, by **version** |
| softbuffer, winit | `window` by version, plus a `raw-window-handle` patch redirect |
| tokio | a `mio` patch redirect, and a `cfg` value |
| russh | `aes-gcm`, `chacha20`, `poly1305` — RustCrypto, general-purpose, for the pure-Rust backend |
| raw-window-handle, target-lexicon | nothing |
| memmap2, ctrlc | a version relabel only |

**No fork introduces a crates.io dependency that is not already general-purpose,
and no fork's ToyOS delta invokes an external binary.** Licences declared in the
fork manifests: Apache-2.0 (cpal, russh), MIT (tokio), MIT OR Apache-2.0
(getrandom, softbuffer, stacker), "Apache-2.0 AND MIT" (winit). All permissive;
every clone carries its upstream `LICENSE` file.

One arrangement worth naming, because it is the exception the rule does not
mention: `rust/library/std/Cargo.toml` depends on `toyos-abi` and `toyos` **by
path** (`../../../toyos-abi`), which the fork rule forbids. It resolves because
`rust/` is a submodule that only ever exists inside the monorepo, not a
standalone crate fork — but it is the same shape as the violation the rule
exists to prevent, and `issues/build/` already records the ordering constraint it
creates.

## 10. What passed, counted

- **35** of 43 direct crates, unremarked.
- **14** forks, all of them, on both the crate bar and the binary bar.
- **5** external binaries: `git`, `cargo`, `rustc`, `rustup`,
  `qemu-system-x86_64`.
- **4** committed binary artifacts of our own making (two photographs, a linker
  fixture, a rasterized font table whose *source* is 7c's open question).
- **2** HDA fixtures, both our own dumps.
- **3** cargo configs, all pointing at `toyos-ld`; no guest binary links through
  anything we did not write.

---

# Part III — HOW THE TREE CHECKS ITS OWN BAR

## 11. The mechanism question

A one-time list decays. Both of today's violations existed for months and were
found by collision. The question is what mechanism would have caught them, and
what it would cost.

**The binding constraint is already established.** The same question was answered
today for fork pins, and the conclusion was that **anything touching the network
must be an on-demand command, never part of `cargo test` or the landing gate.**
That applies here without modification: a check that asks crates.io how popular a
crate is, or GitHub what a licence says, cannot be a gate. Everything proposed
below is offline and reads only files already on disk.

**This section proposed four mechanisms. Decided 2026-08-08: one was built and
three were refused, so read what follows as a record rather than as a menu.** The
owner accepted the fork-pin check — `cargo run -- --check-forks`, §11.6 — and
**rejected the three offline ledgers of §11.1–§11.3 as brittle**. They are left
below unedited because the reasoning is worth reading and because a proposal that
was put and answered should not be put again as if it were new. §11.5's ranking
is historical for the same reason.

The concern the ledgers were written against still stands: every one of them
would have gone red on the tree as it was written, and a red gate landed into
`--land` breaks every other agent's worktree. What was built goes red on the
state of the world instead, and is not a gate at all.

### 11.1 A crate ledger — the one with real teeth

**What.** A committed file naming every third-party crate the tree may resolve,
with a one-line reason. A `#[test]` in `src/` unions the `name = ` lines of all
28 `Cargo.lock` files and compares. A crate not in the ledger is red, by name,
with the lockfile that pulled it in.

**Why this one first.** It does not judge crates — it forces a human to judge one
at the moment it arrives, which is the step that was missing. Both of today's
violations are the same shape: something entered and nothing asked.

**Cost.** Reading 28 lockfiles and one data file: milliseconds, no network, no
guest. It fits inside `cargo test --lib`, which the landing gate already runs.
The seeding cost is the real one — 471 names to classify once, of which 43 are
direct and the rest are a signature.

**What it cannot catch.** Whether a crate is *good*, only whether it is *new*. A
crate already in the ledger that turns bad. A dependency added to a fork (the
forks resolve into the same lockfiles, so this is partly covered — but only for
workspaces that have been re-resolved). It says nothing about licences.

### 11.2 An external-binary ledger

**What.** The same shape, for `Command::new` string literals and for
`"/sbin/…"`-style path literals, scanned across every `.rs` file and every
`.github/workflows/*.yml` (apt/brew package names included).

**Cost.** A directory walk. Milliseconds.

**What it cannot catch, and this is the important part.** `Command::new(tool)`
where `tool` is a variable — which is how **three of the four** macOS tools are
invoked. Scanning path-shaped literals as well recovers `/sbin/fsck_msdos`,
`/sbin/newfs_msdos` and `/usr/bin/hdiutil`, but `Command::new("find")` is
indistinguishable from a string that happens to say "find". It also cannot see a
binary a third-party build script invokes, or anything `rust/`'s bootstrap does.

So: it would have caught `dosfstools` in the CI YAML immediately, and the three
macOS tools via their absolute paths. It would **not** reliably have caught
`find`, `ps` or `df`. Honest verdict: useful, and weaker than the crate ledger.

### 11.3 An asset ledger

**What.** Every file git tracks that is binary, plus every third-party source
corpus, listed with `sha256`, upstream, licence, and whether it ships in the
image. A new binary file, or a changed hash, is red.

**Cost.** Hashing 14,634,080 bytes: tens of milliseconds.

**What it cannot catch.** Whether the licence recorded is true. It is a ledger,
not a lawyer. But every one of §7's seven findings is a row that would have been
empty, and an empty row is a question somebody has to answer before landing.

### 11.4 What no offline mechanism reaches

Stated plainly so the proposal is not oversold:

- **`rust/`'s own dependencies.** Python, `cmake`/`ninja` when LLVM is built from
  source, and the prebuilt CI LLVM the bootstrap downloads (present on this host:
  `rust/build/aarch64-apple-darwin/ci-llvm` and `rust/build/host/ci-llvm`, with
  two `llvm-<sha>-false` caches under `rust/build/cache/`). That set changes when
  upstream changes it, and nothing we write sees it.
- **What a crate's build script does.** `ring` is the worked example.
- **Truth of a licence claim.**
- **A fork clone that is path-overridden.** `.cargo/config.toml` is gitignored and
  per-developer, so what a given machine actually builds can differ from what the
  ledgers describe.

### 11.5 If only one thing is built

**§11.1.** It is the cheapest, the hardest to fool, and it closes the class both
of today's violations belong to: something arrived and nobody was asked. §11.3 is
second and would have caught five of §7's seven findings on its own. §11.2 is
third and is the weakest of the three, which is worth knowing before anyone
spends a day on it.

### 11.6 What was built — `cargo run -- --check-forks`

`src/forkcheck.rs`. It takes the fork inventory from `forks.toml`, the
**consumed** branch from the `[patch]` and git-dependency entries of every
manifest — never from `forks.toml`, which records a `pr_branch` for
`raw-window-handle` and `target-lexicon` that is deliberately not the branch
cargo resolves — asks each remote for that branch's head with `git ls-remote`,
and compares the answer against every lockfile that pins it.

- **Every lockfile that holds a fork pin**, `rust/Cargo.lock` and
  `rust/compiler/rustc_codegen_cranelift/Cargo.lock` among them. `rust/` is an
  empty stub in a linked worktree, so it is walked through
  `toolchain::rust_dir`. A check reading one lockfile could not have seen
  `libloading` pinned at two revisions at once, which is what §11 was written
  about.
- **It reports and never re-pins.** Each drift comes with the
  `cargo update --manifest-path … -p <crate>@<version>` that would fix it, and
  that is where it stops: a helper that re-pins on its own lands a dependency
  change nobody reviewed.
- **On demand only, and it says so on every run.** It needs the network, so it
  is in neither `cargo test` nor `--land` — the constraint this section's
  preamble settled, restated in the command's own banner because that is the
  line the next person adding a check reads. Exit 1 if any pin is behind or any
  remote could not be reached.
- A `forks.toml` entry no manifest consumes — the shape doomgeneric has, being
  fetched by `userland/doom/build.rs` rather than by cargo — is a line under
  `not compared:`, never a crash.

Measured on `b36cf64`, 2026-08-08: **14 remotes, 16 branches, 7 lockfiles, 38
pins, all current**, 8.3 s wall. Its teeth were shown against the real tree by
checking the five non-submodule lockfiles out at `b15e54e^`, the state before the
re-pin commit: it named all six drifts that commit fixed — `raw-window-handle` at
`76c4971c` and `libloading` at `2ca5f54b` in two lockfiles among them — and
exited 1. Five unit tests in the module keep it honest with a local git
repository standing in for a remote, so they need no network and run inside
`cargo test --lib`.

---

## 12. Cross-references

Filed in `issues/build/` (Build and toolchain), each pointing here:

- Python and the host C toolchain as undeclared build prerequisites (§3, §4).
  **Now declared** — the entry says so and stays open, because declaring is not
  removing.
- `df`, `ps` and `find` as external binaries (§5); `find` is new to the list.
  **Now declared, still there.**
- doomgeneric downloaded unpinned from a moving branch (§6). **CLOSED.**
- `dosfstools` in a committed workflow (§2). Open; the CI agent owns it.
- The licence and provenance gaps (§7), including the DECUS "not for profit"
  file, which is the sharpest of them. **CLOSED except `assets/wallpaper.jpg`**,
  which only the owner can close.

Not filed, because another agent owns them: the three macOS FAT tools
(replacement being scoped) and `assets/timgm6mb.sf2` (being removed).
