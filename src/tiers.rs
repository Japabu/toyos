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
//! **This is interim and it is a loss.** Fifty-two registered tests are
//! Nightly: twenty-six for [`Why::Cost`] — a CI execution over the line
//! (loaded audio and ordinary `audio_tone` each have two measured SMP labels,
//! all four over it) — twenty-two for [`Why::TimerAnchored`], Nightly by
//! classification rather than by cost, mostly nowhere near the line and one
//! (`i8042_quarantine`) straddling it run to run for the classification's own
//! reason — and four for [`Why::RidesTheBootOf`], riding
//! `metal_sim_compositor`'s shared boot. Between them they account for
//! 1,755.2 s of the 2,102.5 s the committed profile still prices (the two
//! labels below its `UNMEASURED_MS` markers are the rest of it), and none is
//! gated per pull request. `guards` on every row says what stopped being gated,
//! because a run that quietly does less is the whole failure mode here —
//! `specs/assessments/test-cost-audit.md` §7 is the long form.
//!
//! **Nothing here is an optimisation and nothing here changes an assertion.**
//! A relegated test measures exactly what it measured; the manual nightly
//! command runs it. #188 holds only the optimisation work that would make one
//! of these fast enough to come back to the per-PR tier — and **two names left
//! by that door on 2026-08-17**: `xhci_msi_only` (35,223 ms) and
//! `swiss_german_layout` (12,645 ms) were each a guest binary waiting out a
//! fixed fallback deadline nobody had sent the sentinel for, 30 s and 8 s of
//! host wall clock with no assertion behind either. They are registered Fast
//! against an `UNMEASURED_MS` marker until the shards price them;
//! `specs/assessments/test-cost-audit.md` §5.10 is the measurement.
//!
//! **CI is the instrument for a per-PR policy.** The effective profile starts
//! with the last full twelve-shard run and replaces every name measured by the
//! first fast-tier run. That retains a price for withheld tests while using the
//! freshest CI price for everything the fast tier did execute. Dev-host TCG
//! timings remain useful optimisation evidence, but they do not decide which
//! side of a KVM CI cutoff a test belongs on.

use std::collections::{BTreeMap, BTreeSet};

