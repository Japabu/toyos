//! Which tests every `cargo test` runs, and which ones it does not — as data,
//! with what each relegation costs the tree written beside it.
//!
//! **The owner's line, 2026-08-11: the fast per-PR path runs tests taking ten
//! seconds or less.** Everything above it moves to a nightly tier, scheduled in
//! `.github/workflows/ci.yml` at `03:00 UTC` and reachable on demand through
//! `--nightly` or a `workflow_dispatch`. This module is that decision as data:
//! [`RELEGATED`] is exactly the set the nightly job selects, and `tests/toyos.rs`
//! writes [`Tier::Nightly`] against each of those names in its own registration.
//!
//! **This is interim and it is a loss.** Fifty-three registered tests are
//! Nightly: twenty-four for [`Why::Cost`] — a CI price without margin, which
//! since 2026-08-21 means over [`FAST_COMMIT_MS`] rather than over the ceiling
//! itself — twenty-five for [`Why::TimerAnchored`], Nightly by classification
//! rather than by cost and mostly nowhere near the line — and four for
//! [`Why::RidesTheBootOf`], riding `metal_sim_compositor`'s shared boot.
//! Between them they carry 604.8 s of the 994.9 s the committed profile prices
//! across 327 labels, and none is
//! gated per pull request. `guards` on every row says what stopped being gated,
//! because a run that quietly does less is the whole failure mode here.
//!
//! **Nothing here is an optimisation and nothing here changes an assertion.**
//! A relegated test measures exactly what it measured; the manual nightly
//! command runs it. #188 holds only the optimisation work that would make one
//! of these fast enough to come back to the per-PR tier — and **two names left
//! by that door on 2026-08-17**: `xhci_msi_only` (35,223 ms) and
//! `swiss_german_layout` (12,645 ms) were each a guest binary waiting out a
//! fixed fallback deadline nobody had sent the sentinel for, 30 s and 8 s of
//! host wall clock with no assertion behind either. Both are `Tier::Fast`
//! again, on run 32023797195's twelve shards rather than on the dev host:
//! **5,857 ms and 5,441 ms**.
//!
//! **The fast tier demands margin, 2026-08-21.** [`FAST_COMMIT_MS`] is the
//! price a test may be *committed* at, four fifths of the ceiling, and it is
//! what both directions of the tier rule are decided against: a Fast name may
//! not be priced in the band below the ceiling, and a [`Why::Cost`] row returns
//! only at or under it. Before it, one sample decided both directions, and on
//! one afternoon three straddlers bounced every merge-queue entry in turn. Read
//! [`FAST_COMMIT_MS`] for the derivation and the three names.
//!
//! **CI is the instrument for a per-PR policy.** The effective profile starts
//! with the last full twelve-shard run and replaces every name measured by the
//! first fast-tier run. That retains a price for withheld tests while using the
//! freshest CI price for everything the fast tier did execute. Dev-host TCG
//! timings remain useful optimisation evidence, but they do not decide which
//! side of a KVM CI cutoff a test belongs on.
//!
//! **The nightly renders the verdict; a landing renders only its own names,
//! 2026-08-22.** The rule here is unchanged and so is the ceiling — what moved
//! is which run's red stops a landing. A pull request's and a merge-queue
//! composition's `durations` job refuses a price verdict only for the names
//! that change registered or re-tiered, and prints every other one as a
//! `::warning::`; the nightly's twelve hosted shards pass no base and refuse
//! them all, and a nightly red is fixed by a pull request the next day like
//! every other nightly red. The reason is a measurement, not a preference: over
//! six hosted twelve-shard runs a per-shard common price factor explains 57% of
//! a name's run-to-run variance and spreads 1.28x p10–p90
//! (`issues/build/a-shards-boot-width-does-not-price-its-tests.md`), so a name
//! priced anywhere near a line reads over it on some runs by shard luck alone —
//! and under the required merge queue that red dequeues the whole composition,
//! every pull request behind it included. [`ci_profile_verdicts`] is the shape
//! that filtering needs: one verdict per name, each saying whether it is about a
//! price. A verdict about the *declaration* — a marker on a Nightly row, a
//! duplicate row, an empty `guards`, a rider on a carrier that is not Nightly —
//! is true whoever measured it and is refused on every run.

use std::collections::{BTreeMap, BTreeSet};

/// The ceiling the fast tier is defined by, in milliseconds.
///
/// Policy, and the owner's. A test at exactly the line is fast: the rule he
/// stated is "ten seconds or less".
///
/// **2026-08-12: the line is hard, and there is deliberately no margin or
/// hysteresis band** — a measured crossing reds `durations`, however close.
/// That still holds, and [`FAST_COMMIT_MS`] is not a softening of it: the
/// ceiling refuses the same crossings it always did, and the commitment line
/// below it refuses ever being *near* one.
/// Same date, the tier boundary: a test whose verdict or duration is anchored
/// to real time — it plays or records in real time, waits out a staged latency
/// window, or measures a rate, such that a 2x slower machine would change its
/// verdict or price — belongs Nightly; only a compute-bound verdict stays Fast.
/// **2026-08-13: the sweep applying this to the rest of the fast tier landed**
/// — [`Why::TimerAnchored`] is the classification it needed, and every
/// borderline name the cost audit raised has one of the three `Why` rows now.
pub const FAST_CEILING_MS: u64 = 10_000;

/// The price a test may be **committed** at and still be [`Tier::Fast`], in
/// milliseconds: four fifths of [`FAST_CEILING_MS`].
///
/// **The fast tier demands margin — the owner's decision, 2026-08-21.**
/// [`FAST_CEILING_MS`] alone was a one-sample rule pointing both ways: the gate
/// reds a Fast name measured over the ceiling, and invites a [`Why::Cost`] row
/// back to Fast the moment one measurement lands under it. A test whose price
/// sits within a few percent of the line therefore flips per partition, and the
/// red lands on whichever pull request measured it next — an author whose diff
/// has nothing to do with it.
///
/// Three of them bounced merge-queue entries on 2026-08-21 alone.
/// `i8042_absent`: committed 9,221 ms, measured 10,738 ms in runs 32475363422
/// and 32476143292 — **16% over its commitment**. `i8042_health`: committed
/// 9,509 ms, measured 10,281 ms in run 32506320411 — **8% over**.
/// `xhci_full_speed_device`: committed 6,900 ms, measured 10,166 ms in run
/// 32513441183 — **47% over**, and the one of the three that had margin. Two of
/// the three had been returned to Fast that same day on one calm nightly sample
/// each.
///
/// **A fifth is the width the evidence asks for, and it is not a curve fit.**
/// A band that just cleared the observed 8% and 16% straddles would be fitted
/// to them; a fifth is chosen because of what it makes the *ceiling's* red
/// mean. Committed at or under 8,000 ms, a name measured over 10,000 ms has
/// grown by at least a quarter over the price it was committed at — a finding
/// about that test, not a coin landing. `xhci_full_speed_device`'s 47% is
/// exactly that finding, and it is why margin does not make it a straddler.
///
/// Both directions are this line's, and both live in [`ci_profile_verdicts`]:
/// a Fast name may not be priced in `(FAST_COMMIT_MS, FAST_CEILING_MS]`, and a
/// [`Why::Cost`] row returns to Fast only at or under it. **A straddler cannot
/// be Fast.**
///
/// **Margin is not enough by itself, and 2026-08-22 measured why.** A fifth of
/// room does not make a *variable* name safe: `xhci_full_speed_device` is
/// committed at 6,900 ms with 31% of margin and was priced 4,700, 6,816, 6,900,
/// 7,456, 7,499 and 9,890 ms over six hosted twelve-shard runs — the 9th most
/// variable of 83 Fast names, and the last of those six reded merge-queue
/// composition 32550410305 in the band. No tier holds such a name under a rule
/// that reads one sample per run, so the answer is not a wider band but a
/// narrower *audience*: a landing renders the price verdict only for the names
/// it touched, and the nightly renders it for all of them. The module header
/// carries that rule and the measurement behind it.
pub const FAST_COMMIT_MS: u64 = FAST_CEILING_MS * 4 / 5;

