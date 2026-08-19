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
