---
status: open
kind: defect
opened: 2026-08-19
---

# "Rust + QEMU is everything you need" is the premise, and no instrument holds it on any OS but this one

The dependency rule says only Rust and QEMU. If that is true, then on a fresh
machine of any major OS, installing those two and running the build is the whole
setup. `.github/workflows/portability.yml` is now the instrument that checks
this nightly (`workflow_dispatch` too) — two jobs, Linux and Windows. What each
proves today:

- **Linux** (`linux` job) — a stock `ubuntu-latest` runner, `build-essential`
  and the declared QEMU (`.github/qemu-version`) apt-installed, `rustup`
  installed from `sh.rustup.rs`, then `cargo run -- --build-only` with no
  cache. Measured cost (four from-scratch runs of the identical command inside
  `toolchain.yml`'s own `build` step, 2026-08-15/19): 3864-4048 s
  (~65 min) for the command itself, ~69-71 min total job wall time in three of
  the four — comfortably inside a nightly's budget, so this job runs the real
  thing rather than a reduced form. What it does **not** yet prove: GitHub's
  hosted image is itself heavily provisioned (tens of GB, several SDKs, cmake
  and ninja already on `PATH`), so a green run here still cannot distinguish
  "the three packages this job installs were enough" from "the image's own
  contents quietly covered the rest" — the same gap this file originally named
  for CI's ubuntu runners. Closing it means the same move `ci.yml`'s guest
  shards already made: a minimal container image instead of the hosted
  runner's own filesystem.
- **Windows** (`windows` job) — `cargo build -p toyos-build` only,
  `cargo run` being unreachable: `src/toolchain.rs`'s `link_host_target` calls
  `std::os::unix::fs::symlink` with no `#[cfg(unix)]` guard
  (`issues/build/the-build-system-does-not-compile-on-windows.md`), so the
  crate does not compile on this OS at all. This job is that issue's own
  standing measurement — it reds every night until the symlink issue closes,
  which is the declared frontier and the point, not a surprise. Its first
  green is that issue's proof of fix, and only then does `cargo run
  -- --build-only` belong in this job, matching Linux.
- **macOS** — still not instrumented. `host-tests.yml` already runs
  `macos-latest` today, so the free tier clearly reaches it: the gap is that
  nobody has pointed a `--build-only` at a fresh one, not that the tier is
  unavailable. Left out of the first cut on purpose (see the PR that added
  `portability.yml` for why); still proven only by the accumulated dev host,
  and the "four macOS FAT tools" standing failure still cuts the way this file
  originally said — those tools exist on any macOS, but leaning on them means
  the *Linux* image-build path differs from what is exercised daily, or fails
  there.

## What is still open

- A macOS job, the natural third row — `host-tests.yml` is the proof it is
  reachable on this tier.
- The Linux job's own gap above: a minimal container (`ci.yml`'s guest shards'
  own move) in place of `ubuntu-latest`'s heavily provisioned image, so a green
  run means what it currently only gestures at.
- The third step this file originally sketched — `cargo test` where
  virtualization allows, a TCG run where it does not — was deliberately left
  out of the first cut. `portability.yml`'s Linux job stops at
  `--build-only`; wiring in a guest boot means the job also becomes one
  `src/ci.rs`'s `every_gate_that_boots_a_guest_names_its_instrument` should
  reach (add the file to that test's `GATES` list and run
  `.github/instrument.sh`), which the current build-only job correctly does
  not do — it never calls `qemu::launch`.
- If a job here is ever expected to red on a real (not-yet-fixed) gap that
  is not already carried by its own filed issue the way Windows's is, the
  redlist doctrine's "declared, not absorbed" still needs a worked answer for
  a *workflow-level* red — today `nightly-red-portability` only reacts to a
  `linux` red and Windows is hand-excluded by a workflow comment, which does
  not generalise past one known case.

## What this is not

Not a port. Nothing here fixes Windows or changes the build; the fix lives with
the symlink issue. The instrument makes the premise *behave like a premise* —
checked every night on machines nobody has groomed — instead of a sentence in a
doc. When the self-hosting north star eventually adds a fourth row (ToyOS
itself), this is the table it joins.