/// A committed profile row that exists only to put a new registration into one
/// KVM measurement run. `--merge-durations` always refuses a committed marker
/// after writing the measured artifact, so it cannot be evidence on a merge
/// head. Zero is not usable for this: several real in-guest verdicts measure
/// below the profile's millisecond resolution.
pub const UNMEASURED_MS: u64 = u64::MAX;

/// Which run a registered test belongs to. Every entry of `MACHINE_TESTS`,
/// `SCREEN_TESTS`, and `AUDIO_TESTS` answers this or does not compile, for the
/// same reason each machine-owned test answers `Sched`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Every `cargo test`.
    Fast,
    /// `cargo test --test toyos-build -- --nightly`, run every night by
    /// `.github/workflows/ci.yml`'s `03:00 UTC` schedule and on demand through
    /// the same flag or a `workflow_dispatch`.
    Nightly,
}

/// Why a name is not in the fast tier. The three are not interchangeable and
/// the gates below check different things of each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// **Priced without margin by itself** — over [`FAST_COMMIT_MS`], whether
    /// or not it is also over [`FAST_CEILING_MS`]. A row in the band between
    /// the two is a straddler: cheap enough that a quiet run prices it under
    /// the ceiling and expensive enough that a loaded one does not, which is
    /// the state this variant exists to hold rather than to oscillate through.
    Cost,
    /// **Under the ceiling, and relegated anyway** because it shares one boot
    /// with the named test that is over it. `group_of` in `tests/toyos.rs` makes
    /// a run of adjacent names one guest, so the group's cost is the group's and
    /// a member cannot be moved out of it — keeping a cheap rider in the fast
    /// tier would put the whole boot back in it.
    ///
    /// This is the collateral the record has to name: the riders here go dark
    /// with the slow carrier boot they share.
    RidesTheBootOf(&'static str),
    /// **Nightly by classification, not by cost** — its verdict or duration is
    /// anchored to real time (`FAST_CEILING_MS`'s 2026-08-12 boundary): it plays
    /// or records in real time, waits out a staged latency window, or measures a
    /// rate, such that a 2x slower machine would change its verdict or price.
    /// No ceiling requirement in either direction: a row's label may measure
    /// anything at all, over the line or nowhere near it, and neither moves it —
    /// only reclassifying the verdict itself would.
    TimerAnchored,
}

/// One test that the fast tier does not run.
pub struct Relegated {
    pub test: &'static str,
    /// The last measurement recorded for this row, in milliseconds — for
    /// `audio_tone_load`, which registers one test but emits an `(smp=1)` and
    /// an `(smp=8)` label, the sum of both as last recorded. Documentation,
    /// not a fixture: [`ci_profile_verdicts`] checks a fresh profile's tier
    /// *placement*, never this field against it, so a nightly run refreshing
    /// every Nightly label does not have to reproduce this number. A human
    /// updates it by hand when a "returns to Fast" or "belongs Nightly"
    /// finding lands a tier correction. Tier movement is by measurement in both
    /// directions, and a nightly run's measured profile is what refreshes these
    /// numbers — validated against the tier rule, never against equality with a
    /// past measurement.
    pub ci_ms: u64,
    pub why: Why,
    /// **What stops being gated per pull request.** Not what the test does —
    /// what the tree loses while this sits in the nightly tier. The owner reads
    /// this list to decide whether the interim is acceptable, so a row that
    /// restates the test's name is a row that answers him with nothing.
    pub guards: &'static str,
}

