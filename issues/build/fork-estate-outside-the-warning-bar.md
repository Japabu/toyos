---
status: open
kind: defect
opened: 2026-08-01
---

# The fork estate is invisible to the zero-warning bar

Cargo passes `--cap-lints allow` to every package whose source is not a *path*
source. All 14 forks in `forks.toml` are consumed as git dependencies, so rustc
discards their warnings before anything can print them. Measured on `sshd`'s
graph: 140 of 143 units capped, the three exceptions being the local path crates
`sshd`, `toyos` and `toyos_abi`.

This is not a build-system defect and no build-system change can reach it. The
build system used to swallow cargo's diagnostics on success as well — that is
fixed — and the forks stayed invisible, because the cap is applied by cargo
upstream of anything `src/build.rs` does.

The trap to avoid is `[lints]` inside a fork: it is a manifest change, so it
lands in `git log <base>..toyos` and would put ToyOS lint policy into every
upstream PR the estate sends. The same objection rules out a cargo config inside
a fork checkout, and `RUSTFLAGS`, which would apply `-Dwarnings` to the whole
graph including untouched upstream crates.

**Turning each fork into a path source is the only thing that lifts the cap**,
so running the audit means cloning all 14 beside the monorepo and adding
overrides — which changes what every build in the repo resolves, so it needs a
quiet tree. Cover every patch site, not just userland. Then one full build with
the complete output captured to a file: expect real volume, because tokio,
winit, russh and cpal are large and their ToyOS deltas have never been linted
once. Triage against the fork rule — a warning in upstream's own code is not
ours and must not be "fixed", because that inflates the delta; only warnings
inside ToyOS-added modules and `target_os = "toyos"` arms are actionable.
Restore the overrides afterwards; they are dev-only and must never be committed.
(`cargo build -vv` also defeats the cap, but only by dumping every rustc command
line, which at this scale is unusable.)

**The standing mechanism is an open question, and "fix it later in the build
system" is not one of the answers** — the build system cannot see these warnings
at all. The honest options are a periodic path-override audit run deliberately,
or accepting that fork hygiene is a review-time concern checked when a fork is
updated. Pick one.

This is one instance of a wider shape: the estate is outside every check the
tree runs on itself. It is also invisible to ABI signature changes until a build
breaks, and it can hold frozen copies of first-party crates
(`issues/build/a-fork-depended-on-toyos-abi-by-git.md`). Each instance was found
by accident or by a build breaking; none by a check. **"I enumerated the call
sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`** —
grepping the monorepo is a partial enumeration that reads as complete, and it
cost every agent a blocked workspace once already.
