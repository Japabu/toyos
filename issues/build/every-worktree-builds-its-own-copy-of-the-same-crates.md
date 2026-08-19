---
status: open
kind: defect
opened: 2026-08-19
---

# Every worktree builds its own copy of the same crates, and one shared target directory removes almost all of it

Twenty-four linked worktrees hold twenty-four copies of one compilation. The
primary checkout's `target/` is 16 GB, and cargo's share of it is 12.5 GB
(`du`, 2026-08-19): `debug` 11 GB, `x86_64-unknown-toyos` 895 MB, `release`
286 MB, `aarch64-apple-darwin` 285 MB. The remaining 3.5 GB — `bootable*.img`,
`nvme.img` 1.0 GB, the staged `kernel-*`/`bootloader.efi-*` copies, `stamps/` —
is the build system's own output and is **per-worktree by design**: `kernel_key`
hashes profile and features and not content, and `buildlock::artifact` is a lock
under `<worktree>/.build-locks`. Only cargo's 12.5 GB is a candidate for
sharing. Beside `target/`, a worktree also holds `kernel/target` 318 MB,
`bootloader/target` 194 MB and `userland/target` 1.4 GB.

## One shared `CARGO_TARGET_DIR` is not the answer: measured, it silently swaps branches

The first draft of this entry proposed `<primary>/target` for every host-workspace
member, on the premise that "cargo does not key artifacts on the worktree path,
fingerprints are content based, so even the workspace-local crates are reused
when the source matches". **Half of that is true and it is the wrong half.**
Cargo indeed does not key on the path — and it does not compare content either.
Freshness for a path package is *mtime*, so a checkout whose sources are merely
*older* than another checkout's build is declared fresh and never looked at.

Measured 2026-08-19 on this host, two trees over one `CARGO_TARGET_DIR`: A is a
worktree, B an APFS clone of it with `toyos-fat32/src/lib.rs` and
`src/redlist.rs` edited.

| | crates compiled | wall clock |
|---|---:|---:|
| A into an empty shared dir | 140 | 31.59 s |
| B, two files genuinely different | **0** | 0.08 s |
| B again, after adding `pub static MEASUREMENT_MARKER` with an mtime older than A's build | **0** | 0.02 s |

After the third row the shared `libtoyos_fat32-78eb18c4401b513b.rlib` carried
none of B's code, and B's build had reported `Finished`. Then B's file was
touched to the current time: B compiled it, the marker appeared in the rlib —
and the next `cargo build` in **A** printed `Finished` in 0.01 s and left B's
rlib in place. A links B's code, silently, with no diagnostic anywhere.

With one target directory each, both trees compile and each gets its own
artifact — and both write the file name `libtoyos_fat32-78eb18c4401b513b.rlib`.
That identical name is the collision: cargo's `-C metadata` does not carry the
checkout path, so two checkouts of one package are one artifact.

**It is the common case, not a corner.** `git worktree add` stamps every file at
creation. A worktree cut at 10:00, another checkout building at 10:05, this one
building at 10:06: every file this branch did not itself edit is older than that
fingerprint, so it compiles nothing and runs the other branch's binaries. The
"0 recompiles for the second worktree, 3 for a different branch" that this entry
was opened on were mtime coincidences, not content agreement — the 3 were the
files that branch had edited recently enough.

The blast radius is the whole tree, not one rlib: `toyos-ld` and `toyos-cc` are
host-workspace members, and they are the linker and C compiler every guest
binary is built with.

## Correct sharing exists, is measured, and is nightly

`cargo -Z checksum-freshness` — "Use a checksum to determine if output is fresh
rather than filesystem mtime" (`cargo -Z help`, cargo 1.97.1). Same two trees,
same shared directory, `--workspace`:

| | crates compiled | wall clock |
|---|---:|---:|
| A into an empty shared dir | 140 | 42.30 s |
| B | 2 | 8.01 s |
| A | 2 | 10.15 s |
| B | 2 | 5.41 s |
| A | 2 | 4.61 s |

Every alternation recompiled exactly the two crates that differ and nothing
else, and the marker followed the tree that built last. So the thrash is bounded
by the divergence — but the flag is unstable, and the host workspace builds with
stable cargo, so the measurement above needed `RUSTC_BOOTSTRAP=1`. Whether this
project buys a shared target directory at that price is the owner's to decide.

