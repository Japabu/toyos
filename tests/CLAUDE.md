# Tests

Loads when you read a file under `tests/`. Root `CLAUDE.md` has the commands, the TCG caveat and the CI-versus-this-host rule.

## The harness

- The harness has more machine shapes than the CLI — read `tests/common/` for the list. **Ground truth is the host side of the device**, not what the guest says it did. **Where QEMU cannot stage a failure, an actuator is how the harness reaches it**, and each carries a comment saying why nothing else can — a `SYS_DEBUG` arm in `arch/syscall.rs` where a test can ask after boot, a boot parameter where a boot has already acted by then. One that merely re-states what the code does is not an actuator, and **one that replaces only a verdict makes its own gate vacuous** — replace what a real failure leaves in the buffer too, or the caller still holds bytes it would never have had. Device *order* is a shape dimension as much as size: a pool block a failed bind gave back is visible only when the refused device is enumerated first. **A test that writes to a disk decides which disk from data on the disk, never from the flag that enabled it** — the boot stick shares the bus.

- **A full run builds two kernels: the one an image ships and the one carrying every actuator.** An actuator is a *boot parameter*, `BootOptions::kernel_params`, named in `kernel/src/actuator.rs`, and never a build; **a bound that moves is armed at runtime**, never `#[cfg]`d, because an inertness claim expires the day a feature grows a second site. `kernel_features` is a build and one outside `DECLARED_KERNEL_BUILDS` is refused. `SYS_DEBUG` exists only under `test-actuators`, nothing folds the name into a boot that did not ask, and `ACTUATOR_TESTS` is the shared-boot list that gets it, its own doc comment saying which binaries and why. Gates: `suite_split` and `assert_actuators_match_features`, plus three under `cargo test --lib`. `specs/assessments/test-cost-audit.md` §5.9, §5.9.7.

- **The suite runs twelve guests at once, and a test is serial until it says otherwise.** Every boot owns its image, socket and scratch files; every worker owns a lane of the pid's temp dir (`tests/common/lane.rs`). Each `MACHINE_TESTS`/`SCREEN_TESTS` entry carries a `Sched` and does not compile without one — parallel phase, then a serial tail of the tests whose *verdict* is a wall-clock margin, then **gate A alone**, which asserts the live-guest count is zero rather than trusting its position. `cargo test --test toyos-build -- --jobs N` overrides the width (a bare `cargo test -- --jobs N` hands it to the `--lib` harness, which refuses it); 12 is a request rather than a promise, because `buildlock::guest_slot` bounds the machine at twelve guests across every worktree. **A timeout a test hands `run_test` is a liveness guard and never a verdict, so `qemu::budget` pays it out per guest the phase may have up** — `drain_serial` is the one duration that does not scale, because callers pace with it. **Every red from the wide phase is re-run alone**: green there names the classification as the bug and the run stays red on it. `group_of`'s adjacent runs are one task on one worker, and a shared boot's console is drained *by* its members. Order is longest-job-first on the last run's measurements. The harness builds at opt-level 2, and a boot's kernel, bootloader and initrd are memoized per run on what each is a function of — so a source edit mid-run is not picked up, because a run is a measurement of one tree. Costs, the three ledgers and the width table: `specs/assessments/test-cost-audit.md` §5.1–§5.4.

Every profile declares its device size or does not compile, and the images are sparse so a realistic one is nearly free — ask any device for its real capacity, `specs/device-test-strategy.md`.

## The two tiers

**`cargo test` runs Fast and names what it withheld.** The CI ceiling is ≤10 s per execution. `RELEGATED` records 54 slow tests plus six shared-boot riders; `--nightly` runs all 60. Registrations carry `Tier`, close shared groups, and refuse missing evidence. Bootstrap new names exactly as `specs/assessments/test-cost-audit.md` §7.2 says. `guest-suite` requires the merged-profile gate. Scheduled Nightly CI is still open; #188 only optimises tests back into Fast.

## Gate A (audio)

Two tiers. `tests/audio-baseline.toml` documents both in full and justifies every number in it.

- **Fast** — part of every `cargo test`. One boot per config. Certifies that the instrument is alive and that this build does not *reproducibly* put silence on the wire. **The verdict is harm** — a mid-tone gap in the capture, or a period soundd submitted with no client audio behind it — and it re-boots once to confirm before failing. The per-run ceilings are printed and fail nothing: a drain that recovered is not audio anyone heard, and one run is one sample, so nothing here certifies a rate.

