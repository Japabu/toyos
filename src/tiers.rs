//! Which tests every `cargo test` runs, and which ones it does not — as data,
//! with what each relegation costs the tree written beside it.
//!
//! **The owner's line, 2026-08-11: the fast per-PR path runs tests taking ten
//! seconds or less.** Everything above it moves to a nightly tier, which task
//! #188 builds and which will be slower anyway because it also carries TCG and
//! the long-running audio and stress work. This module is that decision in a
//! form the nightly job can read: [`RELEGATED`] is exactly the set it has to
//! run, and `tests/toyos.rs` writes [`Tier::Nightly`] against each of those
//! names in its own registration.
//!
//! **This is interim and it is a loss.** Twenty-seven names are over the line on
//! their own and four more ride a boot with one that is; between them they are
//! 1,347.7 s of the 1,760.5 s of test time the dev host measured, and none of
//! them is gated per pull request until #188 lands. `guards` on every row says
//! what stopped being gated, because a run that quietly does less is the whole
//! failure mode here — `specs/test-cost-audit.md` §7 is the long form.
//!
//! **Nothing here is an optimisation and nothing here changes an assertion.**
//! A relegated test measures exactly what it measured; it is run later. #188
//! holds the work that would make one of these fast enough to come back.
//!
//! **The two instruments do not agree about which tests are long**, which is
//! why the ceiling is applied to one reading and the other is only asked to
//! corroborate. `target/test-durations` on the dev host and
//! `tests/test-durations` from CI's twelve KVM shards disagree by up to 15x in
//! both directions — `metal_sim_pointer_churn` is 16.2 s here and 235.2 s
//! there, `boot_partition_identity` 4.5 s here and 247.0 s there. The rows
//! below carry the dev-host number, which is what the line was set against, and
//! [`tests::the_committed_profile_corroborates_every_relegation`] holds them
//! against CI's.

use std::collections::BTreeSet;

/// The ceiling the fast tier is defined by, in milliseconds.
///
/// Policy, and the owner's. A test at exactly the line is fast: the rule he
/// stated is "ten seconds or less".
pub const FAST_CEILING_MS: u64 = 10_000;

/// Which run a registered test belongs to. Every entry of `MACHINE_TESTS` and
/// `SCREEN_TESTS` answers this or does not compile, for the same reason each
/// one answers `Sched`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Every `cargo test`.
    Fast,
    /// Only `cargo test --test toyos-build -- --nightly`, and the nightly
    /// workflow #188 builds.
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
    /// This is the collateral the record has to name: four tests here cost 4.6 s
    /// between them and go dark because a fifth costs 62.6 s.
    RidesTheBootOf(&'static str),
}

/// One test that the fast tier does not run.
pub struct Relegated {
    pub test: &'static str,
    /// What it measured, in milliseconds, on the dev host on 2026-08-11 — an
    /// M4 Pro under cross-arch TCG, two other worktrees building and booting
    /// guests. One reading of one run: `desktop_window_child` is an expected
    /// failure whose cost is the liveness ceilings it exhausts, and it has been
    /// read at 24.8 s and at 293.6 s off the same profile.
    pub dev_ms: u64,
    pub why: Why,
    /// **What stops being gated per pull request.** Not what the test does —
    /// what the tree loses while this sits in the nightly tier. The owner reads
    /// this list to decide whether the interim is acceptable, so a row that
    /// restates the test's name is a row that answers him with nothing.
    pub guards: &'static str,
}

