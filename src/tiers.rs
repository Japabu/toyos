//! Which tests every `cargo test` runs, and which ones it does not — as data,
//! with what each relegation costs the tree written beside it.
//!
//! **The owner's line, 2026-08-11: the fast per-PR path runs tests taking ten
//! seconds or less.** Everything above it moves to a manually invoked nightly
//! tier. `--nightly` exists now; scheduled CI is separate, unassigned work in
//! `specs/issues/build/nightly-tier-has-no-workflow.md`. This module is that
//! decision as data: [`RELEGATED`] is exactly the set the manual command selects
//! and a future scheduled job must include, and `tests/toyos.rs` writes
//! [`Tier::Nightly`] against each of those names in its own registration.
//!
//! **This is interim and it is a loss.** Fifty-five registered tests have a CI
//! execution over the line (loaded audio and ordinary `audio_tone` each have two
//! measured SMP labels), and six more ride a shared boot with one that is.
//! Between them they account for 4,105.7 s of the effective 4,439.1 s CI
//! profile, and none is gated per pull request. `guards` on every row says what
//! stopped being gated, because a run that quietly does less is the whole
//! failure mode here — `specs/test-cost-audit.md` §7 is the long form.
//!
//! **Nothing here is an optimisation and nothing here changes an assertion.**
//! A relegated test measures exactly what it measured; the manual nightly
//! command runs it. #188 holds only the optimisation work that would make one
//! of these fast enough to come back to the per-PR tier.
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
/// The full sweep applying this to the rest of the fast tier is pending as its
/// own change.
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
    /// Only manual `cargo test --test toyos-build -- --nightly` until the
    /// scheduled workflow in `specs/issues/build/nightly-tier-has-no-workflow.md`
    /// is built.
    Nightly,
}

/// Why a name is not in the fast tier. The two are not interchangeable and the
/// gates below check different things of each.
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
    /// This is the collateral the record has to name: six tests here cost 11.3 s
    /// between them and go dark with their two slow carrier boots.
    RidesTheBootOf(&'static str),
}