/// The ceiling the fast tier is defined by, in milliseconds.
///
/// Policy, and the owner's. A test at exactly the line is fast: the rule he
/// stated is "ten seconds or less".
///
/// **2026-08-12: the line is hard, and there is deliberately no margin or
/// hysteresis band** — a measured crossing reds `durations`, however close.
/// Same date, the tier boundary: a test whose verdict or duration is anchored
/// to real time — it plays or records in real time, waits out a staged latency
/// window, or measures a rate, such that a 2x slower machine would change its
/// verdict or price — belongs Nightly; only a compute-bound verdict stays Fast.
/// **2026-08-13: the sweep applying this to the rest of the fast tier landed**
/// — [`Why::TimerAnchored`] is the classification it needed, and every
/// borderline name `specs/assessments/test-cost-audit.md` §7 raised has one of the three
/// `Why` rows now.
pub const FAST_CEILING_MS: u64 = 10_000;

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
    /// Over [`FAST_CEILING_MS`] by itself.
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
    /// not a fixture: `validate_ci_profile` checks a fresh profile's tier
    /// *placement*, never this field against it, so a nightly run refreshing
    /// every Nightly label does not have to reproduce this number. A human
    /// updates it by hand when a "returns to Fast" or "belongs Nightly"
    /// finding lands a tier correction — `specs/testing-strategy.md` §5.
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
        guards: "The kernel-thread machinery: klogd spawns with a process-table row, \
                 `ps` and the census name it, and a deliberate panic in it halts the \
                 machine instead of being recovered off a stale `syscall_rip` — the \
                 nondeterminism §4.3 exists to forbid. Two boots, and the second (the \
                 `klogd-panic` actuator) is the cost; the spawn half alone is one \
                 cheap boot, and \
                 specs/issues/build/klogd-hosted-pays-two-boots-for-one-fast-verdict.md \
                 is the split that puts it back in the fast tier. What still runs per \
                 pull request: every boot's console output is klogd's drain, so the \
                 thread starving or dying is visible in any test that reads a line.",
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
                 `std_unwind_so` trio (specs/user-machine-state.md §2, \
                 specs/assessments/ci-plan-assessment-2026-08.md §9.3), ~51 ms riding an \
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
    Relegated {
        test: "idle_stack_guard",
        ci_ms: 52_822,
        why: Why::Cost,
        guards: "The guard page under every per-CPU idle stack. Its absence is invisible \
                 to every log line and every screendump — an overflow rewrote whatever the \
                 allocator had put underneath — so the only way to ask is to touch it, and \
                 SYS_DEBUG action 9 is the one read. **`ci_ms` is now stale on the high \
                 side and deliberately left:** 2026-08-17 took a flat 20 s `drain_serial` \
                 off it — the fatal path halts every CPU without QEMU exiting, so the drain \
                 waited out its whole ceiling for a machine that would never speak again — \
                 and the same test measures 28.5 s to 3.0 s on the dev host. Whether \
                 that is enough to cross back is a KVM question this branch cannot answer, \
                 so the number above is the last CI measurement and the next nightly run \
                 replaces it.",
    },
    Relegated {
        test: "dump_nmi_probe",
        ci_ms: 24_625,
        why: Why::Cost,
        guards: "Ctrl+Alt+D's NMI probe: a CPU that ignores a kick is named and then asked \
                 where it is with the one interrupt it cannot mask, with the rip it brings \
                 back resolved against the kernel's own symbols. On the T14 the dump named \
                 three CPUs without saying which of three causes each was. **`ci_ms` is \
                 stale on the high side for the same reason `idle_stack_guard`'s is:** \
                 2026-08-17 replaced its flat 20 s `drain_serial` with the two lines the \
                 report actually owes — an NMI interrupts a CPU rather than killing it, so \
                 the guest neither exits nor halts and the drain was paid in full on every \
                 green run — and it measures 22.4 s to 6.0 s on the dev host. That may put \
                 it under the line on KVM; the next nightly measurement decides, and it is \
                 the one row here most likely to return to Fast.",
    },
    Relegated {
        test: "metal_sim_pointer_churn",
        ci_ms: 260_607,
        why: Why::Cost,
        guards: "Eight plug-and-pull cycles of a pointer under a compositor holding the \
                 merged pointer's fd across all of them. The owner froze his desktop this \
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
                 relegation is about was cut about sevenfold at L6 of the log architecture** — \
                 `/bin/logd` ends on an error instead of retrying inside a budget, which is what \
                 turned 1,737 failing flushes over six seconds into the handful a single refusal \
                 costs (`specs/log-architecture-spec.md` §5.4) — and `ci_ms` above is untouched \
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
    Relegated {
        test: "i8042_health",
        ci_ms: 47_121,
        why: Why::Cost,
        guards: "Two boots distinguish untouched silence from one injected key: the quiet \
                 report has zero interrupts and no alive/mute verdict; the active report \
                 has nonzero interrupts, bytes and keys, and its health wake does not keep \
                 a CPU spinning.",
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
    Relegated {
        test: "i8042_absent",
        ci_ms: 10_410,
        why: Why::Cost,
        guards: "A normal boot is paired with i8042=off: the latter clears the FADT bit, \
                 exposes the floating 0xff bus refusal, and must complete within the 300 ms \
                 comparison bound.",
    },
    Relegated {
        test: "audio_tone",
        ci_ms: 21_934,
        why: Why::Cost,
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

/// Validate the complete CI duration profile against the declared tiers.
///
/// This is production code because `--merge-durations` is the required gate.
/// A filtered Rust unit-test invocation can exit successfully after running
/// zero tests, so CI must not depend on a test name remaining spelled a
/// particular way for the policy verdict to execute.
pub fn validate_ci_profile(ci: &BTreeMap<String, u64>) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();
    let nightly = relegated_names();

    for (label, &ms) in ci {
        let name = canonical_profile_name(label);
        if ms == UNMEASURED_MS {
            if nightly.contains(name) {
                errors.push(format!(
                    "{label} is marked UNMEASURED but {name} is Nightly, so fast CI cannot \
                     execute it to replace the marker; bootstrap it as Fast"
                ));
            }
            continue;
        }
        if ms > FAST_CEILING_MS && !nightly.contains(name) {
            errors.push(format!(
                "{label} measured {ms} ms in CI, over the {FAST_CEILING_MS} ms line, \
                 but {name} remains Fast"
            ));
        }
    }

    for row in RELEGATED {
        if !seen.insert(row.test) {
            errors.push(format!("{} has two tier rows", row.test));
        }
        if row.guards.trim().is_empty() {
            errors.push(format!("{} says nothing about what it guards", row.test));
        }
        let labels: Vec<(&str, u64)> = ci
            .iter()
            .filter(|(label, _)| canonical_profile_name(label) == row.test)
            .map(|(label, &ms)| (label.as_str(), ms))
            .collect();
        if labels.is_empty() {
            errors.push(format!(
                "{} is in the nightly tier but has missing CI evidence; an unmeasured \
                 test stays Nightly, but its evidence must be restored",
                row.test
            ));
            continue;
        }
        // The profile loop above already refuses a marker on a Nightly row; do
        // not also grade a sentinel here as if it were a fresh measurement.
        if labels.iter().any(|(_, ms)| *ms == UNMEASURED_MS) {
            continue;
        }
        match row.why {
            Why::Cost if !labels.iter().any(|(_, ms)| *ms > FAST_CEILING_MS) => {
                errors.push(format!(
                    "{} is Nightly for Cost, but every current CI label is at or under \
                     the {FAST_CEILING_MS} ms line and it belongs Fast: {labels:?}",
                    row.test
                ));
            }
            Why::RidesTheBootOf(carrier) => {
                if !labels.iter().all(|(_, ms)| *ms <= FAST_CEILING_MS) {
                    errors.push(format!(
                        "{} now crosses the line itself and must be Why::Cost: {labels:?}",
                        row.test
                    ));
                }
                if !nightly.contains(carrier) {
                    errors.push(format!(
                        "{} rides {carrier}, but {carrier} is not Nightly",
                        row.test
                    ));
                }
            }
            Why::Cost | Why::TimerAnchored => {}
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
        ci.insert("hda_probe".to_string(), FAST_CEILING_MS + 1);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("hda_probe"), "{refusal}");
        assert!(refusal.contains("remains Fast"), "{refusal}");
    }

    #[test]
    fn the_profile_gate_returns_a_cost_row_to_fast_at_the_line() {
        let mut ci = committed_profile();
        ci.insert("audio_tone_load (smp=1)".to_string(), FAST_CEILING_MS);
        ci.insert("audio_tone_load (smp=8)".to_string(), FAST_CEILING_MS - 1);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("audio_tone_load"), "{refusal}");
        assert!(refusal.contains("belongs Fast"), "{refusal}");
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
    /// ceiling. That is a real tier-placement finding ("returns to Fast"),
    /// never masked by ci_ms no longer being checked for equality.
    #[test]
    fn a_cost_row_at_the_ceiling_still_reds_despite_drifted_ci_ms() {
        let mut ci = committed_profile();
        ci.insert("desktop_window_child".to_string(), FAST_CEILING_MS);
        let refusal = validate_ci_profile(&ci).unwrap_err();
        assert!(refusal.contains("desktop_window_child"), "{refusal}");
        assert!(refusal.contains("belongs Fast"), "{refusal}");
    }

    #[test]
    fn only_fast_can_carry_the_one_run_unmeasured_marker() {
        let mut ci = committed_profile();
        ci.insert("hda_probe".to_string(), UNMEASURED_MS);
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
