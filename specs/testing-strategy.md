# The testing strategy

**What this is.** The owner asked a clean-slate question on 2026-08-12: *does it
make sense to move all of the QEMU tests into the nightly tier, run only unit
tests on PR CI, and run the QEMU suite locally? What is the optimal setup?* This
document answers it from the instruments' own record. **It proposes; every
decision point is marked.** Nothing here changes an assertion, a tier or a
workflow — `specs/test-cost-audit.md` is what the suite costs,
`specs/ci-plan.md` is what CI is, and this is the policy those two are evidence
for.

Every number below came off a command that was run, and the run ids are given so
each can be re-read. Where a figure is arithmetic on measured components it says
**(derived)**.

**The recommendation, before the evidence for it.** Keep a guest gate on every
pull request and make it exactly the compute-bound core; move everything
timer-anchored, plus TCG breadth, audio's thorough tier and stress, to a nightly
job that must be *built* before more is moved into it; keep the local suite as
feedback and as the only contention instrument, never as a gate. **Unit-only PR
CI is worth 161 seconds — 2.7 minutes — and would leave 99.1% of the kernel
reviewed by nothing until the next night**, over a tree whose two worst
kernel-isolation defects are caught by three guest tests costing 48 milliseconds
between them. Running the suite locally as the gate is already refused by the
owner's own decision to retire `--land`: the dev host is cross-arch TCG and
cannot execute the class that decision was made for.

---

## 1. The strategic goal

**Every defect class has exactly one instrument that owns it, and every PR
verdict is deterministic for its author.**

Two clauses, and they are not the same claim.

- **One owner per class.** `specs/ci-plan.md` §8 already says the dev host and a
  CI shard are two instruments and not two readings of one quantity. The goal
  extends that: for every class of defect this tree can suffer, exactly one gate
  is *responsible* for catching it, and that gate's red is the class's alarm.
  Where a class has no owner it is named as unowned rather than left to whichever
  instrument happens to trip over it. The failure this prevents is the one
  `specs/ci-plan.md` §8.3 records twice — a red read as being about the tree when
  it was about the difference between two machines.
- **A deterministic PR verdict.** A red on a pull request must be a statement
  about the author's diff. A gate that reds at a rate is not a gate; it is a
  coin the author has to flip until it lands, and every flip costs the runner
  budget that makes the next author wait. `specs/ci-plan.md` §9.2 already sets
  this bar — *green means green, red means a real defect, and a re-run tells you
  which* — and records that one run in five is "far above the one in fifty that
  bar treats as tolerable".

**The two clauses conflict, and that is the whole design problem.** Determinism
per PR argues for a small, compute-bound gate. Owning every class argues for the
gate that can execute the class — and the classes this tree has actually been
bitten by are overwhelmingly *not* compute-bound. The rest of this document is
where the line goes and what it costs on each side of it.

**A third goal, deliberately not adopted: minimum PR latency.** §2 measures what
latency is made of here, and the answer is that test verdicts are **5% of the
critical-path job**; boots and image builds are 28%, and 65% is pipeline that one
test would pay as readily as 246. Optimising the 5% by removing the only evidence
the tree has for most of its defect classes is the trade this document declines —
and §8 names where the minutes actually are.

---

## 2. What a pull request actually costs, measured

This is the section the owner's question turns on, so it is first.

### 2.1 The runs

Three complete `ci` runs, read with `gh api .../jobs` and
`gh api .../logs`, decomposed per shard into **setup** (container start, `apt`,
checkout, `rustup`, toolchain install, cache restore — everything before
`cargo test`), **build** (everything inside the suite step before the harness's
first boot: the host crates, 110 C test binaries, the Rust guest binaries, the
kernel/bootloader/userland clean and rebuild), and **harness** (the suite's own
`test result: … total (Xs)` line, which contains every image build, every QEMU
boot and every verdict).

| run | tree | tier | run wall | slowest guest job | Σ harness, 12 shards | runner-minutes |
|---|---|---|---:|---:|---:|---:|
| `31472065811` | `main` | full, 297 tests | **786 s** | **748 s** | **3,719.0 s** | 134.3 |
| `31495665450` | `wt/toyos-slowtests` | 31 relegated | 730 s | 677 s | 2,358.6 s | 112.9 |
| `31601279765` | `wt/toyos-slowtests` | 60 relegated, 246 tests | 1,115 s * | **542 s** | **1,646.2 s** | 105.0 |

\* `31601279765` ran its twelve shards in two waves because three other pull
requests were in flight; §2.4 prices that separately.

### 2.2 The fixed cost of a guest shard does not move with the tier

Across all 36 guest jobs in the three runs:

| | setup | build | harness |
|---|---:|---:|---:|
| `31472065811` (full) | 83 / 91 / 167 s | 185 / 227 / 232 s | 175.6 / 337.6 / **427.4** s |
| `31495665450` (31 out) | 88 / 94 / 148 s | 175 / 227 / 250 s | 64.6 / 198.7 / **325.9** s |
| `31601279765` (60 out) | 90 / 103 / 178 s | 183 / 239 / 249 s | 98.7 / 130.3 / **181.5** s |

(min / median / max)

**Setup and build are invariant at roughly 90 s + 230 s = 320–340 s per shard,
and the tier does not touch either.** They are the price of *having a guest job
at all*. The harness column is the only one that responds to what the tier
holds back.

The build's 230 s is not mysterious and is already filed:
`specs/issues/build/external-dep-fingerprint-is-mtime-not-content.md` — every
job rebuilds `toyos-ld` from the same sources to the same bytes, the mtime moves,
and `invalidate_stale` then cleans the kernel, the bootloader, userland and the
Rust test crates. The eight `external deps changed: cleaning` lines are in shard
1's own log of run `31601279765`, and `specs/ci-plan.md` §12.5 measured them in
both arms of the cache probe.

### 2.3 What the nightly relegation actually bought, and what it was advertised as

`src/tiers.rs` (branch `wt/toyos-slowtests`) records that the 60 relegated
registrations account for **4,083.8 s of 4,433.8 s — 92.1% — of the effective CI
duration profile**. The suite prints that figure on every run.

Measured against wall clock, the same relegation bought:

| | full tier | fast tier | change |
|---|---:|---:|---:|
| Σ harness across 12 shards | 3,719.0 s | 1,646.2 s | **−56%** |
| slowest shard's harness | 427.4 s | 181.5 s | **−58%** |
| slowest guest job | 748 s | 542 s | **−28%** |
| `ci` run wall (unqueued) | 786 s | ~590 s (derived) | **−25%** |
| runner-minutes | 134.3 | 105.0 | −22% |