/// One test that the fast tier does not run.
pub struct Relegated {
    pub test: &'static str,
    /// Milliseconds in the effective CI profile. For `audio_tone_load`, which
    /// registers one test but emits an `(smp=1)` and `(smp=8)` label, this is the
    /// sum of both labels; the cutoff is still applied to each label separately.
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
        test: "desktop_window_child",
        ci_ms: 65_217,
        why: Why::Cost,
        guards: "A window opened from a shell and then closed, and whether the desktop \
                 answers afterwards. The only reproduction #156 has anywhere. Its \
                 EXPECTED_FAILURES entry is Stale::OnThisDate, so the declaration still \
                 expires on 2026-09-06 whether or not the test has run since.",
    },
    Relegated {
        test: "sshd_fail_closed",
        ci_ms: 73_258,
        why: Why::Cost,
        guards: "sshd once accepted every credential. This is the gate for that class: it \
                 mints a host identity under /home, finds no authorized_keys, \
                 authenticates nobody, and — the half a missing-file check passes without \
                 — never holds port 22 while it cannot accept a key.",
    },
    Relegated {
        test: "fpu_isolation",
        ci_ms: 67_692,
        why: Why::Cost,
        guards: "The whole user machine state surviving every exit from Ring 3, on a \
                 one-CPU machine, against a second boot of an `fpu-save-nothing` kernel \
                 that must fail the same three arms. A negative gate: without the second \
                 boot the first proves only that the machine works, which it did before \
                 the gate existed.",
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
        ci_ms: 231_761,
        why: Why::Cost,
        guards: "Whether a fatal panic can paint the panel once a compositor owns the \
                 scanout. That is the T14's only configuration and the assumption three \
                 freeze investigations rested on; no other screen test asks it.",
    },
    Relegated {
        test: "metal_sim_compositor",
        ci_ms: 230_812,
        why: Why::Cost,
        guards: "The four daemons surviving the T14's device shape, each in its own words: \
                 the compositor naming the firmware framebuffer it claimed, netd exiting \
                 rather than panicking with no NIC, soundd staying up on a null sink, sshd \
                 saying it found no netd. Nothing supervises any of them, so the message \
                 is the entire diagnostic. First of a six-test shared boot, whose tier \
                 closes upward as one unit.",
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
        ci_ms: 145_669,
        why: Why::Cost,
        guards: "That doom opened the SoundFont this tree committed, played to the end of \
                 the check, and that what it rendered reached the device. The three links \
                 src/soundfont.rs's host tests cannot make, and the three `b8b0749` broke \
                 for a cycle with the suite green.",
    },
    Relegated {
        test: "doom_sound_flood",
        ci_ms: 117_839,
        why: Why::Cost,
        guards: "doom's sound producer outrunning its audio callback without the game \
                 dying. The first domino of the T14 freeze: an `extern \"C\"` frame with \
                 no unwind path turned the overflow panic into abort, and the kernel and \
                 compositor followed it down.",
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
        test: "launcher_refusals",
        ci_ms: 76_081,
        why: Why::Cost,
        guards: "The capability architecture's own enforcement gate, and endowment landed \
                 the day before this: sixteen malformed launches at /bin/init, which is \
                 the one process the machine cannot lose, with init still launching, the \
                 kernel's live-object count unchanged, and init naming what it refused.",
    },
    Relegated {
        test: "xhci_msi_only",
        ci_ms: 35_223,
        why: Why::Cost,
        guards: "The T14's Thunderbolt controller, which printed `no MSI-X capability, \
                 using polled mode` on a real boot when there was no polled mode. Every \
                 other controller in this suite has MSI-X, so `msix=off` is the only way \
                 this branch executes at all.",
    },
    Relegated {
        test: "control_regs_negative",
        ci_ms: 40_323,
        why: Why::Cost,
        guards: "The negative control for the control-register verdict: a \
                 `no-ap-control-regs` kernel with a genuinely divergent AP, which \
                 `control_regs` has to refuse. Without it, whether the verdict would \
                 recognise a real one is answered in prose.",
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
                 SYS_DEBUG action 9 is the one read.",
    },
    Relegated {
        test: "dump_nmi_probe",
        ci_ms: 24_625,
        why: Why::Cost,
        guards: "Ctrl+Alt+D's NMI probe: a CPU that ignores a kick is named and then asked \
                 where it is with the one interrupt it cannot mask, with the rip it brings \
                 back resolved against the kernel's own symbols. On the T14 the dump named \
                 three CPUs without saying which of three causes each was.",
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
        test: "swiss_german_layout",
        ci_ms: 12_645,
        why: Why::Cost,
        guards: "Swiss German end to end, injected by physical key position — which asserts \
                 the table, the modifier levels, the ISO key and the dead-key machine at \
                 once.",
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
        test: "boot_partition_identity",
        ci_ms: 233_792,
        why: Why::Cost,
        guards: "The boot volume is selected by partition identity rather than by whatever \
                 disk enumeration happened to put first. It is the regression gate for \
                 moving the same image among differently ordered devices.",
    },
    Relegated {
        test: "kernel_heartbeat",
        ci_ms: 200_026,
        why: Why::Cost,
        guards: "After the first full mask, no CPU may disappear from two consecutive \
                 heartbeat samples, no sample gap may exceed 1 s, and at least one ran \
                 field must be nonzero. One-sample KVM scheduling blips remain allowed.",
    },
    Relegated {
        test: "metal_sim_window_drag",
        ci_ms: 164_363,
        why: Why::Cost,
        guards: "Injected pointer packets move a real compositor window by the requested \
                 coordinates. Each step depends on the prior painted position, so this is \
                 the end-to-end gate for input ordering and desktop geometry.",
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
                 either loses compatible disks or hides failed writes.",
    },
    Relegated {
        test: "hda_tone",
        ci_ms: 87_881,
        why: Why::Cost,
        guards: "soundd drives an Intel HDA stream through DMA and the host capture contains \
                 the expected tone. Host-side codec tests do not connect ring programming, \
                 interrupts, the daemon, and samples on the wire.",
    },
    Relegated {
        test: "control_regs",
        ci_ms: 83_255,
        why: Why::Cost,
        guards: "The BSP and every AP agree on the control-register state user isolation \
                 relies on. A machine can boot with one divergent AP and fail only when a \
                 task later lands there.",
    },
    Relegated {
        test: "i8042_budget_expiry",
        ci_ms: 80_146,
        why: Why::Cost,
        guards: "With the init budget deliberately spent before probe on an otherwise \
                 answering controller, the driver names which stage lacked budget, does \
                 not misreport `did not take`, and completes boot without a keyboard.",
    },
    Relegated {
        test: "short_sleep_livelock",
        ci_ms: 75_208,
        why: Why::Cost,
        guards: "Repeated sub-tick sleeps still allow runnable work to make progress. It is \
                 the dedicated machine for a scheduler livelock that would otherwise kill \
                 the shared boot and be blamed on the next binary.",
    },
    Relegated {
        test: "i8042_health_cadence",
        ci_ms: 70_975,
        why: Why::Cost,
        guards: "The keyboard controller's health report is driven by the device's own byte \
                 cadence, including a staged multi-period silence, rather than by a host \
                 timeout that can certify a starved guest.",
    },
    Relegated {
        test: "xhci_slow_connect",
        ci_ms: 64_579,
        why: Why::Cost,
        guards: "A root hub whose ports stay empty during the controller's discovery window \
                 still notices the device when it appears. This is the delayed-enumeration \
                 shape real hubs impose after reset.",
    },
    Relegated {
        test: "desktop_locale_detect",
        ci_ms: 62_293,
        why: Why::Cost,
        guards: "The locale wizard works on the compositor surface the shipped desktop uses, \
                 and its physical-key answer reaches the installed layout. Console-only \
                 coverage cannot see focus, surface input, or desktop persistence.",
    },
    Relegated {
        test: "late_storage_connect",
        ci_ms: 53_778,
        why: Why::Cost,
        guards: "A disk held absent through the boot scan is bound after it appears instead \
                 of being mistaken for a device the initial scan already owned. It is the \
                 storage-side delayed-port regression gate.",
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
        ci_ms: 38_496,
        why: Why::Cost,
        guards: "An unplug and replug collapsed inside one debounce window leaves one \
                 coherent device rather than a ghost or a lost port. The test refuses if \
                 the race was not actually staged.",
    },
    Relegated {
        test: "xhci_slot_exhaustion",
        ci_ms: 37_338,
        why: Why::Cost,
        guards: "With driver DMA device blocks clamped to one while the controller \
                 advertises more slots, excess devices are refused and slot 1 still binds \
                 as a disk.",
    },
    Relegated {
        test: "screen_paged_scrollback",
        ci_ms: 37_144,
        why: Why::Cost,
        guards: "Without input, the automatic panic pager shows the first boot line and \
                 final panic marker on different pages and exposes at least two distinct \
                 page footers. A single-page panic test cannot establish cycling.",
    },
    Relegated {
        test: "blocked_dump",
        ci_ms: 33_140,
        why: Why::Cost,
        guards: "On a live eight-CPU compositor boot, Ctrl+Alt+D produces a complete 8/8 \
                 report whose parked count equals its deadline classes, whose process \
                 census covers those tasks, and whose final verdict is present.",
    },
    Relegated {
        test: "metal_sim_input",
        ci_ms: 32_603,
        why: Why::Cost,
        guards: "The T14-shaped machine without virtio input still binds its PS/2 devices \
                 and delivers input to the shipped stack. The normal virtual-machine shape \
                 would silently take a different driver path.",
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
        test: "netd_connection_caps",
        ci_ms: 14_785,
        why: Why::Cost,
        guards: "A burst admits at least two black-hole connects up to one boundary, then \
                 answers every request at and beyond it with ResourceExhausted. Both sides \
                 refuse an implementation that rejects everything or at random.",
    },
    Relegated {
        test: "screen_console_panic",
        ci_ms: 11_103,
        why: Why::Cost,
        guards: "A panic reached through the interactive console replaces the live text \
                 surface with the fatal report. It joins input delivery to panic painting, \
                 a path neither an early panic nor an ordinary shell tests.",
    },
    Relegated {
        test: "xhci_superspeed_ports",
        ci_ms: 10_639,
        why: Why::Cost,
        guards: "SuperSpeed protocol ports are discovered and kept distinct from USB2 \
                 companion ports. A controller can enumerate ordinary devices while its \
                 USB3 half is absent or misrouted.",
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
        test: "xhci_full_speed_device",
        ci_ms: 10_088,
        why: Why::Cost,
        guards: "A full-speed device behind xHCI receives the correct slot and endpoint \
                 context rather than the high-speed defaults. High- and SuperSpeed devices \
                 do not exercise that context encoding.",
    },
    Relegated {
        test: "i8042_keyboard",
        ci_ms: 94_611,
        why: Why::Cost,
        guards: "The expected PS/2 translations and usages reach userland, selected \
                 press/release counts balance, no modifier remains stuck, and an i8042 \
                 drain is observed. It carries the two cheaper observations below on the \
                 same boot.",
    },
    Relegated {
        test: "i8042_no_spurious_wake",
        ci_ms: 5_019,
        why: Why::RidesTheBootOf("i8042_keyboard"),
        guards: "Pause supplies real bytes that decode into zero events: those drains must \
                 not wake the reader or fabricate Pause input, while interleaved A-key \
                 drains must wake and reach userland.",
    },
    Relegated {
        test: "i8042_mouse",
        ci_ms: 2_053,
        why: Why::RidesTheBootOf("i8042_keyboard"),
        guards: "Paced PS/2 mouse packets preserve button and displacement state through the \
                 merged input path. It reuses the initialized controller trace boot and \
                 cannot be selected independently without paying that boot again.",
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
    let cost: BTreeSet<&str> =
        RELEGATED.iter().filter(|r| r.why == Why::Cost).map(|r| r.test).collect();

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
        // The profile loop above already refuses a marker on a Nightly row.
        // Do not then add that sentinel to a second audio label and overflow
        // while constructing the rest of the explanation.
        if labels.iter().any(|(_, ms)| *ms == UNMEASURED_MS) {
            continue;
        }
        let measured: u64 = labels.iter().map(|(_, ms)| ms).sum();
        if row.ci_ms != measured {
            errors.push(format!(
                "{} records {} ms but the merged CI labels sum to {measured} ms",
                row.test, row.ci_ms
            ));
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
                if !cost.contains(carrier) {
                    errors.push(format!(
                        "{} rides {carrier}, but {carrier} has no current Cost row",
                        row.test
                    ));
                }
            }
            Why::Cost => {}
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
