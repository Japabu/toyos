# Fork lint audit — plan

## Why fork problems keep arriving as surprises

**The fork estate is systematically outside every check the tree runs on itself.**
Four instances, all confirmed:

1. **Invisible to the zero-warning bar** — cargo gives every non-path source
   `--cap-lints allow`, so rustc discards fork warnings before anything prints
   them (this note's original subject).
2. **Invisible to ABI signature changes** until a build breaks. A first-party
   signature can change and no check notices that a pinned fork calls the old
   one; `map_shared` stranded the `mio` fork and blocked every agent's workspace
   until someone tried to build.
3. **Holding frozen copies of first-party crates.** A fork depending on
   `toyos-abi` by *git* rather than by version resolves — silently, against a
   snapshot — and `[patch]` does not redirect it (`specs/issues/`).
4. **Un-fixable except in a quiet-tree window**, because editing a fork means
   `.cargo/config.toml` path overrides, which change the build for every agent at
   once. So fork fixes queue behind a scheduling constraint no other work has.

Each was found by accident, by a build breaking, or by someone going and looking.
None was found by a check. That is the sentence to keep: **the estate is not
covered by the project's own instruments, so every fork problem surfaces as a
surprise rather than as a red test.**

### The enumeration lesson

**"I enumerated the call sites" is only true if the enumeration covered
`~/.cargo/git/checkouts/`.** Nothing in the tree tells you otherwise until a
build breaks. Grepping the monorepo for callers of a first-party function is a
*partial* enumeration, and it reads as complete — every caller you can see is a
caller you found.

This cost every agent a blocked workspace today, on an enumeration that was
careful by every standard applied to the tree itself. Before changing any
signature in `toyos-abi` or `toyos`, grep the checkouts too.

---

The zero-warning bar does not reach the fork estate, and no build-system change
can make it. This is the hand-off note for the agent that runs the audit.

## The finding

Cargo passes `--cap-lints allow` to every package whose source is not a **path**
source. All 14 forks in `forks.toml` are consumed as git dependencies, so rustc
discards their warnings before anything can print them.

Measured 2026-08-01 on `sshd`'s graph: **140 of 143 units capped**. The only
three uncapped were `sshd`, `toyos` and `toyos_abi` — the local path crates.

This is independent of the build system. `src/build.rs` used to swallow cargo's
diagnostics on success (fixed, f8f80c4), and the July fork audit blamed that for
the unenforced fork-hygiene bar (`specs/assessments/forks-audit-2026-07-27.md:5`). Only half
of that was right: the suppression is gone, and the forks are still invisible.

## `[lints]` is the wrong tool here — do not reach for it

`[lints.rust] warnings = "deny"` in a `Cargo.toml` is what extended the bar to
`toyos-ld`, `toyos-cc` and `tests/toyos-rust-tests` (b90fe9a). **It must not be
used inside a fork.** A `[lints]` table is a manifest change, so it becomes part
of `git log <base>..toyos` — the delta that is supposed to be exactly what an
upstream PR contains, and nothing else. Adding it would put a ToyOS-local lint
policy into every upstream PR the estate ever sends.

The same objection rules out `.cargo/config.toml` inside a fork checkout, and
`RUSTFLAGS`, which would apply `-Dwarnings` to the entire dependency graph
including untouched upstream crates.

## What running it takes

0. **First, `grep` all fifteen checkouts for git-sourced ToyOS dependencies** —
   `grep -l 'toyos-abi\|toyos =' ~/.cargo/git/checkouts/*/*/Cargo.toml` and check
   each for the `{ git = ... }` form. Cheapest step, and it establishes whether
   instance 3 above is live before anything else is touched.
1. **Clone all 14 fork repos beside the monorepo** and add path overrides in
   `.cargo/config.toml` (gitignored; `.cargo/config.toml.example` documents the
   mechanism). Turning each into a path source is the *only* thing that lifts
   the cap. Cover every patch site, not just userland — forks are patched in
   `userland/Cargo.toml`, `rust/Cargo.toml`, `toyos-cc/Cargo.toml`,
   `tests/toyos-rust-tests/tls-cranelift/Cargo.toml` and the root `Cargo.toml`.
2. **One full build, complete output captured to a file.** Expect real volume:
   tokio, winit, russh and cpal are large, and their ToyOS deltas have never
   been linted once.
3. **Triage against the fork rule.** A warning in upstream's own code is not
   ours and must not be "fixed" — that inflates the delta and breaks the
   promise about `base..toyos`. Only warnings inside ToyOS-added `toyos.rs`
   modules and cfg-gated `target_os = "toyos"` arms are actionable.
4. **Restore `.cargo/config.toml`** when done. The overrides are dev-only and
   must never be committed.

`cargo build -vv` also defeats the cap, but only by dumping every rustc command
line; at this scale the output is unusable. Path overrides are the practical
route.

## When

**Only when the tree is quiet.** Path-overriding the forks changes what every
build in the repo resolves, so it cannot run while other agents are building.

## Standing mechanism — open question

There is no clean way to make this a build-time gate, because the only
mechanisms that work are ones a fork must not carry. The honest options:

- a periodic path-override audit, run deliberately, as above; or
- accept that fork hygiene is a review-time concern and check it when a fork is
  updated, rather than on every build.

Pick one. "Fix it later in the build system" is not on the list — the build
system cannot see these warnings at all.