**92.1% of the profile is 25% of the pull request.** The gap is not an error in
either number; they measure different things. The duration profile is a
partition weight — since `8b68e8b` on that branch it deliberately excludes the
artifact build a boot needs — while the pull request pays setup, build, image
builds and QEMU boots whether a shard runs four tests or forty. This document's
position is that **the profile is the right instrument for partitioning shards
and the wrong one for pricing policy**. `src/tiers.rs`'s headline should carry
the wall-clock figure beside the profile one, because the profile figure is the
one a reader will take for a saving.

### 2.4 The floor is the host job, and the queue is bigger than the suite

The other required checks on the same head, run by run for `31601279765`:

| check | workflow | wall |
|---|---|---:|
| `host` | `host-tests.yml`, `macos-latest` | **381 s** — setup+cache 44 s, `cargo test --lib` 32 s, the fourteen host crates 231 s, sshd 56 s |
| `abi-split` | `landing.yml` | 37 s |
| `gate-stage` | `landing.yml` | 2 s |
| `build` (toolchain) | `toolchain.yml`, warm | 8 s |
| `tcg` | `ci.yml` | 436 s |

**So a pull request cannot conclude faster than the `host` job no matter what is
removed from it**, because it is on a macOS runner and is compilation almost end
to end (`specs/ci-plan.md` §5: 594 s cold, 442 s warm, on a shorter crate list
than today's).

**That floor is itself variable and the range is worth stating: 274–580 s over
`host tests`' nine most recent runs**, `gh run list -w 'host tests'`, 2026-08-11
to 2026-08-12. Every comparison below uses the *same head's* figure — 381 s on
run `31601279765`'s head — because comparing a guest job on one day against a
host job on another measures the cargo cache and not the gate. At the shorter end
of that range the guest gate costs more than the numbers here say, and at the
longer end it costs nothing at all.

Two consequences, and they decide the whole question:

- **The first 45 seconds of guest harness time are free.** A shard's fixed cost
  is 320–340 s and the floor is 381 s, so a guest job only appears on the
  critical path once its harness exceeds the difference. A pull request that
  boots one cheap guest and a pull request that boots none finish at the same
  minute.
- **Twelve guest shards at the current fast tier cost 542 − 381 = 161 s of pull
  request latency** over a unit-only gate, and **at the full tier 748 − 381 =
  367 s**. (derived from the measured jobs above.)

And the queue is larger than either. Run `31601279765` took 1,115 s where its
own slowest job was 542 s: shards 1, 2, 3, 5, 7, 8 and 11 did not start until
13:31:59–13:36:17, seven to eleven minutes after the run opened, because
`codex/debug-wait-census`, `wt/toyos-std` and `wt/toyos-logd` had runs open at
the same time and a public repository on the free plan runs twenty jobs at once.
**Queueing cost that pull request ~525 s — more than three times what its entire
guest gate cost it.** `specs/ci-plan.md` §5 recorded the same effect from the
other end: a required three-second check queued twenty-two minutes behind twelve
advisory shards.

### 2.5 The answer to the owner's arithmetic

*If all guest tests left PR CI, how many minutes would a PR actually save, and
what fraction is coverage versus pipeline overhead?*

- **From today's fast tier: 161 s — 2.7 minutes.** (542 s slowest guest job
  against the 381 s `host` floor, same head.)
- **From the full tier: 367 s — 6.1 minutes.**
- **Of those 2.7 minutes, the fraction that is guest test execution rather than
  pipeline is small.** On the critical-path shard of `31601279765` (shard 4,
  542 s), the harness spent 181.5 s and the duration artifact attributes 27.7 s
  of that to the six tests' own verdicts; the remaining ~154 s is image builds
  and QEMU boots, and the 354 s before the harness ran is setup and build. **In
  round terms: 5% of the critical-path job is test verdicts, 28% is boots and
  images, 65% is pipeline that one test would pay as readily as 246.**

**So the owner's instinct is arithmetically sound and directionally expensive.**
Unit-only PR CI is real and it is 2.7 minutes at today's tier. What it buys back
is a gate that cannot execute any of the classes §3 shows this tree is actually
bitten by, over a kernel that §4.4 measures as **99.1% guest-only**. §5 prices
what each of those classes then costs to detect elsewhere.

---

## 3. The instruments, and what each one owns

Five instruments exist. Only three of them are typed anywhere —
`src/redlist.rs`'s `Instrument::{Ci, DevHostAlone, DevHostLoaded}`, each carrying
its own `cannot_say()` — and that file states the two omissions plainly: metal is
not an instrument there because the suite does not run on the T14, and gate A's
thorough tier is not a row because its verdicts are `Fisher p=…` rather than a
name going red.

### 3.1 The matrix