## The lock question is answered, and favourably

Cargo holds the build-directory lock for the **compile phase only**. A `cargo
test` whose test sleeps 30 s, and a second checkout's `cargo build` on the same
directory at t=+5 s: the probe recompiled and finished in **0.05 s**, with no
`Blocking` line. Positive control, the same probe against a 15 s build script:
it blocked **12.58 s** and printed `Blocking waiting for file lock on build
directory`, releasing the moment the compile ended. A shared directory would
cost the compile phase and never the guest phase.

## Where it would go, if it goes anywhere

`hostws::target_dir` is the function that answers "where did cargo put this
crate's output", and it is the only place the answer should be derived —
`<primary>/target` via `primary_checkout()`, degenerating to `<root>/target`
where there are no worktrees. Two sites build the path themselves and would have
to go through it: `src/build.rs`'s `stage_artifact` and `src/pr.rs`'s merge-file
directory. Two more are target-directory computations in disguise:
`toolchain::toyos_ld_binary` and `toyos_cc_binary`.

It also needs an absolute `build.target-dir` in each worktree's gitignored
`.cargo/config.toml`, because agents type `cargo test` by hand. Measured: such a
config **also redirects every nested workspace** — a `.cargo/config.toml` below
it that does not set `target-dir` inherits it, so `kernel/`, `bootloader/`,
`userland/` and the crates under `tests/` would move too unless every cargo
invocation in the build system passes `--target-dir` explicitly. `cargo clean`
follows it as well, so a hand-typed clean in one worktree would empty the
directory every worktree shares.

## The owner ruled: try it — and the mechanism holds

2026-08-19, on the section above: *"an experimental feature is not a workaround
— try it."* So it was tried, on this repository rather than on a two-file
fixture, and the mechanism is sound. What stops it is not the feature.

Two APFS clones of a worktree, `toyos-fat32/src/lib.rs` and `src/redlist.rs`
edited in one of them, one shared `CARGO_TARGET_DIR`, `cargo build --workspace`
alternating. The marker is `pub static SHARED_TARGET_MARKER` in `toyos-fat32`,
read back out of the rlib with `strings`. B's two edited files are stamped
2020-01-01, which is what a worktree cut before another checkout's build looks
like to cargo.

Plain cargo 1.97.1, the default toolchain — **the hazard, reproduced**:

| | crates compiled | wall clock | the shared rlib holds |
|---|---:|---:|---|
| A into an empty shared dir | 140 | 30.55 s | A |
| B | **0** | 0.09 s | **A** |
| A | 0 | 0.08 s | A |
| B | **0** | 0.08 s | **A** |

B printed `Finished` twice and never once held its own code.

`RUSTC_BOOTSTRAP=1 CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true`, same trees, same
directory — **the hazard, gone**:

| | crates compiled | wall clock | the shared rlib holds |
|---|---:|---:|---|
| A into an empty shared dir | 140 | 30.38 s | A |
| B | 2 | 3.69 s | **B** |
| A | 2 | 3.71 s | **A** |
| B | 2 | 3.69 s | **B** |
| A | 2 | 3.72 s | **A** |

Every alternation recompiled exactly the two crates that differ, and the marker
followed the last builder every time and never leaked across. 1.6 GB for both
trees. The artifact both trees contend for is
`debug/deps/libtoyos_fat32-78eb18c4401b513b.rlib` — byte-for-byte the same name
this entry reported before, from a different pair of checkouts, which is the
collision restated.

The lock behaves the same under the flag as without it. A 30 s test *run* phase
in one checkout, a second checkout's `cargo build` on the same directory at
t=+5 s: **0.134 s**, no `Blocking` line. Positive control, the same probe
against a 15 s build script: **12.655 s** and `Blocking waiting for file lock on
build directory`. The compile phase is what a shared directory serialises, and
nothing else.

## Where it stops: nothing can turn the flag on for a hand-typed cargo

`-Z checksum-freshness` is honoured when the cargo that runs is nightly-capable
and ignored when it is not — and **ignored silently**. The probe is a `touch`
that changes no content: an mtime cargo recompiles, a checksum cargo does not.