/// Every test the fast tier does not run, newest measurement first within each
/// boot.
pub const RELEGATED: &[Relegated] = &[
    Relegated {
        test: "desktop_window_child",
        dev_ms: 293_569,
        why: Why::Cost,
        guards: "A window opened from a shell and then closed, and whether the desktop \
                 answers afterwards. The only reproduction #156 has anywhere, and the \
                 single most expensive name in the suite. Its EXPECTED_FAILURES entry is \
                 Stale::OnThisDate, so the declaration still expires on 2026-09-06 \
                 whether or not the test has run since.",
    },
    Relegated {
        test: "sshd_fail_closed",
        dev_ms: 116_052,
        why: Why::Cost,
        guards: "sshd once accepted every credential. This is the gate for that class: it \
                 mints a host identity under /home, finds no authorized_keys, \
                 authenticates nobody, and — the half a missing-file check passes without \
                 — never holds port 22 while it cannot accept a key.",
    },
    Relegated {
        test: "fpu_isolation",
        dev_ms: 94_520,
        why: Why::Cost,
        guards: "The whole user machine state surviving every exit from Ring 3, on a \
                 one-CPU machine, against a second boot of an `fpu-save-nothing` kernel \
                 that must fail the same three arms. A negative gate: without the second \
                 boot the first proves only that the machine works, which it did before \
                 the gate existed.",
    },
    Relegated {
        test: "desktop_audio_client",
        dev_ms: 80_722,
        why: Why::Cost,
        guards: "An audio client spawned by a shell inside a terminal inside the \
                 compositor — the only configuration in which all three of its \
                 descriptors are pipes to a surface, which is the T14's. A second client \
                 connecting while the first streams, and the desktop still answering \
                 after both.",
    },
    Relegated {
        test: "wall_clock_refusals",
        dev_ms: 73_749,
        why: Why::Cost,
        guards: "Every way the wall clock can fail to answer, and the century register's \
                 absence — four boots, a different kernel build each.",
    },
    Relegated {
        test: "hda_probe",
        dev_ms: 67_496,
        why: Why::Cost,
        guards: "Stage H0 of specs/hda-driver-plan.md: every branch of the probe's four \
                 questions on the machine built to present both answers to each, and the \
                 plain kernel on the same machine to show the probe stays out of an \
                 ordinary boot.",
    },
    Relegated {
        test: "screen_fatal_halt_composited",
        dev_ms: 67_363,
        why: Why::Cost,
        guards: "Whether a fatal panic can paint the panel once a compositor owns the \
                 scanout. That is the T14's only configuration and the assumption three \
                 freeze investigations rested on; no other screen test asks it.",
    },
    Relegated {
        test: "metal_sim_compositor",
        dev_ms: 62_636,
        why: Why::Cost,
        guards: "The four daemons surviving the T14's device shape, each in its own words: \
                 the compositor naming the firmware framebuffer it claimed, netd exiting \
                 rather than panicking with no NIC, soundd staying up on a null sink, sshd \
                 saying it found no netd. Nothing supervises any of them, so the message \
                 is the entire diagnostic. First of a six-test shared boot, and the reason \
                 the other five below go with it.",
    },
    Relegated {
        test: "metal_sim_compositor_stall",
        dev_ms: 13_343,
        why: Why::Cost,
        guards: "A client that stops talking, stops listening, or never stops. The guest \
                 asks whether the compositor still answers; the host asks whether it is \
                 still painting, which is the only way a livelock that answers everybody \
                 and draws nothing is visible.",
    },
    Relegated {
        test: "metal_sim_client_death",
        dev_ms: 4_014,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "A client that dies, or asks for something the kernel refuses on its \
                 behalf, costing the compositor that client and nothing else — with the \
                 desktop still painting and every dropped client named.",
    },
    Relegated {
        test: "metal_sim_window_caps",
        dev_ms: 479,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "That the window cap the compositor *derived* from total memory and the \
                 screen is the number of windows a client actually gets. A constant on \
                 both sides would agree with itself forever.",
    },
    Relegated {
        test: "metal_sim_ipc_hostile_peer",
        dev_ms: 77,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "A client that lies about its frame lengths, with the host insisting the \
                 case count the guest reports is the whole case list.",
    },
    Relegated {
        test: "metal_sim_scanout_wc",
        dev_ms: 0,
        why: Why::RidesTheBootOf("metal_sim_compositor"),
        guards: "The scanout's memory type from IA32_PAT through the MTRR combination to \
                 the mapping the compositor writes through — three parts that fail \
                 independently and none of which any page table records.",
    },
    Relegated {
        test: "doom_music",
        dev_ms: 57_510,
        why: Why::Cost,
        guards: "That doom opened the SoundFont this tree committed, played to the end of \
                 the check, and that what it rendered reached the device. The three links \
                 src/soundfont.rs's host tests cannot make, and the three `b8b0749` broke \
                 for a cycle with the suite green.",
    },
    Relegated {
        test: "doom_sound_flood",
        dev_ms: 56_612,
        why: Why::Cost,
        guards: "doom's sound producer outrunning its audio callback without the game \
                 dying. The first domino of the T14 freeze: an `extern \"C\"` frame with \
                 no unwind path turned the overflow panic into abort, and the kernel and \
                 compositor followed it down.",
    },
    Relegated {
        test: "screen_console_scroll",
        dev_ms: 49_620,
        why: Why::Cost,
        guards: "Every row of the panel, character for character, after a workload built \
                 to leave stale glyphs behind a scroll. #90 was the owner seeing prior \
                 text survive in the middle of a cleared screen.",
    },
    Relegated {
        test: "launcher_refusals",
        dev_ms: 46_261,
        why: Why::Cost,
        guards: "The capability architecture's own enforcement gate, and endowment landed \
                 the day before this: sixteen malformed launches at /bin/init, which is \
                 the one process the machine cannot lose, with init still launching, the \
                 kernel's live-object count unchanged, and init naming what it refused.",
    },
    Relegated {
        test: "xhci_msi_only",
        dev_ms: 37_480,
        why: Why::Cost,
        guards: "The T14's Thunderbolt controller, which printed `no MSI-X capability, \
                 using polled mode` on a real boot when there was no polled mode. Every \
                 other controller in this suite has MSI-X, so `msix=off` is the only way \
                 this branch executes at all.",
    },
    Relegated {
        test: "control_regs_negative",
        dev_ms: 36_415,
        why: Why::Cost,
        guards: "The negative control for the control-register verdict: a \
                 `no-ap-control-regs` kernel with a genuinely divergent AP, which \
                 `control_regs` has to refuse. Without it, whether the verdict would \
                 recognise a real one is answered in prose.",
    },
    Relegated {
        test: "desktop_typing_damage",
        dev_ms: 22_971,
        why: Why::Cost,
        guards: "What one typed character costs the desktop, read off the compositor's own \
                 damage_px_max. A repainted window is 89% of the panel and the clock's \
                 readout is 0.46%; the gate at 2% sits between them by forty either way.",
    },
    Relegated {
        test: "idle_stack_guard",
        dev_ms: 22_779,
        why: Why::Cost,
        guards: "The guard page under every per-CPU idle stack. Its absence is invisible \
                 to every log line and every screendump — an overflow rewrote whatever the \
                 allocator had put underneath — so the only way to ask is to touch it, and \
                 SYS_DEBUG action 9 is the one read.",
    },
    Relegated {
        test: "dump_nmi_probe",
        dev_ms: 22_400,
        why: Why::Cost,
        guards: "Ctrl+Alt+D's NMI probe: a CPU that ignores a kick is named and then asked \
                 where it is with the one interrupt it cannot mask, with the rip it brings \
                 back resolved against the kernel's own symbols. On the T14 the dump named \
                 three CPUs without saying which of three causes each was.",
    },
    Relegated {
        test: "metal_sim_pointer_churn",
        dev_ms: 16_185,
        why: Why::Cost,
        guards: "Eight plug-and-pull cycles of a pointer under a compositor holding the \
                 merged pointer's fd across all of them. The owner froze his desktop this \
                 way twice, on the fourth cycle's enumeration.",
    },
    Relegated {
        test: "toybox_cp_volume",
        dev_ms: 16_082,
        why: Why::Cost,
        guards: "The real /bin/cp against a FAT32 volume sized from what the volume says it \
                 has left, including the case where it fills.",
    },
    Relegated {
        test: "usb_boot_stick_pulled",
        dev_ms: 15_552,
        why: Why::Cost,
        guards: "device_del on the stick carrying /boot and /log while the desktop draws — \
                 #152's only instrument. The failure has no other witness: /log dies with \
                 the event, the machine has no serial port, it is not a panic, and \
                 Ctrl+Alt+D answers nothing.",
    },
    Relegated {
        test: "screen_pager_keys",
        dev_ms: 14_588,
        why: Why::Cost,
        guards: "PageUp and PageDown reaching the panic console's pager off the i8042 with \
                 every CPU stopped. The decode is host-tested in toyos-ps2; that a \
                 keystroke reaches a machine which has stopped scheduling is only \
                 answerable here.",
    },
    Relegated {
        test: "xhci_deaf_registers",
        dev_ms: 14_249,
        why: Why::Cost,
        guards: "A controller and a port that stop answering. Five register spins had no \
                 deadline at all — port reset, halt, HCRST, CNR, R/S — which on the T14 is \
                 `Boot: peripherals ready` on the panel forever and nothing else.",
    },
    Relegated {
        test: "hda_client_stall",
        dev_ms: 13_679,
        why: Why::Cost,
        guards: "A client that stops producing mid-stream, on a cyclic HDA ring and on a \
                 virtio queue, which must answer differently — underruns against deferred. \
                 Asserting both is what refuses the two obvious wrong fixes.",
    },
    Relegated {
        test: "xhci_hid_break",
        dev_ms: 10_563,
        why: Why::Cost,
        guards: "A HID interrupt endpoint completing with a code the driver dropped where \
                 it read it, at the fourth completion and at the first. A Logitech mouse \
                 on the T14 went silent for the rest of the boot with every bind-time line \
                 reading perfectly.",
    },
    Relegated {
        test: "swiss_german_layout",
        dev_ms: 10_475,
        why: Why::Cost,
        guards: "Swiss German end to end, injected by physical key position — which asserts \
                 the table, the modifier levels, the ISO key and the dead-key machine at \
                 once.",
    },
    Relegated {
        test: "iommu_discovery",
        dev_ms: 10_221,
        why: Why::Cost,
        guards: "Four machines whose remapping units differ in exactly one advertised \
                 capability each, and whether the kernel's decode moves with them. A \
                 plausible constant satisfies any single-machine assertion.",
    },
];

