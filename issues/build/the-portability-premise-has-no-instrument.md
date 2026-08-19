---
status: open
kind: defect
opened: 2026-08-19
---

# "Rust + QEMU is everything you need" is the premise, and no instrument holds it on any OS but this one

The dependency rule says only Rust and QEMU. If that is true, then on a fresh
machine of any major OS, installing those two and running the build is the whole
setup. Nothing checks this. What actually holds today, per OS:

- **Linux** — the strongest coverage, incidentally: CI's ubuntu runners build
  the toolchain and boot the twelve KVM guest shards on every PR. But CI
  runners carry a large preinstalled image, so "worked on CI" is weaker than
  "Rust + QEMU sufficed" — nothing proves the build did not quietly lean on a
  tool the runner happened to have.
- **macOS** — proven daily by the dev host, which is one machine with an
  accumulated environment; never by a fresh runner. The declared standing
  failure "four macOS FAT tools" cuts the other way here: those exist on any
  macOS, but their use means the *Linux* image build path differs from the one
  exercised daily, or fails.
- **Windows** — the build system does not compile there at all.
  `src/toolchain.rs:1788` calls `std::os::unix::fs::symlink` unguarded
  (`issues/…/the-build-system-does-not-compile-on-windows.md`). Nothing further
  is known because nothing has ever gotten past that line.

## The instrument: a nightly matrix job

GitHub Actions offers all three: `ubuntu-latest`, `windows-latest`, and
`macos-latest` (arm64). One nightly workflow, three jobs, each doing exactly
what the premise names and nothing else:

1. install Rust (rustup) and QEMU — the one per-OS step, and *what that step
   is* on each OS is part of what the job documents
2. `cargo run -- --build-only`
3. `cargo test` — where virtualization allows; a TCG run where it does not

Nightly, not the PR gate: the toolchain bootstrap is expensive, macOS runner
minutes are billed at a multiplier, and a portability regression does not need
to block a merge the same evening it lands — it needs to be *seen*.

## The jobs red on day one, and that is the point

- Windows reds at compile time on the symlink. Expected — the job turns the
  known hole into a standing measurement instead of a filed claim, and its
  first green is the fix's proof.
- Linux may red in the image-build step if the FAT tooling path really is
  macOS-shaped; CI's green does not settle this because CI's image jobs may
  take a different path than a bare runner. Whatever reds is exactly the gap
  between the premise and the tree.
- macOS on a fresh runner may red on something the dev host stopped noticing
  years of setup ago. Same value.

The redlist's own rule applies: a red the instrument expects is declared
(`EXPECTED_FAILURES` / the redlist, whichever owns workflow-level reds), not
absorbed — so the nightly can be green-with-declared-reds from its first run
while each OS's gap has a name.

## What this is not

Not a port. Nothing here fixes Windows or changes the build; the fix lives with
the symlink issue. This is only the instrument that makes the premise *behave
like a premise* — checked every night on machines nobody has groomed — instead
of a sentence in a doc. When the self-hosting north star eventually adds a
fourth row (ToyOS itself), this is the table it joins.
