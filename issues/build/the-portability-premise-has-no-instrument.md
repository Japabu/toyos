---
status: open
kind: defect
opened: 2026-08-19
---

# "Rust + QEMU is everything you need" is the premise, and no instrument holds it on any OS but this one

The dependency rule says only Rust and QEMU. If that is true, then on a fresh
machine of any major OS, installing those two and running the build is the whole
setup. `.github/workflows/portability.yml` is now the instrument that checks
this nightly (`workflow_dispatch` too) — three jobs, Linux, macOS and Windows.
What each proves today:

- **Linux** (`linux` job) — moved off `ubuntu-latest` onto the same minimal
  `debian:sid` container `ci.yml`'s guest shards run in, closing the gap this
  file originally named: git, curl, ca-certificates, `build-essential`,
  python3 (`rust/x`'s own bootstrap dependency for a clean clone — never
  declared before because `ubuntu-latest` ships one already) and the declared
  QEMU (`.github/qemu-version`) apt-installed, `rustup` installed from
  `sh.rustup.rs`, then `cargo run -- --build-only` with no cache. A green run
  here now means the declared package list was the whole of what this needed,
  not that the hosted image's own tens-of-GB contents quietly covered the
  rest — the same move `ci.yml`'s guest shards already made, for the same
  reason. **Not yet re-measured**: the previous `ubuntu-latest` version's cost
  (four from-scratch runs inside `toolchain.yml`'s own `build` step,
  2026-08-15/19, 3864-4048 s / ~65 min for the command alone) is not a number
  about this container — a from-scratch `apt-get install` inside a freshly
  pulled `debian:sid` is a different cost than a pre-provisioned VM image, and
  the workflow's own `timeout-minutes: 350` is carried over rather than
  re-derived. The first real number is whatever the first nightly run reports.
  Still open, as before: whether the bootstrap genuinely needs cmake or ninja
  (neither is installed here) has never been tested against a machine that
  lacks them.
- **macOS** (`macos` job) — added. `host-tests.yml` already proved
  `macos-latest` reachable on this plan; this job points a `--build-only` at a
  fresh one for the first time. QEMU comes from Homebrew, and the workflow
  tries to pin it to `.github/qemu-version` first. **Finding**: the pin does
  not exist as a general mechanism — measured empirically against a macOS
  Homebrew install while writing this job (2026-08-20), `brew install
  qemu@11.1.0` answers "No available formula with the name \"qemu@11.1.0\"."
  (exit 1). Homebrew-core keeps exactly one version of the `qemu` formula —
  whichever it currently calls stable — with no versioned formula the way
  `python@3.11` or `openssl@3` have one, so a pin is only possible on the day
  the declared version happens to equal current stable. That same measurement
  found today's unpinned `qemu` *is* 11.1.0, so the job's first runs will
  likely show the declared and installed versions agreeing, but by
  construction and not by anything the workflow enforces — a future
  disagreement is expected eventually and is itself the finding this job
  exists to surface, not a reason to fail it (`src/main.rs`'s
  `check_prerequisites` only notes a QEMU version other than declared outside
  a guest-booting gate, and this job boots no guest). The "four macOS FAT
  tools" standing exception does not qualify this job's honesty the way this
  file previously worried: `--build-only` runs no test, and
  `src/image.rs`'s `format_fat32` builds both volumes this build writes with
  the pure-Rust `fatfs` crate — `newfs_msdos`/`hdiutil` are exercised only by
  `toyos-fat32`/`toyos-fat32-check`'s host test fixtures, in `host-tests.yml`,
  never in a `--build-only` path. Cost is entirely unmeasured — this job has
  never run — so its `timeout-minutes: 350` borrows Linux's ceiling rather
  than a real number.
- **Windows** (`windows` job) — unchanged. `cargo build -p toyos-build` only,
  `cargo run` being unreachable: `src/toolchain.rs`'s `link_host_target` calls
  `std::os::unix::fs::symlink` with no `#[cfg(unix)]` guard
  (`issues/build/the-build-system-does-not-compile-on-windows.md`), so the
  crate does not compile on this OS at all. This job is that issue's own
  standing measurement — it reds every night until the symlink issue closes,
  which is the declared frontier and the point, not a surprise. Its first
  green is that issue's proof of fix, and only then does `cargo run
  -- --build-only` belong in this job, matching Linux.

`nightly-red-portability` now reacts to either `linux` or `macos`, the same
"declared, not absorbed" reasoning as before: `windows` stays hand-excluded,
since its red is the tracked frontier this file already names and reporting it
nightly would be noise the redlist doctrine calls absorbed rather than
declared.

## What is still open

- The third step this file originally sketched — `cargo test` where
  virtualization allows, a TCG run where it does not — is still deliberately
  out of the first cut. `portability.yml`'s Linux and macOS jobs stop at
  `--build-only`; wiring in a guest boot means either job also becomes one
  `src/ci.rs`'s `every_gate_that_boots_a_guest_names_its_instrument` should
  reach (add the file to that test's `GATES` list and run
  `.github/instrument.sh`), which the current build-only jobs correctly do
  not do — neither calls `qemu::launch`.
- If a job here is ever expected to red on a real (not-yet-fixed) gap that
  is not already carried by its own filed issue the way Windows's is, the
  redlist doctrine's "declared, not absorbed" still needs a worked answer for
  a *workflow-level* red — today `nightly-red-portability` reacts to a
  `linux` or `macos` red and Windows is hand-excluded by a workflow comment,
  which does not generalise past one known case.
- The Linux container move and the macOS job are both unmeasured against a
  real nightly run as of this writing — the numbers above are constructed
  from the workflow's own logic and a local Homebrew check, not from a
  completed GitHub Actions run. The first scheduled run (or a
  `workflow_dispatch`) is what turns "should prove" into "proved"; if either
  job reds for a reason its own comment does not already predict (Linux
  missing a package the bootstrap turns out to need beyond git/curl/
  ca-certificates/build-essential/python3/qemu-system-x86, or macOS lacking
  disk, cores, or something else Xcode's command-line tools do not provide),
  that is new information for this file, not a workflow bug to silently work
  around.

## What this is not

Not a port. Nothing here fixes Windows or changes the build; the fix lives with
the symlink issue. The instrument makes the premise *behave like a premise* —
checked every night on machines nobody has groomed — instead of a sentence in a
doc. When the self-hosting north star eventually adds a fourth row (ToyOS
itself), this is the table it joins.