/// Every test the fast tier does not run.
pub const RELEGATED: &[Relegated] = &[
    Relegated {
        test: "klogd_hosted",
        ci_ms: 11_805,
        why: Why::Cost,
        guards: "The kernel-thread machinery, now for all three of them: klogd, usbd \
                 and iod spawn with process-table rows, `ps` and the census name them, \
                 and a deliberate panic takes the row's own branch instead of being \
                 decided by a stale `syscall_rip` — a verdict that depended on which \
                 syscall ran last. Both branches, which is the only way two rows are \
                 two rows: klogd's panic halts the machine, usbd's kills the thread and \
                 the machine boots. Three boots, and the two actuator arms \
                 (`klogd-panic`, `usbd-panic`) are the cost; the spawn half alone is \
                 one cheap boot, and \
                 issues/build/klogd-hosted-pays-two-boots-for-one-fast-verdict.md \
                 is the split that puts it back in the fast tier. What still runs per \
                 pull request: every boot's console output is klogd's drain, so the \
                 thread starving or dying is visible in any test that reads a line, and \
                 `blocked_dump` names all three in the fast tier.",
    },
    Relegated {
        test: "desktop_window_child",
        ci_ms: 65_217,
        why: Why::Cost,
        guards: "A window opened from a shell and then closed, and whether the desktop \
                 answers afterwards. The only reproduction #156 has anywhere. Its \
                 EXPECTED_FAILURES entry is Stale::OnThisDate, so the declaration still \
                 expires on 2026-09-06 whether or not the test has run since.",
    },
    Relegated {
        test: "fpu_isolation",
        ci_ms: 11_075,
        why: Why::Cost,
        guards: "The whole user machine state surviving every exit from Ring 3, on a \
                 one-CPU machine, against a second boot of an `fpu-save-nothing` kernel \
                 that must fail the same three arms: a leaked FP register value entering \
                 the next process, a masked x87 exception surviving a switch, and \
                 bit-identity across 20,000 syscalls, two page faults and a preemption \
                 spin. A negative gate: without the second boot the first proves only \
                 that the machine works, which it did before the gate existed. What still \
                 runs per pull request: the compute-bound `fault_gates`/`std_unwind`/ \
                 `std_unwind_so` trio, ~51 ms riding an \
                 existing shared boot, still catches a pending x87 \
                 control word killing the next process — the one shape that put this \
                 defect on CI in the first place — but proves nothing about a leaked \
                 register value, sustained preservation under scheduling churn, or \
                 whether an assertion has any teeth at all: the trio carries no negative \
                 control.",
    },
    Relegated {
        test: "desktop_audio_client",
        ci_ms: 121_441,
        why: Why::Cost,
        guards: "An audio client spawned by a shell inside a terminal inside the \
                 compositor — the only configuration in which all three of its \
                 descriptors are pipes to a surface, which is the T14's. A second client \
                 connecting while the first streams, and the desktop still answering \
                 after both.",
    },
    Relegated {
        test: "wall_clock_refusals",
        ci_ms: 103_987,
        why: Why::Cost,
        guards: "Five boots gate the RTC update flag never clearing, four reads never \
                 agreeing, absent-century fallback, explicit century-register decoding, \
                 and firmware timezone conversion.",
    },
    Relegated {
        test: "screen_fatal_halt_composited",
        ci_ms: 7_424,
        why: Why::TimerAnchored,
        guards: "metal-panic-probe fires once at framebuffer-claim + 5 s (kernel/src/heartbeat.rs); \
                 the test waits that staged window out and then the pager's cycling on top of \
                 it. Whether a fatal panic can paint the panel once a compositor owns the \
                 scanout is the T14's only configuration and the assumption three freeze \
                 investigations rested on; no other screen test asks it.",
    },
    Relegated {
        test: "metal_sim_compositor",
        ci_ms: 8_625,
        why: Why::TimerAnchored,
        guards: "Waits for three `compositor: frames=` batches at STATS_INTERVAL = 2 s, so \
                 ~6 s of its run is a guest reporting timer. The four daemons surviving the \
                 T14's device shape, each in its own words: the compositor naming the \
                 firmware framebuffer it claimed, netd exiting rather than panicking with no \
                 NIC, soundd staying up on a null sink, sshd saying it found no netd. \
                 Nothing supervises any of them, so the message is the entire diagnostic. \
                 First of a six-test shared boot, whose tier closes upward as one unit.",
    },
    Relegated {
        test: "metal_sim_compositor_stall",
        ci_ms: 11_639,
        why: Why::Cost,
        guards: "A client that stops talking, stops listening, or never stops. The guest \
                 asks whether the compositor still answers; the host asks whether it is \
                 still painting, which is the only way a livelock that answers everybody \
                 and draws nothing is visible.",
    },
    Relegated {
        test: "metal_sim_client_death",
        ci_ms: 3_908,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "Five client-death/refusal cases complete, a reaped creator's inherited \
                 connection still obtains a window, and the compositor produces two later \
                 frame batches with a clean console.",
    },
    Relegated {
        test: "metal_sim_window_caps",
        ci_ms: 161,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "That the window cap the compositor *derived* from total memory and the \
                 screen is the number of windows a client actually gets. A constant on \
                 both sides would agree with itself forever.",
    },
    Relegated {
        test: "metal_sim_ipc_hostile_peer",
        ci_ms: 112,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "A client that lies about its frame lengths, with the host insisting the \
                 case count the guest reports is the whole case list.",
    },
    Relegated {
        test: "metal_sim_scanout_wc",
        ci_ms: 0,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "The scanout's memory type from the IA32_PAT MSR through the MTRR \
                 combination to the compositor's installed PDE. The PDE is read back from \
                 the page table; the PAT contents are not recorded there.",
    },
    Relegated {
        test: "doom_music",
        ci_ms: 7_543,
        why: Why::TimerAnchored,
        guards: "Reads a device capture and requires at least 0.8 s of it to carry signal at \
                 peak >= 6000 — an absolute seconds-of-signal floor on audio recorded in real \
                 time. That doom opened the SoundFont this tree committed, played to the end \
                 of the check, and that what it rendered reached the device. The three links \
                 src/soundfont.rs's host tests cannot make, and the three `b8b0749` broke \
                 for a cycle with the suite green.",
    },
    Relegated {
        test: "doom_sound_flood",
        ci_ms: 5_714,
        why: Why::TimerAnchored,
        guards: "check_playback bounds tone and probe within [ceil(frames/128), 4x] of the \
                 device's own period clock, and the capture is read for active samples: a rate \
                 assertion on whether the guest kept up. It is the first domino of the T14 \
                 freeze: an `extern \"C\"` frame with no unwind path turned the overflow panic \
                 into abort, and the kernel and compositor followed it down.",
    },
    Relegated {
        test: "screen_console_scroll",
        ci_ms: 13_401,
        why: Why::Cost,
        guards: "Every row of the panel, character for character, after a workload built \
                 to leave stale glyphs behind a scroll. #90 was the owner seeing prior \
                 text survive in the middle of a cleared screen.",
    },
    Relegated {
        test: "desktop_typing_damage",
        ci_ms: 81_197,
        why: Why::Cost,
        guards: "Eight typed lines echo through the desktop, and the largest \
                 compositor-reported damage frame stays at or below 2% of the panel. That \
                 threshold lies between the measured 89% whole-window repaint and 0.46% \
                 clock update.",
    },
    // 2026-08-21: `idle_stack_guard` (52,822 ms) and `dump_nmi_probe` (24,625
    // ms) left this table — returned to Fast. Both rows had said the 2026-08-17
    // drain fix might carry them under the line and that the next nightly KVM
    // measurement would decide; nightly run 32444411794 measured 5,049 ms and
    // 6,284 ms, and the durations gate refused their Nightly declarations
    // against those labels ("belongs Fast"), which is this table's own return
    // rule firing. `git log` on this file carries their rows.
    Relegated {
        test: "metal_sim_pointer_churn",
        ci_ms: 260_607,
        why: Why::Cost,
        guards: "Eight plug-and-pull cycles of a pointer under a compositor holding the \
                 merged pointer's handle across all of them. The owner froze his desktop this \
                 way twice, on the fourth cycle's enumeration.",
    },
    Relegated {
        test: "toybox_cp_volume",
        ci_ms: 18_735,
        why: Why::Cost,
        guards: "The real /bin/cp against a FAT32 volume sized from what the volume says it \
                 has left, including the case where it fills.",
    },
    Relegated {
        test: "usb_boot_stick_pulled",
        ci_ms: 189_100,
        why: Why::Cost,
        guards: "device_del on the stick carrying /boot and /log while the desktop draws — \
                 #152's only instrument. The failure has no other witness: /log dies with \
                 the event, the machine has no serial port, it is not a panic, and \
                 Ctrl+Alt+D answers nothing.",
    },
    Relegated {
        test: "screen_pager_keys",
        ci_ms: 52_776,
        why: Why::Cost,
        guards: "Thirty paced PageDown presses each move the panic pager through i8042 \
                 after every CPU has stopped. Host decoder tests cannot establish that \
                 delivery path.",
    },
    Relegated {
        test: "xhci_deaf_registers",
        ci_ms: 103_332,
        why: Why::Cost,
        guards: "A controller and a port that stop answering. Five register spins had no \
                 deadline at all — port reset, halt, HCRST, CNR, R/S — which on the T14 is \
                 `Boot: peripherals ready` on the panel forever and nothing else.",
    },
    Relegated {
        test: "hda_client_stall",
        ci_ms: 16_901,
        why: Why::Cost,
        guards: "A client that stops producing mid-stream, on a cyclic HDA ring and on a \
                 virtio queue, which must answer differently — underruns against deferred. \
                 Asserting both is what refuses the two obvious wrong fixes.",
    },
    Relegated {
        test: "xhci_hid_break",
        ci_ms: 61_704,
        why: Why::Cost,
        guards: "A HID interrupt endpoint completing with a code the driver dropped where \
                 it read it, at the fourth completion and at the first. A Logitech mouse \
                 on the T14 went silent for the rest of the boot with every bind-time line \
                 reading perfectly.",
    },
    Relegated {
        test: "iommu_discovery",
        ci_ms: 17_594,
        why: Why::Cost,
        guards: "Four machines whose remapping units differ in exactly one advertised \
                 capability each, and whether the kernel's decode moves with them. A \
                 plausible constant satisfies any single-machine assertion.",
    },
    Relegated {
        test: "kernel_heartbeat",
        ci_ms: 6_794,
        why: Why::TimerAnchored,
        guards: "Serial: its verdict is a cadence — a fixed 3 s drain against a 250 ms \
                 heartbeat period demanding at least four whole beats and no sample gap \
                 over 1 s — and a guest sharing the host with eleven others reaches its \
                 idle loop late for reasons that are not the defect.",
    },
    Relegated {
        test: "metal_sim_window_drag",
        ci_ms: 8_240,
        why: Why::TimerAnchored,
        guards: "Injects pointer packets on 25-120 ms sleeps where each packet's effect must \
                 be on screen before the next is sent — a guest one batch behind aims at the \
                 content instead, which is a different verdict rather than a slower one. Each \
                 step depends on the prior painted position, so this is the end-to-end gate \
                 for input ordering and desktop geometry.",
    },
    Relegated {
        test: "usb_storage_shapes",
        ci_ms: 90_409,
        why: Why::Cost,
        guards: "A raw 4 KiB-sector USB disk completes read and write with host byte \
                 verification, while a 3 TB disk is refused because READ(10) cannot \
                 address its last block.",
    },
    Relegated {
        test: "usb_flush_optional",
        ci_ms: 89_705,
        why: Why::Cost,
        guards: "A device that rejects the optional flush command remains usable, while a \
                 real write failure still propagates. Treating every command error alike \
                 either loses compatible disks or hides failed writes. **The cost this \
                 relegation is about was cut about sevenfold when `/bin/logd` took the file** — \
                 `/bin/logd` ends on an error instead of retrying inside a budget, which is what \
                 turned 1,737 failing flushes over six seconds into the handful a single refusal \
                 costs — and `ci_ms` above is untouched \
                 because it is a CI measurement and the new figure is a dev-host one. A nightly \
                 KVM run is what may bring this name back to Fast.",
    },
    Relegated {
        test: "hda_tone",
        ci_ms: 9_220,
        why: Why::TimerAnchored,
        guards: "soundd drives a real HDA stream and the host analyses the captured wav for \
                 dropouts, gap histogram and phase breaks — recorded in real time. Host-side \
                 codec tests do not connect ring programming, interrupts, the daemon, and \
                 samples on the wire.",
    },
    Relegated {
        test: "i8042_health_cadence",
        ci_ms: 9_683,
        why: Why::TimerAnchored,
        guards: "Injects a 3 s silence against a 500 ms report period and asserts exactly two \
                 counter lines for two keystrokes three seconds apart: the verdict is a \
                 cadence and the absence of lines is the assertion. The keyboard controller's \
                 health report is driven by the device's own byte cadence rather than by a \
                 host timeout that can certify a starved guest.",
    },
    Relegated {
        test: "xhci_slow_connect",
        ci_ms: 4_999,
        why: Why::TimerAnchored,
        guards: "Bounds the first port line from both sides at 0.400 s +/- 0.150 s, and \
                 refuses outright when a slow boot reaches the controller after the 300 ms \
                 held-empty window — a slower machine changes the verdict, not the price. \
                 This is the delayed-enumeration shape real hubs impose after reset.",
    },
    Relegated {
        test: "late_storage_connect",
        ci_ms: 6_229,
        why: Why::TimerAnchored,
        guards: "The same SLOW_CONNECT_NS window applied to the disk's port: a boot that \
                 outgrows it binds the disk in the port scan and the gate reds with \"the port \
                 was not held empty\". It is the storage-side delayed-port regression gate.",
    },
    Relegated {
        test: "audio_tone_load",
        ci_ms: 51_645,
        why: Why::Cost,
        guards: "Loaded Gate A on one and eight CPUs produces a valid tone and capture; a \
                 dropout or silent-period harm is confirmed by the second run and fails. \
                 Wake and cadence distributions remain the separate --audio-gate verdict.",
    },
    // 2026-08-21: `i8042_health` (47,121 ms) left this table — returned to
    // Fast on nightly run 32444411794's 9,509 ms, an honest Cost row measured
    // under the line. The i8042 pacing fix of 2026-08-19 (PR #143) is the
    // likely cause of the drop; its row is in `git log` on this file.
    Relegated {
        test: "syscall_window_nmi_controls",
        // **Derived from a hosted measurement, not measured on a shard, and it
        // says so.** The hosted lane priced this name's parent — all three boots
        // under one Fast name — at 19,740 ms (run 32580794553) against a
        // 10,000 ms ceiling, which is what the split is for. These are two of
        // those three boots, and the dev host puts them at 12.2 s of the 18.4 s
        // the three cost there, so 19,740 × 12.2/18.4 = 13,090. A committed
        // number rather than an `UNMEASURED` marker because only a Fast name may
        // carry one — fast CI is what replaces a marker, and fast CI does not
        // run this.
        //
        // **It is already known to be high, and that is tracked rather than
        // guessed at.** The same session replaced the storm's wall-clock arming
        // with the victim's own syscall count (`nmi_gate::SPINNING_SYSCALLS`),
        // which took these two boots from 12.2 s to 7.2 s on the same host. The
        // next nightly's measurement is the one to believe and it may well
        // return this name to Fast; it stays Nightly until a shard says so
        // rather than on that arithmetic, because a price that lands in the last
        // fifth before `FAST_COMMIT_MS` is decided by which partition ran it,
        // and 7.2 s scaled by the same ratio lands exactly there.
        ci_ms: 13_090,
        why: Why::Cost,
        guards: "That `syscall_window_nmi` is not vacuous: a kernel with vector 2's IST index \
                 taken off must double fault at the syscall entry with `cr2 = rsp - 8`, and a \
                 second NMI entered on IST2 through an early `iretq` must take the loud path \
                 rather than silently overwrite the outer handler's frame. The window property \
                 itself — arrivals at CPL 0 with a user `rsp`, symbolized to the entry, and a \
                 machine that survives 3,000 of them — is gated per pull request by the Fast \
                 name and is not what this row costs.",
    },
    Relegated {
        test: "kernel_log_file",
        ci_ms: 43_056,
        why: Why::Cost,
        guards: "Kernel messages survive into the on-disk log through the real backing \
                 volume and can be read after boot. Serial output alone cannot gate the \
                 persistent diagnostic the laptop depends on after a freeze.",
    },
    Relegated {
        test: "xhci_flap",
        ci_ms: 8_201,
        why: Why::TimerAnchored,
        guards: "Its two QMP writes must land inside one 100 ms debounce or the state under \
                 test never happens; a host that delays the second write reds a green machine \
                 with a sentence indistinguishable from the defect. An unplug and replug \
                 collapsed inside one debounce window leaves one coherent device rather than a \
                 ghost or a lost port.",
    },
    Relegated {
        test: "screen_paged_scrollback",
        ci_ms: 8_279,
        why: Why::TimerAnchored,
        guards: "Must watch the panic pager cycle at PAGE_HOLD_NS = 3 s per page until the \
                 first boot line comes round again — two distinct footers plus HEAD cannot be \
                 obtained without waiting out several of those periods. Without input, the \
                 automatic panic pager shows the first boot line and final panic marker on \
                 different pages; a single-page panic test cannot establish cycling.",
    },
    Relegated {
        test: "usb_storage_gate",
        ci_ms: 17_558,
        why: Why::Cost,
        guards: "Through raw USB mass storage, the guest reads a host-staged nonce, writes \
                 bytes the host verifies, reports clean read/write/refusal/health, leaves \
                 an unstamped disk byte-identical, and binds exactly the boot stick on \
                 metal-sim.",
    },
    // 2026-08-21: `i8042_absent` (10,410 ms, barely over) left this table —
    // returned to Fast on nightly run 32444411794's 9,221 ms. It carries a
    // standing redlist row, so its fast-tier reds are read against that rate
    // and never re-run away.
    Relegated {
        test: "audio_tone",
        ci_ms: 16_921,
        // Reclassified 2026-08-21 from `Cost`: nightly run 32444411794 measured
        // both labels under the line (8,450 + 8,471 ms) and the return rule
        // fired — wrongly, because this row's reason was never its price. It
        // plays a tone in real time and judges dropouts against wake-lateness
        // ceilings: a verdict anchored to real time, `TimerAnchored` by the
        // variant's own definition, and tests/CLAUDE.md already states both
        // audio configs are `Tier::Nightly` as law. The `Cost` label was the
        // 2026-08-12 sweep grading it by the number it happened to show.
        why: Why::TimerAnchored,
        guards: "The real-time audio pipeline glitch check per config: the tone captured on \
                 one and eight CPUs is checked for dropouts against per-run wake-lateness and \
                 underrun ceilings, with a harm verdict confirmed by a second boot before it \
                 fails. `audio_tone_load` runs the same check with two busy-spin burners \
                 added and was already Nightly.",
    },
    // 2026-08-13 sweep: the rest of the fast tier graded against
    // `FAST_CEILING_MS`'s 2026-08-12 boundary. Every row below is under the
    // line — none was relegated for cost — and moves for the same reason: its
    // verdict or duration is anchored to real time.
    Relegated {
        test: "metal_sim_null_audio",
        ci_ms: 9_712,
        why: Why::TimerAnchored,
        guards: "A host-measured drain rate with an 8 s ceiling on a 3.3 s expectation: what \
                 it measures is how fast a client's audio leaves the machine. The last two \
                 audio tests gated per pull request — after this, the claim \"sound comes out \
                 of this machine\" is gated nightly only, alongside its sibling below.",
    },
    Relegated {
        test: "null_sink_shipped_client",
        ci_ms: 6_590,
        why: Why::TimerAnchored,
        guards: "Two 1 s tones drained at soundd's real 2.902 ms period grid, each guarded by \
                 a 15 s wall-clock stuck-detector — real audio playing in real time, the other \
                 half of the last-two-audio-tests loss `metal_sim_null_audio` names.",
    },
    Relegated {
        test: "netd_hostile_peer",
        ci_ms: 4_195,
        why: Why::TimerAnchored,
        guards: "Times netd's 2 s handshake deadline against the clock and counts what \
                 survived a 1 ms-paced burst plus a fixed 100 ms settle before reading how \
                 many netd kept — both wall-clock margins.",
    },
    Relegated {
        test: "usb_transport_break",
        ci_ms: 5_382,
        why: Why::TimerAnchored,
        guards: "`breaks > 2` counts who won the race between the device's late answer to the \
                 abandoned transfer and the Bulk-Only reset — its own doc says one break under \
                 KVM and two under TCG off the same tree. Dynamic USB goes nightly-only with \
                 the three below: what stays gated per pull request is enumeration, \
                 descriptors, slots, PORTSC, short reads, write errors and pool exhaustion — \
                 every static shape — but no pull request exercises a device arriving or \
                 leaving while the machine runs.",
    },
    Relegated {
        test: "xhci_hotplug",
        ci_ms: 7_711,
        why: Why::TimerAnchored,
        guards: "Stages every plug and unplug on fixed 800 ms waits against the driver's \
                 100 ms debounce, with 20-200 ms sleeps pacing the input pokes that follow.",
    },
    Relegated {
        test: "usb_refused_disk_first",
        ci_ms: 7_286,
        why: Why::TimerAnchored,
        guards: "Two fixed 1,200 ms settles around the device_del and the blockdev_add/\
                 device_add, then a fixed 20 s drain that every asserted line must arrive \
                 inside.",
    },
    Relegated {
        test: "usb_disk_index_stable",
        ci_ms: 6_649,
        why: Why::TimerAnchored,
        guards: "A fixed 1,200 ms hotplug settle staged against a 100 ms debounce — \"this is \
                 that with room\" — waited out before the LATE_READY assertion is read.",
    },
    Relegated {
        test: "screen_blocked_dump",
        ci_ms: 4_679,
        why: Why::TimerAnchored,
        guards: "A fixed 2 s settle placed inside the dump's own guest-timed 15 s hold, and \
                 the verdict is whether the report survived the desktop's next repaint — \
                 which only where that wait lands decides.",
    },
    Relegated {
        test: "screen_diag_boot",
        ci_ms: 6_952,
        why: Why::TimerAnchored,
        guards: "thread::sleep(5 s) is the measurement: the assertion is literally that the \
                 log is still on the panel five seconds after the boot finished.",
    },
    // 2026-08-13, second pass: the sweep above kept `i8042_quarantine` Fast on
    // the strength of one under-ceiling committed number; CI found otherwise.
    Relegated {
        test: "i8042_quarantine",
        ci_ms: 11_073,
        why: Why::TimerAnchored,
        guards: "The fault quarantines (masks) the controller's GSI within milliseconds of \
                 `===I8042_READY===` — confirmed from the serial log, before a host round trip \
                 could land anything — so no sentinel `test_rs_i8042_keyboard` might send can \
                 ever reach the guest, and every run necessarily pays the binary's full 5 s \
                 fallback deadline. That fixed wall-clock window is the verdict's floor, not \
                 an incidental cost, which is why the price straddles the 10,000 ms line run \
                 to run rather than sitting on one side of it: 9,355 ms committed, 10,568 ms \
                 in nightly run 31680778730, 11,073 ms in run 31704997228.",
    },
    // 2026-08-19: PR #132's run measured it over the line while PR #125's run,
    // minutes apart on the same main, measured 4,774 ms — the price is the
    // partition's, not the code's, and Fast's promise has to hold under every
    // legal partition. 2026-08-21: nightly run 32444411794 measured 4,578 ms
    // and the `Cost` return rule fired; reclassified instead, because a quiet
    // nightly's number is exactly the reading this row says cannot decide it —
    // what it measures is co-scheduling stretch, a rate, `TimerAnchored` in the
    // variant's own terms. Returning it on one calm sample would re-import the
    // straddle the row records.
    Relegated {
        test: "c_capture_ignores_daemon_lines",
        ci_ms: 12_612,
        why: Why::TimerAnchored,
        guards: "A C program's capture is stripped of other processes' lines before the \
                 comparison, on the boot config's own list of who may speak, with the \
                 filter turned off as the control. Its own work is two `echo`s and string \
                 comparisons — no clock in it — so the price is Sched::Parallel \
                 co-scheduling: 5,241 ms committed, 12,612 ms in run 32301181725 and \
                 4,774 ms in run 32301828122 the same evening, straddling the 10,000 ms \
                 line run to run. The next KVM measurements decide whether it returns to \
                 Fast.",
    },
    // 2026-08-21, the same day it was returned: PR #186 read nightly run
    // 32444411794's 9,221 ms as a Cost row crossing back and put it in Fast;
    // the first two merge-queue compositions after that measured it at
    // 10,738 ms twice (runs 32475363422, 32476143292) and the durations gate
    // bounced every queue entry. Its history is a straddle — 10,410 / 9,221 /
    // 10,738 — and its verdict is two boots compared within 300 ms, a real-time
    // quantity by construction. It was never a Cost row; the return was the
    // orchestrator's misreading, and `issues/build/defect-events.md` says so.
    Relegated {
        test: "i8042_absent",
        ci_ms: 10_738,
        why: Why::TimerAnchored,
        guards: "A normal boot is paired with i8042=off: the latter clears the FADT bit, \
                 exposes the floating 0xff bus refusal, and must complete within the 300 ms \
                 comparison bound — a wall-clock verdict whose price straddles the 10,000 ms \
                 line run to run, so no single calm sample may return it.",
    },
    // 2026-08-21, the margin sweep: [`FAST_COMMIT_MS`] applied to the rest of
    // the fast tier. These three are every `Tier::Fast` name the committed
    // profile priced inside the band, and each is an honest `Why::Cost` — none
    // has a real-time verdict, so none is `TimerAnchored`. Their prices are the
    // committed profile's, unchanged by this sweep: nothing new was adopted.
    Relegated {
        test: "i8042_health",
        ci_ms: 9_509,
        why: Why::Cost,
        guards: "Two boots decide whether the keyboard's own health line can be believed: one \
                 machine nobody touches must say `0 interrupts — the pin has never asserted` \
                 and must not print either of the two lines reachable from the `irqs > 0` gate, \
                 and one machine with a single keystroke must report interrupts, bytes and keys \
                 all non-zero — the pin, the ISR and the decoder as one chain, where interrupts \
                 alone would go green on a driver whose ring never filled. It also counts \
                 `sched: cpu=` lines to prove `verdict_due` self-clears rather than holding a \
                 CPU awake, which is the failure the quarantine path already had once. What \
                 still runs per pull request: `screen_i8042_health` (4,495 ms) reads the panel \
                 copy of the verdict, so a driver that stops speaking entirely is still caught \
                 — but nothing gated per pull request asks whether the quiet and the asserting \
                 machines say *different* things, which is the whole claim. Relegated for \
                 margin, not for a crossing: 9,509 ms committed against 10,281 ms in run \
                 32506320411, 8% apart with the 10,000 ms line between them.",
    },
    Relegated {
        test: "double_panic_names_the_fault",
        ci_ms: 9_120,
        why: Why::Cost,
        guards: "The only execution anywhere of `fatal_exception`'s kernel arm and the \
                 `DOUBLE PANIC` branch below it — a Ring 0 exception is not something a guest \
                 program or a QEMU property can produce, so before this test the branch had \
                 never run under a test at all. It asserts that a machine two crashes deep \
                 names which of four states the arriving panic found, the fault by name and \
                 rip, and the panic that ended it, on the record channel and again on the \
                 lock-free 16550 copy that a wedged log path cannot hold. What still runs per \
                 pull request: `reentry_names_the_first_panic` (5,073 ms) covers the other dead \
                 end — the panic *report* panicking on a CPU already at depth one — so the \
                 pre-panic byte capture still has a gate; what goes dark is the panic-on-fault \
                 half, where the depth is zero and `FAULT rip=` never printed.",
    },
    Relegated {
        test: "console_line_atomicity",
        ci_ms: 8_925,
        why: Why::Cost,
        guards: "That a `write` syscall, and not a buffer boundary, is the unit of console \
                 interleaving: two writers on two CPUs put 2,000 lines through one console and \
                 not one line may carry both tags, at exact width, with both writers' full \
                 counts present so a lost capture cannot pass as a clean one. Two more claims \
                 ride the same capture and have no other gate: a process that exits mid-line \
                 has its unterminated bytes flushed by the last handle's drop, asserted as an \
                 exact run length in both directions, and no kernel record may land inside a \
                 userland line. Every other test in the tree reads the console assuming all \
                 three; none of them asserts one. Relegated for margin: 8,925 ms committed, \
                 within 11% of the line.",
    },
];