| how it was asked for | verdict |
|---|---|
| stable cargo 1.97.1, `[unstable] checksum-freshness = true` in `.cargo/config.toml` | **mtime.** No warning, exit 0 |
| stable cargo, `CARGO_UNSTABLE_CHECKSUM_FRESHNESS=true` | **mtime.** Ignored |
| stable cargo, `[env] RUSTC_BOOTSTRAP = "1"` in `.cargo/config.toml` | **mtime.** `[env]` reaches what cargo spawns, not cargo |
| stable cargo, `-Z checksum-freshness` on the command line | error, exit 101 — loud, but only the build system types a flag |
| stable cargo, `RUSTC_BOOTSTRAP=1` + either of the first two | checksum |
| nightly cargo, `[unstable]` table | checksum |

So the only channel-independent switch is `RUSTC_BOOTSTRAP=1` **in the process
environment**, and no file in the tree can put it there. A `.cargo/config.toml`
carries the shared `target-dir` to a hand-typed cargo perfectly well; it cannot
carry the freshness mode with it. That pair — sharing on, freshness off, no
diagnostic — is exactly the mis-link in the first table.

**And the fork does not supply the nightly cargo.** `rustup run toyos cargo
--version` answers `cargo 1.96.0-nightly (f298b8c82 2026-02-24)`, but
`toolchain::host_cargo` is why: it symlinks `stage2/bin/cargo` to
`~/.rustup/toolchains/nightly-<host>/bin/cargo` **if this machine has one**, and
to the host's stable cargo otherwise. Every CI runner installs `--profile
minimal --default-toolchain stable`, so there the `toyos` toolchain's cargo *is*
stable's. The nightly is the dev host's, not the fork's.

This host's rustup default is `stable-aarch64-apple-darwin`. A hand-typed `cargo
test --workspace --exclude toyos-build` here is an mtime cargo, and that is the
command agents type most.

The two ways to make the invoking cargo nightly-capable both cost more than the
directory is worth, and neither is an agent's to choose:

- **a generated `rust-toolchain.toml`** (channel `nightly`, or `toyos`) — needs
  `/rust-toolchain.toml` added to `.gitignore` to stay untracked, and makes the
  host crates compile with a different compiler locally than in CI, which
  installs stable. With `channel = "toyos"` the guarantee is only as good as the
  machine having a rustup nightly, which is the assumption that just failed.
- **a sentinel that makes a stable cargo refuse** — `-Zunstable-options` in the
  generated config's host-triple `rustflags` errors on a stable rustc and is a
  no-op on a nightly one, so it tracks the freshness predicate exactly. It also
  makes the worktree unbuildable with this machine's default toolchain, which is
  the ergonomics the sharing was for.

Mixing is not a third hazard, and that is worth recording: a directory whose
fingerprints were written under checksum-freshness makes an mtime cargo
*recompile* rather than reuse (measured with one rustc, `RUSTC_BOOTSTRAP` the
only difference), so a stray stable build costs a rebuild and not a wrong link.
But two stable builds in one directory mis-link each other exactly as the
control does. "Usually nightly" is not a safety property.

## What is left

The feature works, the numbers are good, and 76 GB is what the checkouts hold
today (`du -sch` over every `target`, `kernel/target`, `bootloader/target` and
`userland/target`, 2026-08-19). **One decision unblocks it and it is the
owner's**: what compiler a hand-typed `cargo` runs in a worktree. Make this
host's rustup default nightly-capable and the design is a generated
`.cargo/config.toml` carrying the absolute `build.target-dir` and the
`[unstable]` table, plus `target-dir = "target"` in `kernel/`, `bootloader/` and
`userland/`'s committed `.cargo/config.toml` to stop the inheritance measured
above — nothing else is needed, and CI, which has no worktrees, keeps
`<root>/target` and stable cargo untouched.

Two measurements that removed doubts and are not worth re-running: a nested
workspace's `target-dir = "target"` resolves to `<nested>/target` and stops the
inherited redirect (`cargo config get -Z unstable-options --show-origin`, and
the build landed there); and `build.rustflags` is replaced by the nearest
config rather than merged. `cargo +toyos build --workspace` finishes exit 0 in
53.03 s, so the host workspace under the fork toolchain is a policy question and
not a compile blocker.
