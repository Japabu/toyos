# Daemon Testability — Spec

> 2026-07-28. Four daemon surveys, three competing structures, one synthesis.
> The four system daemons have **zero tests** between them. Both soundd bugs found
> on 2026-07-28 were in pure code needing no device, no syscall and no kernel — and
> both cost a 120-boot QEMU campaign to find. That is the case for this work, and
> it is empirical rather than aesthetic.

## 1. Recommendation

Take the **structure** from `traitshell` and the **test mechanics** from `corecrate`, and reject `sharedlib`'s four-workspace estate while adopting its one free step. Concretely: do not create `soundd-core/`. `toyos-sched/` is split because it has two consumers on two targets — the kernel (`no_std`, `x86_64-unknown-none`, consumed by path from `kernel/Cargo.toml`) and the host sim — and because loom needs a separate package to stay out of the kernel lockfile (`toyos-sched/Cargo.toml:26-31` says so). soundd has one consumer, one target, one `std`. Under CLAUDE.md's >2x rule the split buys nothing the seam does not already buy; it only makes purity *checkable* rather than *achievable*, and it costs a lockfile, a re-export layer, ~12 `pub` markers on items that are correctly private, and `rubato` resolved twice. Test the daemon where it lives: `toyos-abi`, `toyos` and the daemons are host-compilable with the toolchain already installed — `~/.rustup/toolchains/toyos/lib/rustlib/aarch64-apple-darwin/lib/` contains `libstd-aa615a6a479298fa.rlib` and `libtest-105ba49d6e82e80b.rlib` (hashes distinct from stable's `libstd-5c291ca0698fa83b`, so genuinely built for this toolchain), `~/.rustup/toolchains/toyos/bin/` holds only `rustc`+`rustdoc` so cargo falls back, and `src/build.rs:183-186` already drives exactly that path. From `corecrate`, take the virtual-clock fake device, the deterministic stall as a scenario opcode, the sweep-instead-of-a-point discipline, and the feature-gated port of the old buggy algorithm as a required-to-fail gate. From `sharedlib`, take exactly one thing: `toyos-abi` is a standalone package at the repo root, in no workspace (`userland/Cargo.toml:3-24` does not list it; the root `Cargo.toml` is the `toyos-build` package with no `[workspace]`), so it can take `#[cfg(test)]` today with a one-line attribute — and it has a real bug in it.

## 2. The seam

**Narrower than all three designs proposed.** `corecrate` wants a 9-method `Dev`; `traitshell` wants 10. Both are wrong for the same reason: `now`, the completion records and the client commands are *values*, not capabilities. The only capability soundd genuinely needs abstracted is where a mixed period goes.

```rust
/// Where a mixed period goes. Production: the mapped DMA buffer + SYS_AUDIO_SUBMIT.
/// Test: a Vec<i32> play timeline driven by a virtual play pointer.
trait PeriodSink {
    fn buffer(&mut self, idx: u32) -> &mut [i16];
    fn submit(&mut self, idx: u32);
}

fn mix_pass(
    st: &mut MixState,
    now: u64,
    records: &[AudioCompletionRecord],
    cmds: &CommandRing,
    sink: &mut impl PeriodSink,
) -> PassOutcome;   // { next_wake_ns, removed: Vec<Fd>, stats_flush: Option<MixStats> }
```

Two methods and four values, against `toyos-sched/src/hw.rs`'s ten for the whole SMP scheduler.

Everything from `userland/soundd/src/main.rs:645` (`loop {`) to `:740` moves into `mix_pass` **except** these, which stay in a ~60-line shell that decides nothing: `:556-558` client signal writes, `:589-596` `poll_add`/`wait`, `:601-603` cmd-pipe drain, `:631-635` `read_completions`, `:695-697` the raw DMA slice (becomes `sink.buffer(idx)`), `:703` `audio_submit` (becomes `sink.submit(idx)`), `:716-717` `close`, `:731`/`:736` `stats.report`.

What that puts under test — verified by reading, not assumed:
- the free_mask/DLL fold at `:641-654`, including `assert!(n > 0)` (`:645`) and `assert_eq!(free_mask & rec.mask, 0)` (`:647`), the kernel's disjointness contract from `kernel/src/audio.rs:22-27`;
- the §5.9 drain branch at `:670-678` — the entire subject of commit `91a653c`;
- the `while free_mask != 0` refill loop at `:680-711`, i.e. `mix_client` + `dither_and_quantize` + submit ordering;
- the `pending_removal` retain at `:715-725`, which is where the "ramp must advance across silence or a closing client is never removed" scar (`:63-66`) actually pays off;
- the stats windowing at `:727-739`.

Two smaller extractions, both pure, both carrying their own bug scars:
- `fn next_wake(t_est: Option<f64>, period: f64, now: f64, streaming: bool) -> u64` out of `:560-584`. The past-due branch at `:574-578` has the comment "arm for the next future grid point, not a blind full period from now" — a fix nothing checks.
- `AudioSlotReader` (`toyos/src/audio.rs:117-155`) gets its backing region parameterized (a `SlotRegion` trait with `SharedMemory` as the default type parameter — 6 lines, every call site unchanged), so `mix_client`'s four impure lines (`:449`,`:454`,`:482`,`:487`) run the **shipping** peek/advance code over a heap allocation. That keeps the Acquire/Release pairing (`toyos/src/audio.rs:143-149`), the `u32` wrap, and the peek→copy→advance ordering the comment at `:425-430` exists to protect under test, rather than a model of them — `toyos-sched/sim/src/hw_impl.rs:6-8`'s rule verbatim.

The fake device lives entirely in `#[cfg(test)]` at the bottom of `main.rs` and is never a production type. It owns a virtual clock, an inflight queue mirroring `SoundController::submit_buffer`'s rejection rules (`kernel/src/drivers/virtio_sound.rs:314-318`), batched completion masks per the ISR's OR-into-one-record behaviour (`:161-178`), recycle-at-record-pop (`kernel/src/audio.rs:138`), and a play pointer that consumes one buffer per 2.902 ms and **splices silence plus a starvation count when the queue is empty**. That last rule is the whole trick: it turns "soundd woke 35 ms late" from a statistical campaign into an equality.

Output is a `Vec<i32>` handed straight to the analyzer that already ships. `analyze` takes `&Wav` (`tests/common/audio.rs:163`), `Wav`'s three fields are all `pub` (`:51-56`), and `gap_histogram` (`:227`), `format_histogram` (`:239`) and `check_gap_regression` (`:250`) take plain values — no file, no RIFF parsing. The host test and the QEMU gate then quantize into the same period buckets from one implementation.

## 3. Which daemon first

**Confirmed: soundd is the pilot — but it is not the first thing to land, because it is blocked.**

The reason to confirm it is narrower and stronger than "two known bugs exist". It is the only daemon where the negative gate can be **calibrated against numbers someone measured**, rather than against reconstructed intent. `git show 91a653c` records the A/B in its body: one deterministic 35 ms mix-thread stall, four configs, same stall — `16p, 16p, 8p+56p, 8p` = 278.6 ms before, `none, none, none, 1p` = 2.9 ms after, "with max_batch=8 in every fixed run, confirming the drain still happened". That is a 96× ratio handed to the harness as a threshold. `git log --oneline -- userland/netd` is one squashed commit; the compositor's is the same. A gate whose must-fail threshold is guessed proves materially less than one whose threshold is a recorded measurement.

Second reason: half the harness already ships and is already pure (`tests/common/audio.rs:163-260`).

Third reason, which is really an argument about the *class* of bug: both 2026-07-28 defects were in code with no device, no syscall and no kernel underneath it, and both cost a QEMU campaign to find. That is the failure mode a host harness exists to eliminate, and soundd is where it is demonstrated.

**But the first commits are not soundd**, because `userland/soundd/src/main.rs` is in the staged set right now and another agent is in it. The unblocked work is worth landing immediately and neither piece is a host-test question:

1. **`tests/testcases/system.toml` (4 lines).** It is `init = ["/bin/soundd", "/bin/test-runner"]` with `[programs]` = soundd, test-runner, toybox. netd and sshd are not merely untested — they are **not compiled into the artifact under test**. The 138-test suite would stay green if netd failed to build. Add netd, sshd and shell.
2. **`tests/common/qemu.rs:318` (1 line).** It passes bare `user,id=net0` where `src/qemu.rs:51` passes `user,id=net0,hostfwd=tcp::2222-:22`. Adding the forward is what makes any end-to-end SSH test possible at all.

Netd is the *worse-covered* daemon, and that is precisely why its first fix is a config change and not a seam.

## 4. The negative gate

**`old_prime_silence_port`** — named after `old_steal_port` (`toyos-sched/sim/src/scenarios.rs:113-134`) and built on the same three rules.

**Mechanism, copied exactly.** `userland/soundd/Cargo.toml` gains three lines:

```toml
[features]
# Compiles the pre-91a653c recovery path for the harness self-validation gate
# ONLY. src/build.rs never passes it, so it is not in any shipping binary.
bug-port = []
```

Under it, `mix_pass` compiles the `prime_silence` closure recovered verbatim from `git show 91a653c^:userland/soundd/src/main.rs` (old lines 484-494 plus the `*free = 0`), and calls `prime_silence(&mut free_mask)` at the drain site (`main.rs:671`) where the shipping path now does a bare `dll.reset()`. This mirrors `toyos-sched/src/cpu.rs:289-297` — the escape hatch exists, and the kernel cannot reach it.

**The test: `the_step_function_recovery_is_caught`.** An A/B on one schedule — same stall script, same 440 Hz client, same `Xorshift32` seed. Only the recovery branch differs.

```
assert!(new_ms  <  6.0, "shipping recovery regressed: {new_ms:.1}ms");
assert!(old_ms  > 20.0, "the step-function recovery went undetected ({old_ms:.1}ms) — \
                         a harness that cannot catch the bug it was built for is decoration");
assert!(old_ms / new_ms > 4.0, "the A/B must be a comparison, not a coincidence");
```

Calibration: 278.6 ms vs 2.9 ms from the commit body. Thresholds set deliberately slack (4× against a measured 96×) so refining the fake device does not silently disarm the gate.

Per `scenarios.rs:455-462`, the port is **excluded from the normal scenario sweep** — a run that treated it as a scenario to pass would assert the opposite of what it is for. Here that falls out of `#[cfg(feature = "bug-port")]`: the gates only exist in a build that also contains the bugs.

**Second gate, required for a different reason: `old_truncating_quantize_port`.** Compiles `as i16` where `main.rs:242` now has `.round()`, and must fail `dither_is_zero_mean` (input +0.50 LSB DC over 10⁵ samples; the truncating form measured mean +0.13). This one does not protect the code — it protects the **detector**. `tests/common/audio.rs:22-30` records that a `s == 0` silence detector is viable only against a truncating quantizer and that with a correct one "the longest run of exact zeros in 4M silent samples measures 47 — well under the `MIN_GAP_SECS` floor of 88. Such a detector reports 'no dropouts' forever." `SILENCE_MAX = 1` (`:31`) and `dither_ratio` (`:213-218`) exist because of that coupling. No QEMU boot can catch a disarmed detector, because the boot *uses* the disarmed detector. An 11-line pure bug silently removed the integration gate's detection power once already.

Designate `old_prime_silence_port` as *the* gate: it is the one that exercises the pass loop, the fake device and the analyzer end to end. The dither gate is a microsecond unit test that happens to be very important.

## 5. Cost

**This makes soundd's productive code slightly bigger, and here is the number.**

- `PeriodSink` trait: 5 lines. Production impl (holds `dma_ptrs`, calls `audio_submit`): ~14.
- `MixState` struct: ~18 lines of field declarations for things that are `let mut` locals today at `main.rs:519-553`. **This is the real ceremony cost** — locals becoming named fields is the part that reads as bureaucracy.
- `PassOutcome`: ~6.
- `SlotRegion` in `toyos/src/audio.rs`: 6, with `SharedMemory` as the default type parameter so no call site changes.
- `next_wake` extraction and the `mix_pass` body: net zero — blocks become functions.
- The shell loop shrinks by whatever moved out.

**Net: about +45 lines of productive code on 1024, roughly 4%.** In exchange, ~600 of those 1024 lines become reachable from a test that runs in milliseconds. Test code is ~400 lines in a `#[cfg(test)] mod` at the bottom of `main.rs` — not counted, and `#[cfg(test)]` deletes it from the shipping binary.

**The same accounting for the alternatives, so the trade is visible:**
- `corecrate`'s `soundd-core/`: everything above, **plus** a `Cargo.toml`, a `lib.rs`, ~12 `pub` markers on items that are correctly private today, a re-export layer, a second `Cargo.lock`, `rubato` resolved twice, and a fourth root workspace `cargo test` does not reach (CLAUDE.md already documents that wart for `toyos-sched`; this multiplies it). Call it +120 productive lines and one new lockfile for the same coverage.
- `sharedlib`: four new root workspaces and four lockfiles, and it concedes in its own failure_mode that it reaches almost none of the defects the surveys found.

**Churn on everything else: zero.** No fork touched, no `toyos`/`toyos-abi` public path moved, no other userland crate recompiled differently, no `src/build.rs` change (it already sets `RUSTUP_TOOLCHAIN=toyos` at `build.rs:183-186` and passes `--target`). This matters more than it looks: `toyos::net` has five external consumers — `rust/library/std/src/sys/net/connection/toyos.rs`, the mio fork's two TCP files, the socket2 fork, and `userland/libc` — two of which are published fork branches under `forks.toml` discipline. `sharedlib`'s plan would force fork commits to buy test coverage.

**Two things that make the tree smaller, and should land regardless:**
- Delete `resolve_program` (`userland/sshd/src/main.rs:31-37`). Both call sites pass the literal `"/bin/shell"` (`:177`, `:190`), so its non-absolute branch is unreachable. −7 lines; the right action is deletion, not a test.
- Delete `toyos-net/` at the repo root. It is a `.DS_Store` and a March 18 `target/` holding built `toyos_abi`/`toyos_net` rmetas, with no sources and nothing tracked by git (`git ls-files toyos-net` is empty). Leftover from an abandoned root-crate experiment.

## 6. Sequencing

**Unblocked work first; soundd last, because another agent is in it.**

**Step 0 — today, zero structural change.** Add `#![cfg_attr(not(test), no_std)]` and a `#[cfg(test)] mod tests` to `toyos-abi/src/ring.rs`. `toyos-abi` is a standalone package at the repo root, outside the root package and outside the `userland/` workspace, so `cd toyos-abi && cargo test` uses the host toolchain today with no new crate, no seam, no mock and no design review. First test: drive a byte stream through `RingHeader` with mismatched reader/writer chunk sizes across the u32 cursor wrap. **I expect this red** — see `designers_missed`.

**Step 1 — today, 5 config lines.** netd/sshd/shell into `tests/testcases/system.toml`; `hostfwd=tcp::2222-:22` onto `tests/common/qemu.rs:318` to match `src/qemu.rs:51`. Highest coverage-per-line change in the repo. The single most valuable test it unlocks: connect over SSH, run `/bin/spin`, disconnect, reconnect. It should fail today — `child.wait()` at `sshd/src/main.rs:123` blocks a `new_current_thread` runtime, and `/bin/spin` never exits.

**Step 2 — verify the premise before writing anything. One command, `--no-run`:**
`cd userland/soundd && RUSTUP_TOOLCHAIN=toyos cargo test --target aarch64-apple-darwin --no-run`
The static evidence is strong (host `libstd`+`libtest` present under the toyos toolchain with hashes distinct from stable's; `toyos-abi`'s only asm gated x86_64 at `syscall.rs:310` / aarch64 at `:330`, and the aarch64 arm uses x0-x4 only, never x18 which darwin reserves; `src/build.rs:183-186` already drives `RUSTUP_TOOLCHAIN=toyos cargo`). But I was told not to run cargo, so this is inference. **If it fails, the structure argument collapses and `corecrate` wins on the spot** — say so rather than working around it.

**Step 3 — soundd, AFTER the live edit lands. Pure tests only, appended at the bottom of `main.rs`.** No refactor, no seam, append-only, so it cannot conflict with an in-flight edit to the middle of the file. Targets pure *today*: `GainRamp` (`:34-76`), `decode_i16_to_f32` (`:146`), `channel_convert_*` (`:153`,`:160`), `append_planar` (`:166`), `accumulate` (`:196`), `Xorshift32` (`:218`), `dither_and_quantize` (`:240`), `Dll` (`:280-320`), `reject_open` (`:809`). Two properties worth more than the rest combined:
  - dither zero-mean at +0.5 LSB DC over 10⁵ samples;
  - `Dll::update` batch invariance — feed a perfect period grid batched as 1, 2, 4, 8 and `t_estimated` must land on the identical value. That pins the `(n_periods - 1)` correction at `:310` whose reasoning is spelled out at `:301-303` and which is unfalsifiable today.
  Land `old_truncating_quantize_port` with them.

**Step 4 — `next_wake`, then `mix_pass` + `PeriodSink`.** The only irreversible step. Do not start it while `mix_thread` is being edited.

**Step 5 — the fake device and `old_prime_silence_port`.** The 35 ms A/B becomes permanent, and then becomes a *sweep*: stall length 1..64 periods, asserting `lost <= max(0, periods - 8) + 1`. That is the literal claim of `91a653c`'s subject line ("proportional, not a step function") turned into an executable property. The original experiment measured one point; virtual time makes the whole curve free.

**Step 6 — compositor Tier 1 only, no seam at all.** `hit_test` (`:310-378`), `DirtyRect` (`:260-293`), `bring_to_front` (`:118-135`), `scale_wallpaper` (`:210-238`). `Framebuffer::new` already takes a `*mut u8` (`userland/window/src/framebuffer.rs:21`), so a fake display is `Vec<u8>`. Stop there and re-evaluate whether Step 4's pattern earned the compositor's 918-line `fn main`.

**Blocking constraint, stated plainly:** `git status` has `userland/soundd/src/main.rs`, `tests/common/audio.rs`, `tests/toyos.rs`, `tests/common/qemu.rs` and `tests/audio-baseline.toml` all staged, and `tests/audio-baseline.toml` still carries "PLACEHOLDER VALUES — being measured. Do not commit in this state." Gate A is not currently gating anything. Steps 0-2 touch none of those files.

## 7. What is NOT worth testing

**sshd, entirely.** 256 lines of glue; the pure surface is ~17 lines and one of them (`resolve_program`, `:31-37`) is dead code that should be deleted rather than extracted. The handler surface cannot be faked from outside russh — `Channel::new` is `pub(crate)` and `Session` has no public constructor — so a host harness would need either a fork widening (against `forks.toml` discipline) or a redesign of a 256-line program. Its four real defects (per-connection single-slot channel state, the `child.wait()` wedge, stderr merged into stdout, non-idempotent CRLF at `:104-112`) were all found *by reading* and are fixable *by reading*. The one that genuinely needs a test is end-to-end, unlocked by Step 1's config lines.

**The compositor's `fn main` decomposition** (`main.rs:646-1563`, 918 lines, five traits). Not yet, and possibly not ever. Its two worst defects were found by inspection: the stale `usize` window index carried in `Interaction` (`:383,388,389`, used at `:1209` and eleven other sites, invalidated by seven mutation sites), and the unchecked `screen_w - status_w - 12` at `:572` that any client can trigger via `MSG_SET_RESOLUTION`. The right fix for the first is to make it unrepresentable — carry a stable `Fd`, which the same file already does 100 lines earlier at `:731` (`last_title_click_fd: Option<Fd>`). Building a five-trait seam to *test* a bug you can *delete* inverts CLAUDE.md's compile-time-safety ordering.

**`CommandRing` under loom.** 39 lines (`main.rs:101-139`), `CMD_RING_SIZE = 64` — a power of two, so the `% CMD_RING_SIZE` wrap at `:126`/`:135` is safe; I checked. It panics on overflow by design. Getting loom means a second package purely to keep loom's ~30 transitive crates out of the lockfile (`toyos-sched/Cargo.toml:26-31` states exactly this reasoning). One 39-line struct does not earn a package. This is the honest cost of my structural choice, and it is the only reason I would accept for reversing it.

**The resampler's numerics.** `rubato`'s window functions go through libm `sin`/`exp`, which need not agree bit-for-bit between an arm64 host and the x86-64 target, so assertions must be tolerance-based — weak and expensive. Test the code *around* it (`mix_client`'s on-demand pull loop at `:449-461`, the accumulation bound reasoned at `:446-448`, `assert_eq!(produced, device_period_frames)` at `:466`), not its output values. Dither and mix are `+`, `*`, `.round()` — IEEE-exact, byte-exact goldens fine.

**netd's protocol layer.** TCP, UDP, DNS and ARP are stock smoltcp 0.12 (`userland/netd/Cargo.toml`, absent from `forks.toml`, no `[patch]` entry). Conformance tests there test upstream. netd owns lifecycle — socket-id table, pipe attach/detach, backpressure, teardown — and that is where all ten of its found defects live.

**Anything below the seam, and this is the honest ceiling.** `SYS_AUDIO_SUBMIT` has no ownership check; `fd::close` never unmaps a `pipe_map` region (`kernel/src/fd.rs:302-322`) while `close_read`/`close_write` do free the `PhysPage` (`kernel/src/pipe.rs:271-273, 300-302`), leaving netd holding a writable mapping of a recyclable page; the 37 ms median wake lateness against a 23.2 ms pipeline. That last one is the **cause** of the audio dropouts, it is kernel-side, and `specs/assessments/audio-glitch-distribution-2026-07-28.md:120-133` says pinning it needs Layer 2 tracing. Everything proposed here makes soundd's *response* to lateness a controlled variable — which is exactly what `91a653c` fixed — and does nothing about the lateness. Anyone reading this as "host tests will stop the glitches" will be disappointed.

## 8. What the designers missed

**`corecrate`.** Its founding premise — that a root workspace is *required* because `userland/.cargo/config.toml` pins the target — is false in the direction that matters. `toyos-abi` and `toyos` are standalone packages at the repo root, in no workspace at all, and are host-testable today with a one-line attribute. More importantly it read `toyos-sched`'s split as a pattern to copy without checking why it exists: two consumers, two targets, plus loom's lockfile isolation. None of those transfer to a single-consumer std binary, and CLAUDE.md's >2x rule then bites. Its 9-method `Dev` is ~7 methods too many; `now`, `read_completions` and `drain_doorbell` yield values, and values should be arguments. Its fault-injection and negative-gate sections are nonetheless the best of the three and I took them nearly wholesale.

**`traitshell`.** Right about the toolchain, and the only design that actually looked — but it then inherited `corecrate`'s 10-method trait shape without noticing that its own thesis ("the trait, not the crate, is the artifact a reviewer polices") makes the trait's *size* the thing to minimize. It also asserts the daemons are "ordinary `std` binaries [and] `toyos` compiles for host"; soundd is a std binary, but `toyos/src/lib.rs:6` is `#![no_std]` — harmless, since a no_std crate links into a std binary fine, but stated without checking. And it never ran the one command it correctly identified as load-bearing.

**`sharedlib`.** Credit first: **the `RingHeader` wrap bug is real and I confirmed the arithmetic.** `PIPE_SIZE = PAGE_2M` (`kernel/src/pipe.rs:88`), so `capacity = 2 MiB − 64 = 2097088` (`toyos-abi/src/ring.rs:22-24`). `read` and `write` map a wrapping-`u32` cursor through `cursor % capacity` (`:57-58`, `:82-83`) and then assume physical contiguity via `first = count.min(cap - offset)` (`:59`, `:84`), while `available()` is correctly `wrapping_sub` (`:39-42`). Since `2097088 = 2^6 × 32767` and `2^15 ≡ 1 (mod 32767)`, `2^32 mod 2097088 = 64 × 2048 = 131072 ≠ 0` — so the cursor→offset map is discontinuous at the u32 wrap, and the writer crosses it while the reader (at most `capacity` ≈ 2 MiB behind) has not. That is silent stream corruption after 4 GiB on any pipe, i.e. every ToyOS `TcpStream`, since netd bridges through `RingHeader`. And its sharpest observation is verified: **the governing rule is already written down — in a daemon.** `userland/soundd/src/main.rs:970-971` reads "Ring indices wrap mod 2^32, so slot_count must divide it evenly" with a `is_power_of_two()` assert, and that rule is absent from the shared ring the kernel, netd, std, mio, socket2 and libc all sit on.

But its headline is **overstated**. `userland/doom/src/sound.rs:569-570` is `(left[i] * 32767.0).clamp(...) as i16` with **no dither at all**. soundd's defect was truncation *paired with* TPDF dither — that pairing is what produced the 2-LSB dead zone and the collapsing noise floor described at `main.rs:233-239`. Doom's is a plain ~0.5 LSB DC bias: real, worth fixing, and a different and much smaller thing — especially since doom's samples are then re-quantized by soundd's correct dithering path. It is not "the same bug still live one crate away", and the whole "the daemon is the wrong unit" argument rests on it. Its four-root-workspace estate is also precisely the growth the owner is worried about, and its own failure_mode concedes it reaches almost none of the defects the surveys found.

**Missed by all three:**
1. **The seam can be a value boundary, not a world trait.** That is what turns the ceremony cost from ~120 productive lines into ~45.
2. **`AudioSlotReader`/`AudioSlotWriter` (`toyos/src/audio.rs:57-155`) have the identical shape to the ring bug — `idx % slot_count` on a free-running `u32` — and are SAFE.** Reader and writer map the *same* counter value for a given slot, so a discontinuity at the wrap moves both sides identically. Same for `CommandRing` (`CMD_RING_SIZE = 64`) and doom's `MusicRing` (`RING_FRAMES = 131072`, and its cursors are `AtomicUsize`, `sound.rs:505,528-529`). Worth writing down so nobody "fixes" three non-bugs while chasing the one real one.
3. **`toyos-net/` at the repo root** — a `.DS_Store` and a March 18 `target/` with built `toyos_abi`/`toyos_net` rmetas, no sources, nothing tracked by git. Dead weight from an abandoned root-crate experiment; delete it. Minor, but it is exactly the "the codebase only grows" symptom, and it argues quietly against minting more root workspaces.
