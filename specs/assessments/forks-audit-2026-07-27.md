# ToyOS Fork Estate Audit — Final Report (2026-07-27)

> **PARTIALLY SUPERSEDED — this is a dated record, not current state.** The body
> below is preserved verbatim as an accurate account of 2026-07-27. Do not act on
> a finding without checking it first: `forks.toml` is the live manifest and
> `specs/issues/` carries current state.
>
> Resolved since, each verified against the code:
>
> | Finding | Status |
> |---|---|
> | Build system swallows all cargo/rustc stderr on success (§1, §5 of Priorities) | Fixed, `f8f80c4`. But the deeper half survives and is *not* in this report: cargo gives every non-path source `--cap-lints allow`, so the forks stay invisible regardless — `specs/plans/fork-lint-audit-plan.md` |
> | Dead `target-lexicon` patch entry in userland/Cargo.toml (§MINOR) | Deleted, `9a3e6c6`, along with three more unused entries (ctrlc, memmap2, stacker) |
> | `bootstrap-cc` needlessly patches tar/filetime (§MINOR); bootstrap-cc rows in §2 | `bootstrap-cc` deleted entirely, `55e8ad8` — it targeted the host and did not work |
> | "~a third of the estate (cargo + 7 satellites, ~110MB) is dormant"; the 25-fork inventory in §2 | Superseded. Nothing is vendored now; `userland/cargo`, `rustix`, `gitoxide`, `tar`, `filetime` and the rest are absent from the tree, and `forks.toml` lists 14 fork repositories consumed as git branches |
> | "zero upstream PRs exist" (§1) | Superseded — two are open: raw-window-handle #223 and target-lexicon #134 |
> | memmap2's `ErrorKind::Unsupported` contract violation (§3 hijack (b)) | Recorded fixed 2026-07-28 in `forks.toml`, along with three real bugs the evaluation found |
>
> Not re-verified here, so treat as still open until someone checks: the socket2
> and tar cfg-gate hijacks, the split-brain `toyos-abi` snapshot, and estate drift.

## 1. Verdict