- **Thorough** — `cargo test --test toyos-build -- --audio-gate N`. N iterations of all four configs; every per-run outcome becomes a rate or a distribution, compared against the recorded sample by Mann-Whitney (counters) and Fisher exact (yes/no). **A scheduler-migration stage transition gates on this tier.** At N=30 it detects a shift in wake lateness or a drop in soundd's wake count; it does *not* detect a doubling of the dropout rate, and no N a human waits for would.

`specs/assessments/audio-gate-history.md` has the gate's own defects and what they hid. The lesson: these counters drift between batches on one host with no code change, so only same-session A/B numbers mean anything.

**The thorough tier is currently red on `main` itself** (`specs/issues/audio/`), so it cannot serve as a pass/fail gate until that is settled; it still answers an A/B. The fast tier is green.

## Expected failures

The rule is in the root file; the mechanics are here. `EXPECTED_FAILURES` names the write-up in `specs/issues/`, and the run prints that pointer beside every `XFAIL` and writes its not-clean last line into every CI shard's job summary. An entry declares what makes it stale as `OnAPass` where the failure is reproducible, or a review date where it is not — one green of an intermittent test is one sample and may not red a healthy tree.

## Probing a refusal

**A guest binary cannot ask what a handle it does not hold does.** `BadHandle`, `Stale` and `WrongType` end the caller with exit 139, so an arm probing one is a **child** — one fault per child, the parent asserting the death. Give each child a `println!` marker before the call and require it: without one, a child that died on the way to the thing under test passes while asserting nothing. `handle_kill_policy` is the matrix and the pattern.

**A test binary holds what `test-runner` holds** (`specs/capability-endowment-spec.md` §4) — the 90 guest binaries are not `[programs]` keys, so no manifest row can name what any of them needs. Its namespace travels to every child by inheritance and its `SysCap` is endowed explicitly at `SYSCAP_LABEL`. A test that needs a *server* builds one: a port, a namespace over its connector, and a child spawned holding it, all inside one binary — `endowment_denied`.

## Waiting for a program's line

**`/bin/init` speaks in every program's name before that program runs** — `init: netd: no nic on this machine` is in the boot capture before netd exists — so a predicate keyed on a `<program>: ` prefix is satisfied by the wrong speaker. Wait for the whole line, in the constant the assertion also reads. Invisible here and red on CI, because on TCG the daemon's own line arrives first anyway. Anything hostile to init goes on `tests/netcase`, the one config whose test-runner is endowed a `launcher` connector — `launcher_refusals`.

## Caveats that bite every agent

- **The dev host is a laptop that sleeps mid-session, and the suite says so.** A run whose wall clock jumped against the monotonic one reports `INVL` per test and exits 2 — re-run rather than investigate. A wild outlier *not* marked that way is a real finding.
- **A landing-gate red on a test that is green alone is not therefore the host.** The class is a verdict that expires on the host's clock: it cannot report anything else, so every defect underneath arrives wearing the same sentence. A wait on the guest is bounded by the guest, a retype loop is a count, an injection is paced; what a duration still decides prints `STALL` — red, and named apart so nobody bisects it. `ALONE: red again` on a loaded host means nothing without a same-session A/B against `main`. None of this re-runs an audio harm verdict away.
- **The converse holds too**: a machine-wide kernel panic reds whichever test was running — that red's name is the workload, never the cause, even when the isolated re-run reds again.
- **Gate A's fast tier reds intermittently, on `main` as much as your branch.** Stash and re-run before believing it is yours: `specs/issues/audio/audio-tone-load-fast-tier-intermittent.md`.
- **A filtered C-test run can be red for a daemon's line rather than its own output** — `cargo test -- <name>` opens one capture window, soundd's boot lines land in it, and that family compares whole stdout. Judge it from a full run.
- **Every guest this host boots is TCG — one vendor's reading of the ISA.** Anything vendor-dependent is gated only by CI's KVM shards. TCG also prices instructions unlike hardware: an uncontended atomic read-modify-write on a hot path is the one that bites.
- **Host suites** run with plain `cargo test` inside `toyos-sched/`, `toyos-ps2/`, `toyos-gpt/`, `toyos-elf/`, `toyos-cc/`, `toyos-ld/`, `toyos-hda/`, `toyos-pci/`, `toyos-xhci/`, `toyos-desktop/`, `kernel-loom/`, `kernel-span/`, `toyos-fat32/`, `toyos-fat32-check/`, `toyos-abi/`, `toyos-manifest/`; `userland/sshd` cross-compiles and needs the host triple (`cargo test --target "$(rustc -vV | sed -n 's/^host: //p')"`). `kernel-loom` is the only memory-ordering check in the tree — x86 TSO hides a missing acquire edge from every guest test.