/// The names [`RELEGATED`] holds, which is what `tests/toyos.rs` checks its own
/// registration against.
pub fn relegated_names() -> BTreeSet<&'static str> {
    RELEGATED.iter().map(|r| r.test).collect()
}

/// What the fast tier stopped paying for, in milliseconds — added up from the
/// rows rather than written down, so the figure the suite prints cannot drift
/// from the list it is a figure about.
pub fn relegated_ms() -> u64 {
    RELEGATED.iter().map(|r| r.dev_ms).sum()
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

    /// The declaration against itself: a row that contradicts the line it cites
    /// is folklore with a number beside it.
    #[test]
    fn every_row_is_on_the_side_of_the_line_it_claims() {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let over: BTreeSet<&str> =
            RELEGATED.iter().filter(|r| r.why == Why::Cost).map(|r| r.test).collect();
        for r in RELEGATED {
            assert!(seen.insert(r.test), "{} has two rows", r.test);
            assert!(!r.guards.is_empty(), "{} says nothing about what it guards", r.test);
            match r.why {
                Why::Cost => assert!(
                    r.dev_ms > FAST_CEILING_MS,
                    "{} is relegated for its cost and measured {} ms, at or under the \
                     {FAST_CEILING_MS} ms line — it belongs in the fast tier",
                    r.test,
                    r.dev_ms
                ),
                Why::RidesTheBootOf(carrier) => {
                    assert!(
                        r.dev_ms <= FAST_CEILING_MS,
                        "{} measured {} ms, over the line on its own — it is relegated by \
                         Why::Cost and not by riding {carrier}'s boot",
                        r.test,
                        r.dev_ms
                    );
                    assert!(
                        over.contains(carrier),
                        "{} rides {carrier}'s boot and {carrier} is not relegated for its \
                         own cost, so nothing here explains why {} is not run",
                        r.test,
                        r.test
                    );
                }
            }
        }
    }

    /// **The other instrument, asked to corroborate and not to decide.** The
    /// ceiling is applied to the dev host's reading; this holds every relegation
    /// against CI's twelve KVM shards, where a name the fast tier could have back
    /// shows up as a row CI also prices under the line.
    ///
    /// One direction only, and deliberately. CI prices 44 further names above
    /// ten seconds that the dev host runs in under three — `boot_partition_identity`
    /// at 247.0 s against 4.5 s — so "every slow name on CI is relegated" would be
    /// a different suite's line, not this one's. A name the committed profile has
    /// never seen is skipped: `launcher_refusals` is newer than the last merge
    /// that wrote it.
    #[test]
    fn the_committed_profile_corroborates_every_relegation() {
        let ci = committed_profile();
        let mut disagree: Vec<String> = Vec::new();
        for r in RELEGATED.iter().filter(|r| r.why == Why::Cost) {
            let Some(&ms) = ci.get(r.test) else { continue };
            if ms <= FAST_CEILING_MS {
                disagree.push(format!(
                    "{}: {} ms on the dev host, {ms} ms on CI — CI now runs it inside the \
                     {FAST_CEILING_MS} ms line",
                    r.test, r.dev_ms
                ));
            }
        }
        assert!(
            disagree.is_empty(),
            "the two profiles disagree about which side of the line these are on. Take a \
             fresh dev-host reading; if it is under the line too, delete the row and put \
             Tier::Fast back in tests/toyos.rs:\n  {}",
            disagree.join("\n  ")
        );
    }
}