The fork strategy is fundamentally sound and mostly well-executed: ~17 of 25 userland forks are textbook additive platform ports (new `toyos.rs` backends + cfg-gated dispatch, winit's sibling-crate `winit-toyos` being the model case), all upstream LICENSE files are intact, lockfiles are healthy (`--locked` passes everywhere), and spot-checked forks compile warning-free for the toyos target. The libc-free std (toyos-abi direct) removes the classic porting bottleneck. However, the strategy's stated goal — upstream-mergeable, first-class platform status — is currently aspirational: **zero upstream PRs exist**, and every PR is hard-blocked by the unpublished, unlicensed `toyos-abi`/`toyos` crates that all forks path/git-depend on. Three verified cfg-gate hijacks (socket2, memmap2, tar) violate the project's own rules, a stale GitHub-pinned `toyos-abi` snapshot creates a split-brain ABI inside shipping binaries, ~a third of the estate (cargo + 7 satellites, ~110MB) is dormant behind a commented-out system.toml entry, and the build system silently swallows all rustc warnings on success — so the fork-hygiene bar is not being enforced by tooling. The estate was synced once (2026-04-09) and is drifting; "kept up to date with upstream" is not yet a process.

## 2. Inventory

| Fork | Vendored | True delta | Used by | Drift (releases) | Status |
|---|---|---|---|---|---|
| rust/ (submodule) | 1.96.0 base 2026-04-09 | 78 files +3390/−96 | toolchain (host + hosted rustc) | ~3.5 mo (1.99.0) | ACTIVE |
| library/backtrace (Japabu/backtrace-rs) | 28ec93b +2 commits | +192 | std backtraces | undocumented | ACTIVE |
| mio | 1.2.0 | +774/−30 | tokio→sshd | 2 | ACTIVE |
| tokio (+util/macros) | 1.51.1 | +119/−41 | sshd | 10 | ACTIVE |
| socket2 | 0.6.3 | +1303/−11 | tokio→sshd | 2 | ACTIVE (gate hijack) |
| russh (+cryptovec) | 0.60.0 | +703/−231 | sshd | 11 | ACTIVE (security-relevant) |
| getrandom-0.2/0.3/0.4 | 0.2.17/0.3.4/0.4.2 | ~+52 total | rand/ring (doom, snake, sshd) | 0/0/1 | ACTIVE (stale ABI pin) |
| cpal | 0.18.0 | +327 (pure add) | doom, toybox | 1 | ACTIVE |
| winit | 0.31.0-beta.2 | +1234 (new winit-toyos crate) | doom, snake | tracks beta | ACTIVE |
| softbuffer | 0.4.8 | +155/−5 | doom, snake | 0 | ACTIVE |
| raw-window-handle | 0.6.2 | ~+69 (pure add) | winit/softbuffer | 0 | ACTIVE |
| target-lexicon | 0.13.5 | +9 | toyos-cc, cg_clif | 0 | ACTIVE (dead userland patch entry) |
| libloading | 0.8.9 (code 0.9.0) | +345 | hosted rustc, tests | 1 | toolchain |
| memmap2 | "0.2.1" (code 0.9.x) | +41/−62 | hosted rustc | relabeled | toolchain (stub hijack) |
| stacker | 0.1.23 | +90 | hosted rustc | 1 | toolchain |
| ctrlc | 3.5.1 | +45/−57 | hosted rustc | 1 | toolchain |
| cargo | 0.97.0 | 43 files +5285/−2173 | system.toml entry COMMENTED OUT | 3 | DORMANT |
| gitoxide (65 crates, 80M) | gix 0.81.0 | +54/−13 (5 files, 4 crates) | cargo fork only | 4–5 | DORMANT |
| rust-url | 2.5.8 | +43/−21 | cargo fork only | 0 | DORMANT |
| errno | 0.3.14 | +54 | cargo fork only | 0 | DORMANT |
| is_executable | 1.0.5 | +2/−1 | cargo fork only | 1 | DORMANT |
| jobserver | 0.1.34 | +162 | cargo fork only | 1 | DORMANT |
| tar | 0.4.45 | +42/−12 | cargo + bootstrap-cc (host) | 1 | DORMANT (wasm hijack) |
| filetime | 0.2.27 | +48 | cargo + bootstrap-cc (host) | 2 | DORMANT |
| rustix | 1.1.4 | +4 (1 file) | NOTHING (workspace-exclude only) | 0 | ORPHANED |

(userland/libc and bootstrap-cc are original ToyOS code, not forks.)

## 3. Findings (ranked; cross-checked in code where auditors disagreed)

**CRITICAL**
1. **Upstreaming is 100% aspirational and hard-blocked.** GitHub API: 1 PR total by Japabu (own repo). Every fork depends on `toyos-abi`/`toyos` via relative path (`userland/mio/Cargo.toml:70`) or git URL; rust std path-deps escape the submodule (`library/std/Cargo.toml` → `../../../toyos-abi`). The repo has no LICENSE file, the crates have no license field, and crates.io names `toyos`/`toyos-abi` are free. Nothing can merge upstream until this is fixed. The `x86_64-unknown-toyos` target existing only in the fork gates all crate-level CI.

**MAJOR**
2. **Split-brain ABI via stale git pin.** All three getrandom forks use `toyos-abi = { git = "https://github.com/Japabu/toyos-abi" }` (verified: getrandom-0.2/Cargo.toml:21, -0.3:31, -0.4:45), pinning a 2026-03-11 snapshot (2fe0c57) that diverges ~499 lines from in-tree. userland/Cargo.lock links BOTH toyos-abi copies into the same binaries; it works only because `SYS_RANDOM=6` hasn't moved — against an ABI documented "completely unstable". Same dual-provenance in rust/Cargo.toml:98-99 (Japabu/getrandom git branches vs vendored subtrees, byte-identical today, nothing enforces sync). Public GitHub forks are ~3.5 months stale vs the subtrees (cpal changed in-repo 2026-07-24, never pushed back).
3. **Three verified cfg-gate hijacks (policy violations).** (a) socket2/src/socket.rs:23,635: upstream's `all(unix, not(redox))` on `MsgHdrMut`/`recvmsg` rewritten to `not(any(redox, wasi))` — now includes Windows, whose `sys` has no `recvmsg` (verified: zero hits in sys/windows.rs; the fn's own doc says unsupported on Windows). (b) memmap2 src/stub.rs: the shared `not(any(unix, windows))` fallback rewritten un-gated from error-returning stubs to a fake read-into-Vec mmap whose `map_mut`/`flush` silently lie — for ALL unknown platforms; crate also relabeled 0.9.x→"0.2.1" to satisfy rustc's pin. (c) tar src/entry.rs:848: `_set_perms` under shared `any(wasm32, toyos)` gate returns `Ok(())` (verified) where upstream wasm returned Err — silent wasm32 behavior change. One auditor called socket2 "upstream-ready"; the structure is, but not until the gate is fixed.
4. **~A third of the estate is dormant.** cargo (+5285/−2173 architectural fork: gix-vs-git2, sqlite-vs-flatfile, curl-vs-reqwest) plus 7 exclusive satellites exist for a system.toml entry that is commented out (line 31); gitoxide vendors 65 crates/80MB for a 5-file delta; userland/rustix is fully orphaned (verified: appears only in the workspace exclude list; lockfiles resolve registry rustix); userland/cargo/target holds 3.7GB stale cache; gitoxide carries a 230KB binary `test_symlink2`.
5. **Build system swallows all cargo/rustc stderr on success — in both modes.** Verified src/build.rs `cargo_build()`: `cmd.output()` captures stderr unconditionally (`Command::output` pipes by default; the `quiet` branch's explicit pipe is redundant), and stderr is printed only on failure. Empirically a full `--build-only` shows zero compiler lines — not even cargo's own unused-patch warning. The zero-warning bar is unenforceable; CLAUDE.md misattributes this to quiet mode.
6. **rust/ fork health.** One sync ever (2026-04-09; 1.96.0 vs upstream 1.99.0), no upstream remote, no tooling. Warts that would fail upstream review: rustc_log `cfg!(toyos)` early-return masking a real, untracked dlopen/vtable relocation bug (violates fail-fast); tidy extdeps blanket-allows `github.com/Japabu/`; no-op drift in rustc_symbol_mangling/v0.rs; wasi getrandom `=0.3.3` un-pinned; a genuine cross-platform fix (memmap map_anon `with_capacity`→`vec![0u8; len]`) smuggled in instead of PR'd. The backtrace-rs fork is undocumented in CLAUDE.md.
7. **russh is 11 releases behind on a network-facing crate** (0.60.0 vs 0.62.4; sshd links it). Correction to one audit: the rustcrypto backend IS a proper additive optional feature (verified `feature = "rustcrypto"` gates + Cargo.toml:19) and independently upstreamable — but its +703/−231 lives inside existing cipher files, so every deferred merge gets more expensive.

**MINOR** — dead `target-lexicon` patch entry in userland/Cargo.toml (verified `[[patch.unused]]` lock:4510; warns on every cargo run); active-fork drift moderate (tokio 10, mio/socket2 2 behind); un-gated crate-wide lint allows in memmap2/libloading (mio/socket2 use `cfg_attr(target_os="toyos", ...)` correctly); upstream READMEs replaced in ctrlc/stacker/memmap2/target-lexicon; bootstrap-cc (host-only) needlessly patches tar/filetime; toolchain non-hermetic (linked "toyos" toolchain ships no cargo → host nightly fallback); guest toyos-cc ships cranelift at opt-0 (33MB initrd binary); CLAUDE.md's "only edit toyos-named std files" rule is unsatisfiable as written (actual dispatcher edits follow tier-3 bringup convention and are clean).

## 4. Recommendations (value/effort ranked)

1. **Hours, high value — mechanical fixes:** point all three getrandom `toyos-abi` deps at `path = "../../toyos-abi"` (kills the split-brain ABI time bomb); delete the target-lexicon patch line from userland/Cargo.toml; delete userland/rustix; delete gitoxide's `test_symlink2` and `rm -rf userland/cargo/target` (3.7GB); drop bootstrap-cc's tar/filetime patches; point rust/Cargo.toml getrandom patches at `../userland/getrandom-*` (zero-diff today).
2. **Hours — fix the three cfg hijacks:** socket2 → `any(all(unix, not(redox)), target_os = "toyos")`; tar → separate toyos-only `_set_perms` (restore wasm's Err); memmap2 → toyos-gated module + restore pristine stub.rs, and fix the version lie by bumping rustc_data_structures' memmap2 req in rust/ (itself upstreamable). Gate the memmap2/libloading lint allows behind `cfg_attr(target_os="toyos", ...)`.
3. **Hours — make warnings visible:** in src/build.rs non-quiet mode, inherit or forward captured stderr on success; correct the CLAUDE.md known-issue entry.
4. **Days, unblocks everything — license + publish:** add MIT OR Apache-2.0 at repo root + license fields; publish `toyos-abi` (and `toyos`) 0.x to crates.io (hermit-abi/vex-sdk model; rustc-dep-of-std features already exist). Decide the "completely unstable ABI" tension via frequent 0.x bumps.
5. **Start the upstream pipeline smallest-first:** rustix (as a 2-line PR, not a fork), is_executable, target-lexicon, getrandom, then errno/rwh/stacker/jobserver/libloading; russh `rustcrypto` feature PR any time (platform-independent); winit-toyos before 0.31 stabilizes; then the rust-lang/rust tier-3 target + std PR (clean the rustc_log workaround, extdeps carve-out, v0.rs noise first — and record the vtable-relocation bug in `specs/issues/` with a reproducer).
6. **Institute sync cadence:** quarterly "Merge upstream *" sweep (russh first, tokio second); add an upstream remote + documented merge process to rust/; add a `git subtree push` step so GitHub forks track vendored state; document the backtrace-rs fork in CLAUDE.md.
7. **Decide cargo's fate explicitly:** if cargo-on-ToyOS is near-term, re-enable it in a CI build gate so the 8-fork constellation stays green; if not, delete cargo + exclusive satellites (subtree re-import is one commit) or mark them frozen in CLAUDE.md. Either way slim gitoxide to the 4 modified crates or drop it with cargo.
8. **Docs honesty:** reword the std-rules section to "new logic only in toyos-named files; cross-platform files may gain additive cfg arms in existing tier-3 style"; note that rust/ builds only as a toyos submodule; document the memmap2/libloading version-relabel trick.