| class | owner | why nothing else can | receipt |
|---|---|---|---|
| **Which vendor's reading of an instruction the kernel depends on** — `syscall`/`sysret`, segment loads, `iret`'s privilege checks | **KVM CI** | QEMU's helpers implement Intel's wording, so a TCG guest gives you one vendor and the dev host has no other. The vendor a job draws is a lottery, so the gate is *the matrix*, not one job | `specs/ci-plan.md` §7: `STAR[63:48]` green on the dev host in every run there had ever been; eight EPYC runners lost **64 boots of 64**, two Xeon runners passed. `specs/issues/kernel/sysret-ss-attrs-unfixed.md` |
| **CPU state leaking between processes at native FPU speed** | **KVM CI** | The x87 `#MF` reproduced 5 of 5 on KVM and never on the dev host; the isolating probe changed one control word and nothing else | `specs/ci-plan.md` §9.3, §10.8; `probe-x87` run `31260763462`, arms `0x037E` 3/3 red and `0x037F` 3/3 green; `src/redlist.rs` rows for `std_unwind`, `std_unwind_so` |
| **Device races the guest only reaches at native speed** | **KVM CI** | KVM runs the guest ~50× further between the host's two QMP writes. `usb_transport_break` was 5 of 5 red on CI, green on the dev host *and* green under TCG on the same runner image and the same QEMU | `probe-xhci-break.yml` run `31264371902`: control arm 3/3 red, fixed arm 3/3 green, one runner, one session. `specs/issues/hardware/xhci-flap-wedges-under-kvm.md` |
| **When two runnable tasks first run** | **KVM CI** | Two siblings spawned 1–3 ms apart reach their first line 0.53–0.56 s apart with the order flipping between reps; on the dev host the same pair is ~30 ms apart in spawn order every time, because its TCG runs one vCPU at a time | `specs/issues/hardware/process-start-skew-on-a-runner.md` |
| **Contention: two guests on one machine, the whole `ALONE: GREEN` class** | **the dev host, loaded** | A shard is one guest per machine at `--jobs 1`. There is never a second guest for the first to contend with, so the class is untestable on a runner *by construction* | `specs/ci-plan.md` §8.2; `specs/issues/build/parallel-tests-red-under-other-suites.md`; nine of the ten dev-host-only red names in `src/redlist.rs` are `Instrument::DevHostLoaded` |
| **Concurrent unmaps deadlocking two shootdown initiators** | **the dev host, loaded** — found; **loom** — gated | Every one of the seven wide-phase failures was green run alone. The field signature is not a gate; the gate is the model that replays the schedule | `specs/issues/audio/wide-phase-reds-under-load.md`; `0c79fb5`; loom's `an_initiator_answers_while_it_waits` |
| **Memory ordering** | **loom, or nobody** | x86's TSO makes every load an acquire and every store a release, so **no guest test on the only architecture ToyOS boots can fail on a missing edge**. `Lock::try_lock` loaded with `Relaxed` and CASed with `Acquire` — no synchronizes-with edge ever formed, through a `Lock<T>` that is `unsafe impl Sync`, at eight call sites | `cdc971d`; the negative control reverts the two tokens and loom reports `Causality violation: Concurrent write accesses to UnsafeCell`. Second instance, `3b3d238`: the log shard's reader accepted an uncommitted record 0 on every shard on every boot |
| **A device whose emulation declines the feature** | **metal** | QEMU reports zero xHCI scratchpad demand and answers `OP_PAGESIZE` bit 0, so a misaligned scratchpad array overlapping slot 2's output context is green everywhere. Found by reading the code against §4.20, not by a test | `specs/metal-track-history.md`, "What QEMU structurally could not find", `5bb673c`, `71940c1` |
| **Firmware variance** | **metal** | The T14 hands over an uninitialised 8042 about one boot in seven; QEMU's controller never drops a config write and never resets itself. A rate only repeated boots on the machine can measure | `specs/issues/hardware/t14-hands-over-an-uninitialised-8042.md`: seven boots of one image, six read `cfg=0x77→0x64`, one read `cfg=0x30→0x60` |
| **Undefined behaviour below the software layer** | **metal** | A memory-type alias on one physical page — WC on one CPU, WB in another's stale TLB — is invisible to TCG, which models no memory types at all, and is the one thing in the freeze window that can stop a machine with no panic, no schedule and no interrupt | `specs/issues/hardware/pulling-the-boot-stick-freezes-the-t14.md` |
| **The emulated path itself, `-cpu qemu64`** | **the `tcg` job** — nominally | §3.3 |
| **CPU state whose only consequence is timing on silicon** | **nothing** | §3.4 |

### 3.2 What the two automated gates have actually caught, counted

Counted from `src/redlist.rs`'s 69 rows, `Finding::Quiet` excluded because a zero
is not a red:

| | names |
|---|---:|
| carrying a CI red row | 23 |
| carrying a dev-host red row | 18 |
| both | 8 |
| **CI red, never recorded red on the dev host** | **15** (16 with `process_stats`, which predates the index) |
| **dev host red, never recorded red on CI** | **10** |

The fifteen: `doom_sound_flood`, `hda_client_stall`, `hda_two_live_refused`,
`kernel_heartbeat`, `late_storage_connect`, `metal_sim_null_audio`,
`metal_sim_pointer_churn`, `sshd_fail_closed`, `std_unwind`, `std_unwind_so`,
`usb_disk_index_stable`, `usb_transport_break`, `xhci_flap`, `xhci_hotplug`,
`xhci_slow_connect`.

The ten: `audio_tone_load`, `desktop_locale_detect`, `desktop_typing_damage`,
`i8042_absent`, `i8042_mouse`, `metal_sim_window_caps`, `netd_connection_caps`,
`null_sink_shipped_client`, `screen_console_scroll`, `screen_early_panic`. Nine
are `DevHostLoaded`.