/// The names [`RELEGATED`] holds, which is what `tests/toyos.rs` checks its own
/// registration against.
pub fn relegated_names() -> BTreeSet<&'static str> {
    RELEGATED.iter().map(|r| r.test).collect()
}

/// The registration name a duration-profile label belongs to.
///
/// Audio runs one registered test on two SMP configurations and deliberately
/// records both measurements. Tier selection and duration preservation must
/// compare those labels with the one registration rather than treating the
/// suffix as a third, undeclared naming scheme.
pub fn canonical_profile_name(label: &str) -> &str {
    let Some((base, suffix)) = label.rsplit_once(" (smp=") else { return label };
    let Some(smp) = suffix.strip_suffix(')') else { return label };
    if matches!(base, "audio_tone" | "audio_tone_load")
        && !smp.is_empty()
        && smp.bytes().all(|b| b.is_ascii_digit())
    {
        base
    } else {
        label
    }
}

/// What the fast tier stopped paying for, in milliseconds — added up from the
/// rows rather than written down, so the figure the suite prints cannot drift
/// from the list it is a figure about.
pub fn relegated_ms() -> u64 {
    RELEGATED.iter().map(|r| r.ci_ms).sum()
}

/// One thing the tier rule has to say about one registered name.
///
/// The name is carried beside the sentence because *which* run renders a
/// verdict is decided per name: `src/durations.rs` refuses the ones the change
/// under measurement registered or re-tiered and prints the rest as warnings,
/// and it cannot do that against a block of prose.
pub struct Verdict {
    /// The registration name — `canonical_profile_name` of the label for a
    /// price verdict, `Relegated::test` for a row verdict.
    pub name: String,
    /// **Whether this verdict is about a measured price.** Only a priced
    /// verdict may be softened to a warning by a run that did not touch the
    /// name: it is the one kind whose truth depends on which shard ran the
    /// test rather than on what the tree says. A marker on a Nightly row, a
    /// duplicate row, an empty `guards`, missing evidence and a rider on a
    /// carrier that is not Nightly are all facts about the declaration, true on
    /// every run, and are refused on every run.
    pub priced: bool,
    /// The sentence, naming the name, the price and what is wrong with it.
    pub message: String,
}