**Read that with `src/redlist.rs`'s own caveat: a name with no rows answers `NOT
ON THE LIST`, which is a claim that nobody measured it and not a claim that it is
green.** The counts are of *recorded* asymmetry, not of every defect.

The cleanest single "dev host red, CI green" pair is `screen_pager_keys`: `QUIET
0 of 5` on CI against `FIRES 3 of 3` on the dev host **alone**, with the dev-host
row explicitly ruling load out — the gate that produced one of them ran at 1.05×
the reference boot and the failure was byte-identical to the ones taken at load
11–16 (`specs/issues/diagnostics/screen-pager-keys-red-on-main.md`).

### 3.3 The `tcg` job is a required check that has never named a tree defect

Searched: `specs/ci-plan.md`, the whole issue estate, `.github/workflows/ci.yml`
and the git log. **The `tcg` job appears in three roles and none of them is
"found a defect in the guest".**

- **As a control arm in a probe**, where it earned its keep — `specs/ci-plan.md`
  §7.3's 2×2 isolated `desktop_typing_damage` as the *QEMU version* rather than
  the tree, and the two TCG cells are the strongest thing in that table because
  their boots are the same speed and one is green throughout. But that was a
  whole `probe-*` shard, not the one-test canary.
- **As a self-diagnosis of the harness**: it ran `process_stats` green on one
  push and `timed out after 5s` on the next, same command, same commit content,
  which is what demonstrated host-scaled ceilings were needed (§7.2).
- **As anti-rot cover, by declaration.** `.github/workflows/ci.yml` states the
  purpose: "a configuration nothing runs is a configuration nobody finds out
  about", and it is the only thing in CI that boots `-cpu qemu64`.

So its required status rests on the owner's rule — "everything should work under
emulation and kvm if it doesnt something with the guest is wrong" — and not on a
caught defect. It is also **the most expensive single required check**: 436 s in
run `31601279765`, and 624 s cold / 409 s warm in `specs/ci-plan.md` §12.5. Two
couplings before anyone proposes removing it: it very nearly *was* a vacuous
required check (§11.1 — GitHub reads a job skipped by an unmet `needs:` as
satisfied, so it carries `if: always()` and a guard step), and **it is the job
that writes the `actions/cache` entry the twelve shards only read**, worth
3,036 s of runner time per run (§12.5). That is a coupling, not a justification.

**Owner decision point (D1).** The `tcg` job's honest description today is *an
anti-rot declaration and a cache writer wearing a gate's badge*. Three ways to
resolve it are in §7.

### 3.4 One class has no owner, and both automated gates are green on it for opposite reasons

Until 2026-08-08 an AP kept the `INIT` value of `CR0`: every core but cpu0 ran
with caching disabled and `WP`, `NE`, `MP` clear, for the whole history of the
tree.

- **KVM could never have failed on it.** The AP arrives with `CD`/`NW` already
  clear under the hypervisor. CI shard 3 read `cr0=0x80000011`.
- **TCG could never have failed on it either.** The bit is architectural state
  with no timing consequence in the emulator. The dev host read
  `cr0=0xe0000011`.

Both readings are in `specs/issues/kernel/ap-control-registers-inherit-init.md`,
from one commit. **A defect can be simultaneously invisible to both automated
gates, in opposite ways — one masking it by fixing it, the other by not
modelling it.** The instrument that would answer is built
(`--kernel-param control-regs-bench`) and has never been run on silicon, which is
why root `CLAUDE.md` records that every multi-CPU measurement this project has
taken was of a machine that no longer exists.

**This is the reason the matrix above is written down at all.** A class with no
owner is invisible unless somebody is keeping a list of owners.

### 3.5 The bound on all of this

`specs/metal-track-history.md` counts **46 code defects and 24 test defects — 70
— plus 34 corrections to records**, across seven adversarial review waves over
`0d2a324..b33b231`, in code whose own suites were green and whose commit messages
carried measured A/Bs and negative-teeth demonstrations. Three of its findings
are worth quoting into a strategy document because they bound what *any* gate
composition can promise:

- `serial::init`'s loopback `assert!` killed the kernel about twenty
  instructions into `kernel_main` on a machine with no 16550, because absent
  hardware reads `0xFF` and `0xFF` passes both the THR-empty test and "receiver
  ready". Every dev config has a 16550, until `--metal-sim` existed.
- The I/O APIC reported success three separate ways on a machine with nothing
  wired up — and the test certifying it asserted `total == hi - lo + 1` against a
  line printing `hi = gsi_base + entries - 1`, **a tautology**. The subsystem and
  its certification were both wrong and both green.
- The PS/2 mouse framer's "resync within ≤2 packets from any offset" was false,
  and the host test certifying it used `packet(0, 5, 7)` — the unique delta pair
  whose body bytes cannot masquerade as heads.

**So: no gate composition in this document is a claim that green means correct.**
The tree's own rule is the one in root `CLAUDE.md`: *mutating your implementation
tests the paths you wrote, never the states you did not think to construct.* What
a gate composition decides is only **which machine gets the chance to disagree,
and how soon.**

---

## 4. The coverage census

### 4.1 What is registered, and where each tier sits

Parsed from `tests/toyos.rs` on `wt/toyos-slowtests` and priced from that
branch's `tests/test-durations` (307 rows, 4,439,089 ms — the figure
`src/tiers.rs` prints).

| registry | `tests/toyos.rs` | total | Fast | Nightly |
|---|---|---:|---:|---:|
| `MACHINE_TESTS` | 362–637 | 110 | 56 | 54 |
| `SCREEN_TESTS` | 306–332 | 17 | 12 | 5 |
| `AUDIO_TESTS` | 285–286 | 2 | 0 | 2 |
| **registered** | | **129** | **68** | **61** |
| shared-boot members | `SHARED_TIER = Tier::Fast`, `:106` | 176 | 176 | 0 |

| | ms | s |
|---|---:|---:|
| the 68 Fast registrations | 324,581 | **324.6** |
| the 61 Nightly registrations | 4,105,715 | **4,105.7** |
| the 176 shared-boot members | 8,793 | **8.8** |

(61 rather than the 60 run `31601279765` printed: `audio_tone` was relegated by
`118e3b7` *because of* that run's measurement, which is the tier gate working.)

**Eleven of the 68 Fast registrations boot no guest at all** — `screen_decoder`,
`serial_vocabulary`, `suspend_detector`, `suspend_invalidates_a_verdict`,
`stall_is_not_a_verdict`, the three `expected_failure_*` gates,
`control_regs_verdict`, `suite_split`, `nightly_tier_is_announced`. They are the
harness testing itself, at 0–20 ms each, and they are compute-bound by
construction. **57 Fast guest tests are what the sweep has to classify.**

### 4.2 The sweep, and it is a smaller change than it sounds

`tests/toyos.rs` and every `tests/common/*.rs` helper were searched for
`thread::sleep`, `Instant::now` and `elapsed`, and each hit mapped to the test
that encloses it.

**Four Fast guest tests have a verdict a clock decides**:

| test | CI ms | what makes it so |
|---|---:|---|
| `metal_sim_null_audio` | 8,546 | `tests/common/audio.rs:838-869` bounds `start.elapsed()` at `MIN_SECS 2.5 ..= MAX_SECS 8.0`, and `:884-892` a period count of 700..=1500 |
| `null_sink_shipped_client` | 6,860 | its guest binary plays 2 × 1 s of `/bin/tone` in real time |
| `netd_hostile_peer` | 4,678 | the guest measures netd's handshake against its own clock — `ANSWER_TIMEOUT 2 s`, `BURST_PACE 1 ms`, `SETTLE 100 ms` |
| `screen_diag_boot` | 7,030 | `tests/toyos.rs:2362` sleeps 5 s, and the comment above it makes the hold *the assertion*: "Holding two orders of magnitude longer than that is what makes 'indefinitely' a measurement rather than a claim" |

**Ten more have a textual verdict and a real-time floor under their price** —
they wait out a staged window or inject at a fixed pace, so a 2× slower machine
changes what they cost and, at the margins, whether they pass:
`i8042_fadt_denial` (5 keys × 20 ms), `i8042_kbd_echo`, `i8042_undecoded_bytes`
(200 ms), `xhci_hotplug` (three 800 ms holds against a 100 ms debounce),
`usb_disk_index_stable` (1,200 ms), `usb_refused_disk_first` (two × 1,200 ms),
`locale_detect`, `locale_detect_unrecognized`, `console_locale_detect` (60–120 ms
per key), `screen_blocked_dump` (a 2 s settle inside a 15 s hold the guest is
timing).

Three are borderline and flagged rather than counted: `i8042_quarantine` (a line
count over its own window, but 1 healthy against 2,685 measured), `wall_clock_file`
(a 300 s drift bound against a ~5 s boot), `hda_probe` (a fixed 1 s drain, textual
verdict).

| reading | swept | Fast guest tests left | of 68 Fast | CI ms swept |
|---|---:|---:|---:|---:|
| strict: a clock decides the verdict | 4 | 53 | 64 | 27,114 (8.4%) |
| **the owner's rule as worded** — verdict **or price** | **14** | **43** | **54** | **99,987 (30.8%)** |
| + the three borderline | 17 | 40 | 51 | 120,207 (37.0%) |

**Recommended: 43 of the 57 Fast guest tests survive, and the fast tier loses
100.0 s of its 324.6 s.** Three of the fourteen are §5.3's standing flakes.

**And the two rules already agree almost everywhere.** Every remaining sleep in
the tree belongs to a test that is *already* Nightly — `xhci_flap`,
`xhci_hid_break`, `usb_flush_optional`, `usb_boot_stick_pulled`,
`kernel_log_file`, `xhci_msi_only`, `i8042_health_cadence`,
`metal_sim_window_drag`, `swiss_german_layout`, `i8042_keyboard`,
`i8042_no_spurious_wake`, `desktop_locale_detect`, `screen_console_scroll`,
`screen_paged_scrollback`, `screen_pager_keys`. The ten-second ceiling and the
real-time rule are two ways of finding mostly the same set, which is a useful
thing to know: the ceiling has been doing the rule's work by accident, and the
sweep is finishing it rather than starting it.

### 4.3 The shared boot is 176 tests for 8.8 seconds, and 62% of that is three sleeps

110 C conformance tests (exit 0 plus stdout byte-compared against a committed
`.expect`, covering preprocessor, integer and float semantics, struct return,
VLAs, varargs, bitfields, function pointers, scoping) and 66 Rust binaries (exit
0, four of them also reading serial): sixteen `abuse_*` kernel-hardening
refusals, thirteen `std_*` conformance tests, the capability and object-lifetime
family, memory and scheduling, filesystem and storage, IPC and ports,
introspection.

**None of the 110 C tests is timer-anchored. Ten of the 66 Rust binaries are, and
they are 5,490 ms of the Rust half's 7,587 ms — 62% of the whole block:**
`null_sink_client_exits` 2,216 ms (plays 2 × 1 s in real time), `sched_stress`
1,300 ms, `audio_idle_suspend` 1,066 ms, `io_uring_cancel_wakes` 522 ms
(`PARK_MARGIN = 500 ms`), `process_lifecycle` 217 ms, and five more at 10–60 ms.

Two things fall out and neither is this document's to fix:

- **The block's boot is attributed to no member.** `run_task`'s `Task::Shared`
  arm times each member around its own `run_test`, so 8.8 s is in-guest test time
  and the block's wall clock is that plus a boot. Any policy priced on the
  duration profile inherits that, which is §2.3's point in miniature.
- **`null_sink_client_exits` runs twice under two names** — once as a shared-boot
  member at 2,216 ms and again inside the machine test `null_sink_shipped_client`
  at 6,860 ms on the `Metal` profile. `check_no_collisions` cannot see it because
  the names differ. `specs/ci-plan.md` §7's "four tests were running twice under
  one name" is the same shape one level along.

Also stale and worth a line from whoever next touches the file:
`tests/toyos.rs:100` and `:127` still say "153 binaries" and "153 tests on one
boot costing about thirteen seconds".

### 4.4 The kernel has essentially no host-side test, and this is the number the owner's question turns on

Counted across `kernel/`:

- **`#[cfg(test)]` modules: 1** — `kernel/src/mm/user_span.rs:55`.
- **`#[test]` functions: 6** — all in that file.
- They do not run from `kernel/` at all. They execute because `kernel-span`
  `#[path]`-includes the file into a host crate, and `kernel-loom` does the same
  for `sync.rs` and `shootdown.rs`.

**Kernel source compiled on a host: 434 lines of 47,779 — 0.91%.**

| subsystem | host-tested? |
|---|---|
| `mm/user_span.rs` (140 lines) | **`kernel-span`**, 6 tests |
| `sync.rs` (152) | **`kernel-loom`**, 2 tests |
| `shootdown.rs` (142) | **`kernel-loom`**, 3 tests |
| the rest of `mm/` — alloc, pmm, paging, region, mmio | **guest only** |
| all of `arch/` — apic, control_regs, entry, fpu, gdt, `idt/`, mtrr, pat, percpu, smp, syscall, tlb | **guest only** |
| all of `object/` — handle, namespace, port, pipe, process, service, shm, syscap, device, file | **guest only** |
| `process.rs`, `scheduler.rs`, `sched/` | **guest only** — the *algorithm* is `toyos-sched`'s 41 host tests; the kernel-side driver and the cutover are not |
| `io_uring.rs`, `pipe.rs`, `vfs.rs`, `tmpfs.rs`, `page_cache.rs`, `block.rs` | **guest only** |
| `fat32_adapter.rs`, `bcachefs_adapter.rs`, `gpt.rs`, `elf/`, `loader/` | **guest only** — the *formats* are host-tested in `toyos-fat32`, `toyos-fat32-check`, `bcachefs`, `toyos-gpt`, `toyos-elf`; the kernel adapters are not |
| `iommu/` (+ `vtd/`) | **guest only** |
| `drivers/` — i8042, xhci, hda, pci, acpi, gop, ioapic, log_ring, nvme, serial, usb_storage, every virtio, **and the panic console** | **guest only** — the *pure* halves are `toyos-ps2`, `toyos-xhci`, `toyos-hda`, `toyos-pci`; every kernel driver is not |

**So: memory and paging, syscall entry, IPC and the object table, VFS, process
lifecycle, IOMMU, interrupt and APIC routing, SMP bring-up, fault handling and
the panic console have no host-side test of any kind.** Every one of them is
verified only by booting a guest.

**This is the answer to "run only unit tests on PR CI", stated as a number: a
unit-only gate reviews 0.91% of the kernel.** The other 99.09% would be gated by
a nightly job that does not exist yet, on a machine that runs 5–17 merges a day.

### 4.5 And the unit gate itself has holes, found while counting

`host-tests.yml`'s loop names fourteen crates; root `CLAUDE.md` names sixteen.
`toyos-abi` (17 tests) and `toyos-manifest` (6) are in the documentation and in
no workflow, and `bcachefs` (71) is in neither. **94 host tests that no gate
runs**, filed as
`specs/issues/build/three-host-crates-are-tested-nowhere.md`.

`toyos-manifest`'s round trip is the thing that makes the build system's renderer
and `/bin/init`'s parser one format, and nothing runs it per pull request. This
is the same defect `specs/ci-plan.md` §5 recorded when four pure crates were
missing from the same loop until 2026-08-08 — "CI was skipping the cheapest tests
it had" — and it recurred because the list lives in two places with nothing
holding them against each other.

**It is worth saying out loud in a document proposing that unit tests carry more
weight: the unit gate is not currently complete, and completing it is cheaper
than any other coverage in this document.**

---

## 5. The PR gate

### 5.1 The proposal

**Keep a guest gate per pull request, and make it exactly the compute-bound
core.** Concretely, the required checks stay the five they are today —
`host`, `abi-split`, `gate-stage`, `tcg`, `guest-suite` — and the *contents* of
`guest-suite` become the fast tier with the owner's real-time rule swept across
all of it, not only across the tests that happened to cross ten seconds.

The three-line version:

- **PR gate** = the host and unit suites, plus every guest test whose verdict is
  compute-bound: an exit code, an expected stdout, a decoded structure, a
  host-side byte comparison.
- **Nightly** = everything timer-anchored, plus TCG breadth, plus audio's
  thorough tier, plus stress.
- **Local** = developer feedback and the contention instrument. **Never a gate.**

### 5.2 Why a guest gate stays, in one measurement

The two most severe defects CI has ever found are gated by three tests that cost
**48 milliseconds between them** and share a boot with 187 others:

| test | fast-tier CI price | the class it owns |
|---|---:|---|
| `process_stats` | **16 ms** | the AMD `SYSRET`/`STAR[63:48]` `#GP` — 64 boots of 64 lost on an EPYC |
| `std_unwind` | **15 ms** | the x87 `#MF` that let any Ring 3 process kill the next unrelated one scheduled on that CPU |
| `std_unwind_so` | **17 ms** | the same, on the thread that panics |

(Measured labels from run `31601279765`'s shard-2 duration artifact — shard 2 is
the one that carried the shared block.)

All three ride the shared boot, and the shared boot is the cheapest thing in the
tree by a margin that is hard to overstate: **176 registered tests for 8,793 ms
of in-guest time**, summed from `tests/test-durations`. On the run decomposed
here, 183 of shard 2's 192 labels are sub-second and cost **3,725 ms between
them**; the nine labels over a second on that shard cost 34,843 ms, and three of
those nine are shared-block members that sleep (§4.3).

`usb_transport_break` — the xHCI Bulk-Only Reset race that only a native-speed
guest reaches — is in the same tier at **6,911 ms**. So **the fast tier already
retains three of the four defect classes §3.1 assigns to KVM CI**, and it retains
them cheaply.

**One asymmetry to note rather than fix here.** `fpu_isolation` — the *negative
control* for the x87 fix, the boot of an `fpu-save-nothing` kernel that has to
fail the same three arms — is Nightly at 67.7 s. So the gate is Fast and free and
the evidence that the gate has teeth is in the tier with no reader. That is the
general shape of what relegation did, and §6 is the answer to it.

**Unit-only PR CI gives all of that up to buy 2.7 minutes** (§2.5). That is the
trade, stated at its narrowest, and this document's position is that it is not
worth taking.

### 5.3 The owner's real-time rule removes exactly the flakes, and that is checkable

The determinism clause of §1 is measurable: which names on `src/redlist.rs` are
*standing* CI reds **and** in the fast tier — that is, which of them can red a
pull request for a reason that is not the author's diff.

Asked with `cargo run -- --known-red` for every name in both sets, there are
three, plus two disputed:

| name | finding | shape |
|---|---|---|
| `screen_blocked_dump` | `FIRES 1 of 2` on CI (PR #33 run `31472702284`) | the verdict is a decoded panel after a keystroke — the keystroke crosses host wall clock |
| `usb_disk_index_stable` | `FIRES 1 of 5` on CI (probe-rate run `31258202923`) | "nothing enumerated on the first controller" — enumeration timing |
| `xhci_hotplug` | `SEEN`, red again alone (run `31247206462`) | `device_add`/`device_del` against a 100 ms debounce |
| `std_unwind`, `std_unwind_so` | `DISPUTED` | `specs/ci-plan.md` §9.3 says closed by `wt/toyos-fpu`; the cited write-up says not re-measured on CI |

**All three of the standing ones are timer-anchored, and every one of them would
leave the fast tier under the owner's 2026-08-12 rule.** That is not a
coincidence to be noted and moved past — it is the rule's justification, measured
against the tree's own flake set rather than argued from first principles. The
rule is what makes §1's second clause reachable.

Against that: 27 of the 37 real names in `src/redlist.rs` are already outside the
fast tier. Relegation did not fix a single one of them; it moved them off the
merge button. §6 is what stops that being a way of not knowing.

### 5.4 Rejected alternatives, priced

Each priced on four axes: (a) PR latency, (b) time-to-detection per class,
(c) bisection cost when the deferred tier reds, (d) flake exposure of the merge
button.

`main` takes **5–17 merged pull requests a day** (`git log origin/main
--first-parent --merges`, 2026-08-03 to 2026-08-12; 13, 14, 5, 5 on the four full
days since landing moved to GitHub). That is the bisection denominator throughout.

#### (i) Unit-only PR CI — the owner's clean-slate option

| | |
|---|---|
| **(a) latency** | 381 s, bounded by `host` on macOS. **Saves 161 s against today, 367 s against the full tier.** |
| **(b) detection** | Every class in §3.1 assigned to KVM CI moves from "this pull request" to "the next nightly". The AMD `SYSRET` class in particular: nothing else in the tree can execute it — the dev host is cross-arch TCG and metal is not an instrument. |
| **(c) bisection** | A nightly red bisects across 5–17 merges. Four confirming runs at ~590 s each is ~40 min of CI per red, plus a human, and the historical toolchain has to still be published for each probe point. |
| **(d) flake** | Best possible — zero guest flakes on the button. |
| **verdict** | **Rejected.** It buys 2.7 minutes and gives up the only instrument for four defect classes, two of which are kernel-isolation defects that let one process kill another. §5.2 is the price of what it discards: 48 ms. |

#### (ii) The current shape — the full guest suite per PR

| | |
|---|---|
| **(a) latency** | 748 s slowest job, 786 s run wall (run `31472065811`). **+367 s over unit-only.** 134 runner-minutes a run, which is the queue pressure §2.4 measured. |
| **(b) detection** | Best possible for every automated class. |
| **(c) bisection** | Best possible — one diff. |
| **(d) flake** | Worst. All 37 real names in `src/redlist.rs` on the button, six of them firing at 20–40% in the rate probe. `specs/ci-plan.md` §9.2's own verdict: "one run in five is far above the one in fifty that bar treats as tolerable." |
| **verdict** | **Rejected on (d), not on (a).** The latency is affordable; the flake rate is what makes the merge button a coin. |

#### (iii) Compute-bound guest core — the proposal

| | |
|---|---|
| **(a) latency** | 542 s today; the swept version is lower, because every name §5.3 removes is one of the expensive ones. **+161 s over unit-only, −206 s against the full tier.** |
| **(b) detection** | Three of KVM CI's four classes gated per PR at ~7 s of test time. The fourth (task start skew) and the device-lifecycle family move to nightly. |
| **(c) bisection** | One diff for the compute-bound classes; 5–17 merges for the rest. |
| **(d) flake** | The three standing fast-tier flakes all leave under the rule. |
| **verdict** | **Proposed.** |

#### (iv) Minimal smoke boot — one or two shards

The tempting middle, and **the measurement kills it**. A guest shard's fixed
cost is 320–340 s (§2.2) and the `host` floor is 381 s on the same head, so the
first ~45 s of guest harness time is free and everything past it is paid second
for second on the *critical-path shard only*. The fast tier's largest indivisible unit is the
shared boot at 166.7 s of harness, so:

| shape | critical-path job | Δ vs unit-only | guest tests gated |
|---|---:|---:|---:|
| unit-only | 381 s | — | 0 |
| shared block alone, 1 shard | **≤ 504 s**, measured rather than modelled: run `31601279765`'s shard 2 is that job plus nine own-boot tests, 192 labels for 504 s | ≤ +123 s | 176 |
| compute-bound core, 12 shards | 542 s | +161 s | 246 |
| full tier, 12 shards | 748 s | +367 s | 297 |

**A smoke gate saves at most 38 seconds against gating everything compute-bound,
and gives up every own-boot machine shape to do it.** Sharding means the marginal
wall cost of guest test *count* is nearly zero; what costs is having any guest
job at all, and then only the longest shard. **Rejected — it is the worst point on the curve.**

#### (v) Merge queue or batch gating

**Closed by an entitlement, and measured rather than read off a page.**
`specs/ci-plan.md` §10.1: `gh api -X POST .../rulesets` with a `merge_queue` rule
answers `Validation Failed … Invalid rule 'merge_queue'`, and the GraphQL
`mergeQueue(branch:"main")` is `null`, while a control ruleset carrying
`non_fast_forward` was accepted on the same repository seconds earlier. The cause
is that `Japabu/toyos` is owned by a User account, the same cause as that
document's §9.4 larger runners.

**It would change the answer if it were available**, and the spec should say so
rather than pretend otherwise: a queue builds base + entries ahead + this pull
request and merges only what it tested, which is strictly better than
`specs/ci-plan.md` §10.2's strict-required-checks arrangement — it keeps the gate-the-merged-result property
*and* removes the serialisation that makes every open pull request re-merge and
re-run after each landing. At 13–14 merges a day that serialisation is most of
the queue §2.4 measured.

**Owner decision point (D2): if this repository moves under an organization, the
merge queue is the first thing to turn on, and it is worth more than any tier
change in this document.**

#### (vi) Run the QEMU suite locally as the gate

**Already refused by a decision the owner made**, and the reasons are three:

- **It cannot execute the class.** `specs/ci-plan.md` §10: "The dev host is arm64
  emulating x86 and gives you one vendor's reading of every instruction it
  emulates… A gate that cannot execute a class of defect is not a gate against
  it." `cargo run -- --land` is retired for exactly this.
- **It is not available when it is needed.** The shared sysroot claim refuses
  every other worktree for the length of an ABI change — measured at 35 and 50
  minutes of nobody being able to build, and observed taking the local guest
  suite away entirely (`specs/ci-plan.md` §8.1).
- **Its verdicts expire on the host's clock.** `specs/issues/build/the-gate-is-a-full-suite.md`
  has three shapes of it — a boot timeout, a host-staged window the guest slid
  past, and a staged image that was not there — and
  `specs/issues/build/landing-test-reds-under-a-concurrent-landing.md` is the
  landing-storm form.

**Rejected as a gate; §7 keeps it as the contention instrument, which is the one
thing it owns.**

---

## 6. The nightly contract

**The nightly tier exists and nothing runs it.** The entry is
`nightly-tier-has-no-workflow` in `specs/issues/build/` — **which does not exist
on `main` yet**: it arrives with `wt/toyos-slowtests`, and this document is
written against that branch's `src/tiers.rs` throughout. Opened 2026-08-11, still
`status: open`:
`cargo test --test toyos-build -- --nightly` runs the relegated tests manually,
nothing in `.github/workflows/ci.yml` schedules that command, and so **60 tests
and 4,083.8 s of effective CI test time are gated by nobody at all**, and
twenty-seven of the thirty-seven names in `src/redlist.rs` sit outside the fast
tier with them (§5.3).

That is the single largest hole this document has to close, and it is larger than
the question the owner asked. **Moving more tests to nightly while nightly has no
reader is not a tier; it is deletion with a note attached.**

So the contract, proposed in full, because a tier without one is what the tree
has now.

### 6.1 What runs

| | |
|---|---|
| **the relegated set** | `--nightly`, the exact set `src/tiers.rs`'s `RELEGATED` names, on the same twelve-shard `debian:sid`/KVM configuration the fast tier runs — so a nightly red and a fast red are the same instrument and comparable |
| **the emulated breadth** | more than the one-test `tcg` canary: a whole shard under TCG, which is the arm `specs/ci-plan.md` §7.2 named as still available and never taken. **This is what would give the `tcg` job's class a real reader** (§3.3, D1) |
| **gate A, thorough** | `gate-a.yml` at N=30, two jobs of about half an hour, dispatched against the ref. It is already in the shards' container as of `specs/ci-plan.md` §12.1, so it is the same instrument too |
| **stress** | whatever the owner wants that a ten-second ceiling forbids by construction |

### 6.2 When

Nightly, on a schedule, plus `workflow_dispatch`. **Not on `main`'s push** — at
5–17 merges a day that is 5–17 hour-long runs, and the whole point of this tier
is that it is on nobody's critical path.

### 6.3 Who reads it, and what a red obligates

**This is the clause that makes the difference between a tier and a bin.** A
nightly job nobody reads is worse than no nightly job, because it converts "we do
not gate this" into "we gate this" without changing anything.

Proposed, and this is the shape the tree already has for exactly this problem:

- **A nightly red files or updates a `src/redlist.rs` row.** That file's whole
  design is for this: a row is one measurement with an instrument, a rate, a run
  id and the day it was taken, and `SHELF_LIFE_DAYS = 31` reds a month later so
  that a row nobody re-measures is deleted rather than believed. A nightly tier
  whose output is redlist rows is a tier with a reader by construction.
- **A nightly red does not block a merge, and does not become an
  `EXPECTED_FAILURES` entry either.** `specs/ci-plan.md` §10.4 already refuses
  that trade by name: an exemption names a defect and its write-up, and "fires
  40% of the time for reasons nobody has looked at" is not one.
- **A nightly green is not evidence a relegated test is fixed.** One green of an
  intermittent is one sample; `tests/CLAUDE.md`'s `Stale::OnAPass` rule and
  `src/redlist.rs`'s `Finding::quiet(of)` — which refuses `of < 2` at compile
  time — both already say so.
- **The nightly job merges its own durations into the same profile.** The issue
  says this and it is load-bearing: without it the withheld labels stay frozen at
  the pre-split baseline and `tiers::validate_ci_profile` cannot notice a Nightly
  test's CI cost changing. A frozen number is what lets a test that became cheap
  sit in nightly forever.

### 6.4 The escape hatch that makes the tier temporary

`src/tiers.rs` already says the interim "is a loss" and that #188 holds the
optimisation work. Two mechanical properties should be added to the contract so
that "interim" is enforced rather than intended:

- **`Why::Cost` already closes downward** — `validate_ci_profile` reds a Cost row
  whose current labels are all at or under the ceiling, with "belongs Fast". So a
  test that gets optimised comes back automatically **as soon as the nightly job
  measures it**, which is the second reason §6.3's last bullet is not optional.
- **A `Why::RealTime` variant.** Under the 2026-08-12 rule, a test relegated for
  being timer-anchored must *not* come back merely because it got fast. `Why`
  today has only `Cost` and `RidesTheBootOf`, so a swept `audio_tone` is recorded
  as costing too much rather than as being anchored to a clock, and the
  downward-closing gate would drag it back the day it measures 9 s.
  **Owner decision point (D3).**

---

## 7. The local suite's role

**Developer feedback, and the contention instrument. Never a gate.** §5.4(vi) is
why it cannot be a gate; this is what it is for, and both halves are load-bearing.

- **Feedback.** It is the fastest instrument in the tree by a wide margin — 246
  fast-tier tests in one process on fourteen cores against twelve machines that
  each pay 330 s of setup and build first. An agent iterating on a kernel change
  should run it constantly and should not wait for CI to learn that a boot
  panics.
- **The contention instrument, and it is the *only* one.** §3.1's fifth and
  sixth rows: `HostSlots`, `buildlock::guest_slot`, `qemu::budget`'s width
  multiplier, the whole `ALONE: GREEN` classification and the shootdown deadlock
  are untestable on a runner by construction. **A CI green is not coverage of a
  contention bug**, and reading it as one is a mistake root `CLAUDE.md` already
  warns about.
- **What it may never do is certify.** Its verdicts expire on the host's clock
  and it is cross-arch TCG, so `ALONE: red again` on a loaded host still means
  nothing without an A/B against `main` in the same session, and nothing it says
  is evidence about which vendor executes an instruction.

**One consequence worth stating as policy: the local suite should run the fast
tier by default and the nightly tier on request.** A developer's feedback loop
and the per-PR gate wanting the same set is a coincidence worth keeping, because
it is what makes "it was green locally" and "it was green on CI" comparable
sentences.

---

## 8. Open questions for the owner

| # | question | why it is his |
|---|---|---|
| **D1** | **What is the `tcg` job for?** It has never named a tree defect (§3.3), it is the most expensive required check at 436 s, and it also writes the cache the twelve shards read. Three options: keep it as an anti-rot declaration and say so in its own comment; promote its class by running a whole TCG shard nightly (§6.1); or drop it as a *required* check and keep the job for the cache, which makes the required set four. | It is his rule that put it there — "everything should work under emulation and kvm". |
| **D2** | **Move the repository under an organization?** It buys the merge queue (§5.4(v)) and larger runners, both of which are worth more than any tier change here, and both are refused today by the User-account entitlement. | A repository-ownership decision. |
| **D3** | **Should `Why` gain a `RealTime` variant** so a timer-anchored test cannot be dragged back into the fast tier by getting cheap (§6.4)? | It encodes his 2026-08-12 ruling in the type rather than in a comment. |
| **D4** | **Is the nightly workflow this document's next task, or a separate one?** `nightly-tier-has-no-workflow` in `specs/issues/build/` is open and unassigned, and until it is built the tier is a bin (§6). This document proposes the contract; somebody has to build it. | It is a scheduling call about what gets an agent next. |
| **D5** | **Sweep the real-time rule across the rest of the fast tier now, or wait?** `src/tiers.rs` records the sweep as pending. §5.3 says it removes exactly the three standing flakes from the merge button; it will also relegate more tests, and every one of them lands in a tier with no reader until D4 is answered. **The two are coupled and the order matters.** | A coverage trade. |
| **D6** | **Does the class in §3.4 get an owner?** `control-regs-bench` exists and has never been run on silicon, and no automated gate can produce or refute that class. The answer is either "metal, on a cadence somebody owns" or "nothing, declared". | Metal time is his. |

### What this document deliberately does not propose

- **No change to any assertion, machine shape or negative gate.** §5's proposal
  moves names between tiers; it deletes nothing. `specs/test-cost-audit.md`'s
  standing position (§3.7) — that selective running trades confidence for speed
  and the audit's answer is no — is unchanged, and this document agrees with it:
  the argument here is that the *guest gate stays*, not that it shrinks.
- **No latency work.** §2.2 shows the largest single item on the critical path is
  230 s of build in every one of thirteen jobs, with a filed cause
  (`specs/issues/build/external-dep-fingerprint-is-mtime-not-content.md`) and a
  measured cache underneath it (`specs/ci-plan.md` §12.5). **That is worth more minutes than every
  tier decision in this document put together, and it costs no coverage at all.**
  It belongs to whoever owns #188 and the build system, not here.