/// Validate the complete CI duration profile against the declared tiers: every
/// verdict the rule renders against `ci`, one per finding, each carrying the
/// registered name it is about and whether it is about a price.
///
/// This is production code because `--merge-durations` is the required gate.
/// A filtered Rust unit-test invocation can exit successfully after running
/// zero tests, so CI must not depend on a test name remaining spelled a
/// particular way for the policy verdict to execute.
///
/// The whole verdict, every name: what the nightly renders, and what
/// `src/durations.rs` filters by name on a pull request or a merge-queue
/// composition. Nothing here knows about a base — the filtering is the caller's,
/// because the rule and the audience are two decisions and only one of them is
/// this module's.
pub fn ci_profile_verdicts(ci: &BTreeMap<String, u64>) -> Vec<Verdict> {
    /// A verdict about the declaration: true whoever measured it.
    fn fact(name: &str, message: String) -> Verdict {
        Verdict { name: name.to_string(), priced: false, message }
    }

    let mut errors: Vec<Verdict> = Vec::new();
    let mut seen = BTreeSet::new();
    let nightly = relegated_names();

    for (label, &ms) in ci {
        let name = canonical_profile_name(label);
        if ms == UNMEASURED_MS {
            if nightly.contains(name) {
                errors.push(fact(
                    name,
                    format!(
                        "{label} is marked UNMEASURED but {name} is Nightly, so fast CI cannot \
                         execute it to replace the marker; bootstrap it as Fast"
                    ),
                ));
            }
            continue;
        }
        if nightly.contains(name) {
            continue;
        }
        if ms > FAST_CEILING_MS {
            errors.push(Verdict {
                name: name.to_string(),
                priced: true,
                message: format!(
                    "{label} measured {ms} ms in CI, over the {FAST_CEILING_MS} ms line, \
                     but {name} remains Fast"
                ),
            });
        } else if ms > FAST_COMMIT_MS {
            errors.push(Verdict {
                name: name.to_string(),
                priced: true,
                message: format!(
                    "{label} is priced at {ms} ms — over the {FAST_COMMIT_MS} ms a Fast test \
                     may be committed at and under the {FAST_CEILING_MS} ms line — and {name} \
                     remains Fast: priced without margin, so relegate it or make it faster. A \
                     price this close to the line is decided by which partition ran it, and \
                     reds whichever pull request measures it next"
                ),
            });
        }
    }

    for row in RELEGATED {
        if !seen.insert(row.test) {
            errors.push(fact(row.test, format!("{} has two tier rows", row.test)));
        }
        if row.guards.trim().is_empty() {
            errors
                .push(fact(row.test, format!("{} says nothing about what it guards", row.test)));
        }
        let labels: Vec<(&str, u64)> = ci
            .iter()
            .filter(|(label, _)| canonical_profile_name(label) == row.test)
            .map(|(label, &ms)| (label.as_str(), ms))
            .collect();
        if labels.is_empty() {
            errors.push(fact(
                row.test,
                format!(
                    "{} is in the nightly tier but has missing CI evidence; an unmeasured \
                     test stays Nightly, but its evidence must be restored",
                    row.test
                ),
            ));
            continue;
        }
        // The profile loop above already refuses a marker on a Nightly row; do
        // not also grade a sentinel here as if it were a fresh measurement.
        if labels.iter().any(|(_, ms)| *ms == UNMEASURED_MS) {
            continue;
        }
        match row.why {
            // **The return rule, and it asks for margin.** Not "under the
            // ceiling" — that returned `i8042_absent` on one calm nightly
            // sample and every merge-queue composition after it measured the
            // same test over the line. A relegated test comes back only when
            // its price has room to move.
            Why::Cost if labels.iter().all(|(_, ms)| *ms <= FAST_COMMIT_MS) => {
                errors.push(Verdict {
                    name: row.test.to_string(),
                    priced: true,
                    message: format!(
                        "{} is Nightly for Cost, but every current CI label is at or under \
                         the {FAST_COMMIT_MS} ms commitment line and it belongs Fast: {labels:?}",
                        row.test
                    ),
                });
            }
            Why::RidesTheBootOf(carrier) => {
                if !labels.iter().all(|(_, ms)| *ms <= FAST_COMMIT_MS) {
                    errors.push(Verdict {
                        name: row.test.to_string(),
                        priced: true,
                        message: format!(
                            "{} is priced without margin itself and must be Why::Cost, not a \
                             rider on {carrier}: {labels:?}",
                            row.test
                        ),
                    });
                }
                if !nightly.contains(carrier) {
                    errors.push(fact(
                        row.test,
                        format!("{} rides {carrier}, but {carrier} is not Nightly", row.test),
                    ));
                }
            }
            Why::Cost | Why::TimerAnchored => {}
        }
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Every verdict as one block of prose, which is what an assertion about
    /// the whole rule reads. `src/durations.rs` renders them one at a time
    /// because it decides each one's audience separately; nothing here does.
    fn validate_ci_profile(ci: &BTreeMap<String, u64>) -> Result<(), String> {
        let verdicts = ci_profile_verdicts(ci);
        if verdicts.is_empty() {
            return Ok(());
        }
        Err(verdicts.into_iter().map(|v| v.message).collect::<Vec<_>>().join("\n"))
    }

    fn committed_profile() -> BTreeMap<String, u64> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test-durations");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
            .lines()
            .filter_map(|l| l.rsplit_once(' '))
            .filter_map(|(n, ms)| ms.parse().ok().map(|ms| (n.to_string(), ms)))
            .collect()
    }

    /// The decisive, bidirectional gate the `durations` CI job runs after it
    /// merges all twelve shards. A slow label left in Fast and a Cost row whose
    /// current label is missing or no longer slow are equally policy drift.
    #[test]
    fn the_ci_profile_and_tiers_agree() {
        if let Err(refusal) = validate_ci_profile(&committed_profile()) {
            panic!("the merged CI profile and tier declaration disagree:\n{refusal}");
        }
    }

    #[test]
    fn the_profile_gate_refuses_missing_cost_evidence() {
        let mut ci = committed_profile();
        ci.remove("desktop_window_child");
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("desktop_window_child"), "{refusal}");
        assert!(refusal.contains("missing CI evidence"), "{refusal}");
    }

    #[test]
    fn the_profile_gate_refuses_a_slow_fast_label() {
        let mut ci = committed_profile();
        ci.insert("iommu_empty_domain".to_string(), FAST_CEILING_MS + 1);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("iommu_empty_domain"), "{refusal}");
        assert!(refusal.contains("remains Fast"), "{refusal}");
    }

    /// The derivation [`FAST_COMMIT_MS`]'s doc claims, asserted rather than
    /// described — including the consequence the width was chosen for.
    #[test]
    fn the_commitment_line_is_four_fifths_of_the_ceiling() {
        assert_eq!(FAST_COMMIT_MS, 8_000);
        assert_eq!(FAST_COMMIT_MS * 5, FAST_CEILING_MS * 4);
        // A Fast name measured over the ceiling has grown by at least a quarter
        // over the worst price it could lawfully have been committed at, which
        // is what makes that red a finding rather than a coin landing. Both
        // sides are constants, so the compiler is the one that checks it.
        const { assert!(FAST_CEILING_MS * 100 / FAST_COMMIT_MS >= 125) };
    }

    /// **The margin rule, and the millisecond it turns on.** A Fast label
    /// priced inside the band is refused by name; one millisecond lower is
    /// not; and the ceiling's own older red still speaks in its own words above
    /// the band rather than being swallowed by it.
    #[test]
    fn a_fast_label_priced_without_margin_is_refused() {
        let at = |ms: u64| {
            let mut ci = committed_profile();
            ci.insert("iommu_empty_domain".to_string(), ms);
            validate_ci_profile(&ci)
        };
        assert!(at(FAST_COMMIT_MS).is_ok());

        let refusal = at(FAST_COMMIT_MS + 1).unwrap_err();
        assert!(refusal.contains("iommu_empty_domain"), "{refusal}");
        assert!(refusal.contains("priced without margin"), "{refusal}");

        let refusal = at(FAST_CEILING_MS + 1).unwrap_err();
        assert!(refusal.contains("remains Fast"), "{refusal}");
        assert!(!refusal.contains("priced without margin"), "{refusal}");
    }

    /// **The return rule, both halves.** A `Why::Cost` row comes back only when
    /// every current label has margin — a label inside the band leaves it
    /// Nightly, which is precisely the straddle `i8042_absent` re-imported by
    /// returning on one calm sample under the ceiling.
    #[test]
    fn a_cost_row_returns_to_fast_only_with_margin() {
        let mut ci = committed_profile();
        ci.insert("audio_tone_load (smp=1)".to_string(), FAST_COMMIT_MS);
        ci.insert("audio_tone_load (smp=8)".to_string(), FAST_COMMIT_MS - 1);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("audio_tone_load"), "{refusal}");
        assert!(refusal.contains("belongs Fast"), "{refusal}");

        // One label without margin is enough to keep the whole registration
        // Nightly, and it is not itself a refusal: a Nightly row in the band is
        // the state this rule exists to hold.
        ci.insert("audio_tone_load (smp=8)".to_string(), FAST_COMMIT_MS + 1);
        assert!(validate_ci_profile(&ci).is_ok());
        ci.insert("audio_tone_load (smp=8)".to_string(), FAST_CEILING_MS);
        assert!(validate_ci_profile(&ci).is_ok());
    }

    /// §5: "Nightly measurements refresh the recorded Nightly costs; they are
    /// validated against the tier rule, never against equality with a past
    /// measurement." A fresh nightly run never reproduces every `ci_ms` to the
    /// millisecond — this drifts every Cost row's numbers, keeping each safely
    /// over the ceiling — and the profile must still validate: `ci_ms` is
    /// last-measured documentation, not a fixture the merge checks against.
    #[test]
    fn a_nightly_measurement_drifts_ci_ms_and_still_validates() {
        let mut ci = committed_profile();
        let cost_names: BTreeSet<&str> =
            RELEGATED.iter().filter(|r| r.why == Why::Cost).map(|r| r.test).collect();
        for (label, ms) in ci.iter_mut() {
            if cost_names.contains(canonical_profile_name(label.as_str())) {
                *ms += 12_345;
            }
        }
        assert!(validate_ci_profile(&ci).is_ok());
    }

    /// The other half of the same bidirectional rule: drifted numbers do not
    /// launder a Cost row that a fresh measurement puts at or under the
    /// commitment line. That is a real tier-placement finding ("returns to
    /// Fast"), never masked by ci_ms no longer being checked for equality.
    #[test]
    fn a_cost_row_with_margin_still_reds_despite_drifted_ci_ms() {
        let mut ci = committed_profile();
        ci.insert("desktop_window_child".to_string(), FAST_COMMIT_MS);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("desktop_window_child"), "{refusal}");
        assert!(refusal.contains("belongs Fast"), "{refusal}");
    }

    #[test]
    fn only_fast_can_carry_the_one_run_unmeasured_marker() {
        let mut ci = committed_profile();
        ci.insert("iommu_empty_domain".to_string(), UNMEASURED_MS);
        assert!(validate_ci_profile(&ci).is_ok());

        ci.insert("audio_tone_load (smp=1)".to_string(), UNMEASURED_MS);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("audio_tone_load (smp=1)"), "{refusal}");
        assert!(refusal.contains("bootstrap it as Fast"), "{refusal}");
    }

    #[test]
    fn audio_profile_labels_have_one_registration_name() {
        assert_eq!(canonical_profile_name("audio_tone_load (smp=1)"), "audio_tone_load");
        assert_eq!(canonical_profile_name("audio_tone (smp=8)"), "audio_tone");
        assert_eq!(canonical_profile_name("ordinary_test"), "ordinary_test");
        assert_eq!(canonical_profile_name("not_audio (smp=8)"), "not_audio (smp=8)");
    }
}
