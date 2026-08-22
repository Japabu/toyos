mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::qemu::{
    self, await_guest, await_marker, await_marker_new, BootOptions, QemuInstance, TestResult,
    STALLED,
};
use common::{audio, compile, faults, hostload, screen, serial, stats, storage, usb};
use toyos_build::testargs::Shard;
use toyos_build::tiers::{self, Tier};

struct TestDef {
    name: String,
    qemu_name: String,
    timeout: Duration,
    check: fn(&TestResult) -> bool,
    /// What the test's window is still owed when the guest's exit closed it.
    ///
    /// A capture ends at `===TEST_END===`, which is the test process exiting —
    /// and a daemon reporting *on* that exit writes its line afterwards. So a
    /// check that counts such lines is counting over a window its own subject
    /// closes, and the last one loses the race
    /// (`null_sink_client_exits`, PR #85 and PR #94). This runs between the
    /// test and its check, with the guest still up, and waits on the guest's
    /// own liveness — never on a span of host time. [`no_settle`] is the
    /// default and costs nothing.
    settle: fn(&mut QemuInstance, &mut TestResult),
}

/// Whether a test may run while other guests are up.
///
/// Every entry of [`MACHINE_TESTS`] and [`SCREEN_TESTS`] answers this or does
/// not compile. That is the serial-by-default rule
/// in its stronger form: the rule's whole safety argument is that *forgetting*
/// must cost a slow suite rather than a wrong measurement, and a name that
/// cannot be added without an answer cannot be forgotten at all.
///
/// **Where the answer is not known it is [`Sched::Serial`].** A wrong `Parallel`
/// is a test measuring a machine it does not have to itself, and neither the
/// suite nor the agent reading its red can tell that from a real defect.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sched {
    /// May run beside other guests. Its assertions hold on a host running as
    /// many QEMUs as the width allows.
    Parallel,
    /// Runs in the serial tail: one guest on the host, after the parallel phase
    /// has drained. Everything with a wall-clock margin on either clock, a
    /// debounce window staged from the host, or a rate.
    Serial,
}

/// The width with no `--jobs`, and where it came from.
///
/// 14 cores and about three host threads a guest divides out to four. The suite
/// says twelve. Alternated in one session on a quiet host, 246 tests, both
/// green: **125.6 s wide eight against 109.1 s wide twelve**, with the parallel
/// phase at 58.3 s against 42.1 s — the same 16 s and the same direction as the
/// pair taken on the tree six commits earlier. A guest here is mostly
/// *waiting* — for a marker, for a debounce, for a device — which is why this is
/// a measurement and not a division.
///
/// **Twelve is the number for one suite on this host**, and [`HostSlots`] is
/// what stops four agents at twelve being 48 guests on 14 cores.
/// An earlier table said eight; it was taken while `drain_serial` was still
/// width-scaled and
/// `metal_sim_pointer_churn`'s twenty-four paced drains *were* the phase.
const DEFAULT_WIDTH: usize = 12;

/// This run's claim on the host's guest budget.
///
/// [`DEFAULT_WIDTH`] is a number for *one* suite, and nothing was handing out
/// the cores that two suites both spend.
/// A second suite on this machine is not a slower first suite, it
/// is a wrong one: `screen_fatal_halt` red at 11 s against 3.3 s alone, and an
/// agent's hour spent chasing that as a regression.
///
/// **One slot per task, never per boot.** A worker holds at most one and never
/// waits for a second while holding one, which is what makes the semaphore
/// deadlock-free rather than lucky: several tests hold two guests at once, and a
/// slot each would let twelve workers each hold one and each wait for another.
///
/// The wait sits outside the task, so it lands in the phase's wall clock and in
/// no test's duration — a `PASS` time, and the profile [`longest_first`] orders
/// on, both stay measurements of the test rather than of the queue.
struct HostSlots {
    root: std::path::PathBuf,
    /// The name this run answers to in another run's waiting message. A pid
    /// alone is not enough to act on: an agent needs to know which worktree.
    label: String,
    /// Zero is the semaphore off. It is the only way to measure a suite against
    /// one that has it, which is what `--host-slots 0` is for.
    budget: usize,
}

impl HostSlots {
    fn take(&self, what: &str) -> Option<toyos_build::buildlock::Guard> {
        let budget = self.budget;
        (budget > 0)
            .then(|| toyos_build::buildlock::guest_slot(&self.root, budget, &format!("{}: {what}", self.label)))
    }
}

/// Which tier the shared boot's 153 binaries are in.
///
/// [`Tier::Fast`] because every member in the effective CI profile is at or
/// under `toyos_build::tiers::FAST_COMMIT_MS`. [`check_no_collisions`] refuses
/// a Fast shared member with no current measurement or one priced without
/// margin, so a newly discovered binary starts conservative instead of
/// inheriting this answer silently. Declared beside [`SHARED_BLOCK`] rather
/// than assumed, for the same reason that is declared.
const SHARED_TIER: Tier = Tier::Fast;

/// The one boot that carries every Rust and C test.
///
/// Declared here for the same reason each list entry is: it is a scheduling
/// answer and it has to be visible.
///
/// **Parallel, once its ceilings stopped being host-wide.** It was moved to the
/// tail because at width 4 `allocator_stress` went from 1 s to past its 5 s and
/// `demand_paging_sse` past its — but not one of those numbers is an assertion.
/// They are liveness guards on a guest that might wedge, and the verdict in
/// every case is the exit code and the expected stdout. [`qemu::budget`] now
/// pays them out per guest the phase may have up, which is what
/// `wait_for_ready`'s boot timeout has done since the phase existed, so the
/// number each author reasoned about is still the number for one guest.
///
/// What that leaves is a block of 153 tests on one boot costing about thirteen
/// seconds between them, which is far too little to be worth a tail slot of its
/// own: alone it is thirteen seconds nothing overlaps, and in the phase it is
/// one task among sixty.
const SHARED_BLOCK: Sched = Sched::Parallel;

/// The shared-boot binaries that call `SYS_DEBUG`, and so cannot run on the
/// kernel an image ships.
///
/// **Everything else on the shared boot now runs on that kernel** — no features,
/// the same staged artifact `cargo run --build-only` writes. Until
/// `test-actuators` became one name, `qemu::fold_inert` put it on *every* test
/// kernel, so nothing in this suite had ever booted the shipping binary and two
/// of the three names below depended on that without saying so.
///
/// A second boot rather than a second image: the block costs a fraction of a
/// second of guest time between its members, and what these need is a syscall
/// number the other 150 must not have.
const ACTUATOR_TESTS: &[&str] = &[
    // Actions 0, 1 and 2: a kernel `panic!`, a null read in kernel context, and
    // a spinlock held across a scheduler entry. Each kills the caller and the
    // machine has to survive it, which is the whole verdict.
    "panic_recovery",
    // Actions 10 and 11: the address of sixteen bytes of kernel memory and
    // whether they still hold what the kernel put there. A guest cannot read the
    // kernel's address space, so without them a kernel that still made the write
    // under test answers a userland that cannot notice.
    "abuse_kernel_addr",
    // Actions 12 and 13: hold one CPU's shootdown acknowledgement back, so that
    // whether the initiator waits becomes a duration userland can read.
    "tlb_shootdown_waits",
    // Action 16, the live-object census per kind, and 17 and 18 for the idle
    // stack the deferred release path runs on. A leak is two readings and a
    // comparison, so on a kernel that answers `InvalidArgument` both readings
    // are the same error and the assertion passes having counted nothing.
    "handle_basic",
    "handle_kill_policy",
    "handle_transfer",
];

/// What [`ACTUATOR_TESTS`] boots: the one kernel that carries `SYS_DEBUG`, with
/// no actuator armed in it.
const ACTUATOR_KERNEL: &[&str] = toyos_build::build::TEST_KERNEL;

/// How many times a shared block will answer a dead guest with a new one.
///
/// Bounded because a block whose every member kills the guest must not boot one
/// per test; three because the failure it exists for is one test taking the boot
/// down, and a block that does it four times has a different problem.
const MAX_SHARED_REBOOTS: usize = 3;

// Rust helper binaries that are spawned by tests, not tests themselves.
const RUST_SKIP: &[&str] = &[
    // Its verdict is a property of the *console capture*, which only a boot of
    // its own can hold: in the shared boot every other binary's output is in the
    // same stream. `console_line_atomicity` runs it.
    "console_line_atomicity",
    "segfault_child",
    "disk_backtrace_child",
    "fault_gate_child",
    "test_panic_child",
    "i8042_keyboard",
    "i8042_mouse",
    "input_events",
    "va_exhaustion",
    // Needs SYS_DEBUG, which the shipping kernel has no arm of at all.
    // `heap_ceiling_recovery` boots the `test-actuators` kernel on one CPU,
    // which is also what makes its claim about *the recovered CPU* precise.
    "heap_ceiling",
    // Fills /tmp to the VFS listing limit, so it needs a boot nothing else
    // shares — every later `read_dir("/tmp")` in it would be refused.
    // `readdir_bound` gives it one.
    "readdir_bound",
    // Needs a live compositor, which `tests/testcases` does not boot.
    // `metal_sim_window_caps` runs it on the config that does.
    "window_caps",
    // Same reason, same config: `metal_sim_ipc_hostile_peer` runs it.
    "ipc_hostile_peer",
    // Same again: `metal_sim_compositor_stall` runs it.
    "compositor_stall",
    // Same again, and it spawns copies of itself as clients that die:
    // `metal_sim_client_death` runs it.
    "compositor_client_death",
    // Same again, and it also needs a host injecting pointer packets:
    // `metal_sim_window_drag` runs it.
    "window_drag",
    // Needs a compositor, a terminal and a shell: `desktop_window_child`
    // launches it from that shell.
    "window_child",
    // Its two spawning arms only mean anything when the two processes share a
    // CPU, and the shared boot has two. `fpu_isolation` gives it a machine with
    // one — and a second boot on the kernel that saves nothing, which is the
    // only thing that proves the arms have teeth.
    "fpu_isolation",
    // Needs netd with a NIC. `netd_connection_caps` runs it on tests/netcase.
    "netd_caps",
    // Same reason, same config: `netd_hostile_peer` runs it there.
    "netd_hostile_peer",
    // Needs a `launcher` connector, which `tests/testcases`'s test-runner has
    // no reason to hold. `launcher_refusals` runs it on tests/netcase, whose
    // test-runner receives one for exactly this.
    "launcher_refusals",
    // Needs a boot image the harness staged a file into before the machine
    // started, which only `esp_filesystem` builds.
    "esp_files",
    // Every question it asks has the *right* answer on an ordinary kernel, so
    // on the shared boot it prints three successes and passes on its exit code
    // — a second test of the same name whose verdict is vacuous.
    // `boot_volume_metadata_error` runs it on the kernel that refuses the
    // reads, which is the only build it says anything about.
    "boot_volume_metadata_error",
    // Two modes, each waiting to be typed at through QMP; on its own nothing
    // ever answers it. `swiss_german_layout`, `locale_detect` and
    // `locale_detect_unrecognized` drive it.
    "locale_gate",
    // A victim, not a test: it spins on `SYS_GETPID` so that another CPU's NMIs
    // have somewhere to land, and on its own it asserts nothing and costs ten
    // seconds. `syscall_window_nmi` runs it on the kernel that storms it.
    "nmi_window_spin",
    // Driven, not run: `screen_console_clear` types its name at a console it is
    // watching, and on its own it asks the kernel to paint over a panel nobody
    // is reading and exits 0. A verdict its own exit code cannot carry — the
    // same shape as `test_screen_churn` below, and it was in the shared registry
    // for the same reason nobody had looked.
    "test_screen_graffiti",
    // A workload, not a test: it prints a pattern for `screen_console_scroll`
    // to assert a panel against, and on its own it has no verdict at all. It
    // used to sit in the shared boot with defaults for its arguments, where it
    // printed four hundred lines to a console nothing was reading and passed
    // on its exit code.
    "test_screen_churn",
    // Spawns `/bin/doom`, which `tests/testcases` does not carry — doom is
    // 4 MiB and every other test boots that config. `doom_sound_flood` runs it
    // on `tests/doomcase`.
    "doom_sound_flood",
    // Same, plus the WAD and the SoundFont doom's music is made of, which no
    // other config should pay 19 MiB of initrd for. `doom_music` runs it on
    // `tests/doommusiccase`.
    "doom_music",
    // Its failure mode is a CPU that never runs anything again, so on the
    // shared boot it would be reported against whichever test came next — and
    // every one after that. `short_sleep_livelock` gives it a boot of its own.
    "abuse_short_sleep",
    // The four below were **running twice under one name**, once here on the
    // plain boot and once as the test that owns the name, and the collision was
    // invisible: `check_registration` compared the three declared lists against
    // each other and never against the binaries the registry discovers.
    // `check_no_collisions` closes that, and this is what it found.
    //
    // Two verdicts under one name is not extra coverage, it is a name that
    // cannot be read: `retry_task` searches the shared registry first, so a
    // machine test of one of these that failed wide was re-run *as the shared
    // binary* and its `ALONE:` line was about a different test. What the shared
    // copy adds is the binary exiting 0 on a boot that gives it nothing to
    // measure — `cache_eviction` in 132 ms against the 22.5 s its own device
    // shape costs (run `31247206462`).
    //
    // `cache_eviction` needs the small NVMe that makes the cache evict at all.
    "cache_eviction",
    // Needs an HDA controller, which `tests/testcases` has none of.
    "hda_client_stall",
    // Gate A's two, whose verdict is the wav the device captured — which the
    // shared boot takes no capture of. The comment below has claimed since it
    // was written that they are excluded from this boot; now they are.
    "audio_tone",
    "audio_tone_load",
];

// Audio glitch tests. Each runs in its own QEMU boot per SMP config and
// asserts on the wav the virtio-sound device captured, so they are excluded
// from the shared multi-test boot.
const AUDIO_TESTS: &[(&str, Tier)] =
    &[("audio_tone", Tier::Nightly), ("audio_tone_load", Tier::Nightly)];

// Scheduler-core gate A covers both SMP configs: smp=1 is the audio spec's
// first-class single-CPU case, smp=8 the full-SMP case.
const AUDIO_SMP: &[u32] = &[1, 8];

// Tests that read a decoded screendump, which is exactly the set for which
// the screen is the device under test: the panic console. On a machine with
// no serial port the rendered report is the only diagnostic that exists, so
// asserting on pixels there is asserting on the product. Everything else that
// used to read a screendump now reads the console instead — a screenshot is a
// poor way to ask "did the right process come up", and thresholds over a live
// desktop are how those tests passed vacuously twice.
// `screen_decoder` needs no guest at all; it proves the decoder against a
// bitmap it rendered itself, before anything points it at a real screen.
/// The order was once about kernel rebuilds — every actuator was a build, and a
/// feature-carrying test last left the plain-kernel ones above it untouched by
/// the thrash. There are two kernels now and nothing to thrash; the order is
/// kept because these are read the way they are
/// written.
const SCREEN_TESTS: &[(&str, Sched, Tier)] = &[
    ("screen_decoder", Sched::Parallel, Tier::Fast),
    // `thread::sleep(5 s)` is the measurement, not a ceiling: the assertion is
    // literally that the log is still on the panel five seconds after the boot
    // finished, so a 2x slower machine changes nothing about the wait but the
    // wait is the verdict either way — timer-anchored.
    ("screen_diag_boot", Sched::Parallel, Tier::Nightly),
    ("screen_log_absent", Sched::Parallel, Tier::Fast),
    ("screen_console_shell", Sched::Parallel, Tier::Fast),
    ("screen_console_clear", Sched::Parallel, Tier::Fast),
    ("screen_console_scroll", Sched::Parallel, Tier::Nightly),
    ("screen_i8042_health", Sched::Parallel, Tier::Fast),
    // Ctrl+Alt+D with no console at all: the panel is the whole channel, and a
    // compositor is holding it. A fixed 2 s settle sits inside the dump's own
    // guest-timed 15 s hold, and the verdict is whether the report survived
    // the desktop's next repaint — which only where that wait lands decides,
    // so it is timer-anchored despite being a screendump-content check.
    ("screen_blocked_dump", Sched::Parallel, Tier::Nightly),
    ("screen_recoverable_untouched", Sched::Parallel, Tier::Fast),
    ("screen_early_panic", Sched::Parallel, Tier::Fast),
    ("screen_late_panic", Sched::Parallel, Tier::Fast),
    ("screen_paged_scrollback", Sched::Parallel, Tier::Nightly),
    ("screen_panic_muted", Sched::Parallel, Tier::Fast),
    ("screen_console_panic", Sched::Parallel, Tier::Fast),
    ("screen_fatal_halt", Sched::Parallel, Tier::Fast),
    // The same fatal path with a compositor holding the panel, which is the
    // only configuration the owner's laptop is ever in and the one no screen
    // test covered: `screen_fatal_halt` boots a config with no compositor, and
    // `screen_blocked_dump` has one but paints through `paint_report` rather
    // than through `halt_all_cpus`.
    ("screen_fatal_halt_composited", Sched::Parallel, Tier::Nightly),
    ("screen_pager_keys", Sched::Serial, Tier::Nightly),
];

/// What `screen_console_shell` types, and what it then looks for on its own.
///
/// The command's *output* differs from the command, which is the whole point:
/// the shell echoes what is typed, so an assertion satisfiable by the echo says
/// only that the console drew a key, not that anything ran. This is asserted as
/// a whole trimmed row, so the echoed `/home/root> echo zqjxk` cannot satisfy
/// it either.
const CONSOLE_NONCE: &str = "zqjxk";
/// `/bin/shell` cds to `$HOME` before its first prompt, and prints
/// `"{cwd}> "` — without the trailing space, which the decoder trims off the
/// end of every row.
const CONSOLE_PROMPT: &str = "/home/root>";

/// What `SYS_DEBUG` action 8 paints. Green, because the decoder thresholds on
/// the brightest channel and a colour a glyph could contain would let a
/// surviving pixel read as text rather than as itself.
const GRAFFITI: [u8; 3] = [0x00, 0xC0, 0x00];

/// Tests whose machine shape *is* the test: metal-sim, where the PS/2
/// keyboard is the only input source and no virtio device exists, or a q35
/// with the i8042 switched off. None of them can share the multi-test boot,
/// so each costs its own. `run_machine_test` dispatches them.
/// Feature-carrying ones last, as SCREEN_TESTS does: each distinct kernel
/// feature set is another kernel rebuild.
///
/// A few adjacent runs of names share *one* boot between them — see
/// [`group_boot`], which is what makes adjacency here load-bearing rather than
/// tidy.
const MACHINE_TESTS: &[(&str, Sched, Tier)] = &[
    ("ioapic_topology", Sched::Parallel, Tier::Fast),
    ("control_regs", Sched::Parallel, Tier::Fast),
    ("control_regs_negative", Sched::Parallel, Tier::Fast),
    ("input_merge", Sched::Parallel, Tier::Fast),
    ("metal_sim_input", Sched::Parallel, Tier::Fast),
    // One boot from here to `metal_sim_compositor_stall` (`METAL_SIM_DESKTOP`).
    ("metal_sim_compositor", Sched::Parallel, Tier::Nightly),
    // Reads the boot log this group already has, after the member above has
    // drained it. Text only, no clock in the verdict.
    ("metal_sim_scanout_wc", Sched::Parallel, Tier::Nightly),
    ("metal_sim_window_caps", Sched::Parallel, Tier::Nightly),
    ("metal_sim_ipc_hostile_peer", Sched::Parallel, Tier::Nightly),
    ("metal_sim_compositor_stall", Sched::Parallel, Tier::Nightly),
    // Last of the group: it drops clients on purpose and its verdict is that
    // the desktop outlived every one of them.
    ("metal_sim_client_death", Sched::Parallel, Tier::Nightly),
    // A thousand pointer packets paced from the host, and not one assertion on
    // when any of them arrived: the settles are 400 ms against a driver that
    // acts in microseconds, both liveness loops run to 20 s, and the three
    // verdicts are a count of bound sources, a frame batch above the taskbar's
    // two, and a desktop still painting afterwards.
    ("metal_sim_pointer_churn", Sched::Parallel, Tier::Nightly),
    // A window dragged by injected pointer packets, and the exact opposite of
    // the churn above on the one question that decides this: here each packet's
    // effect has to be on screen before the next is sent. The press that starts
    // the drag must land on a title bar the previous motion put under the
    // cursor, and the drag's displacement is read back as a coordinate — so a
    // guest one batch behind aims at the content instead, which is a different
    // verdict rather than a slower one. Watched to happen, on a compositor made
    // slow on purpose. Its own boot too: it leaves the pointer somewhere else
    // and the window in a different place than it found them.
    ("metal_sim_window_drag", Sched::Serial, Tier::Nightly),
    // A host-measured drain rate with an 8 s ceiling on a 3.3 s expectation.
    // Not gate A, but the same instrument: what it measures is how fast a
    // client's audio leaves the machine.
    ("metal_sim_null_audio", Sched::Serial, Tier::Nightly),
    ("null_sink_shipped_client", Sched::Serial, Tier::Nightly),
    // Parallel, and this one is argued rather than assumed: not a verdict in it
    // is a wall-clock margin. The flood's size is asserted against the audio
    // callback's own period counter standing still, both playback checks are
    // counted in periods, and the capture is read for amplitude and never for
    // timing. Its own boot, its own config, and the only client its soundd has.
    ("doom_sound_flood", Sched::Parallel, Tier::Nightly),
    // Reads a device capture and requires at least MIN_SIGNAL_SECS = 0.8 s of
    // it to carry signal at peak >= 6000 — an absolute seconds-of-signal
    // floor on audio recorded in real time, not a fraction of the capture and
    // not compute-bound: timer-anchored, and Nightly for that reason.
    ("doom_music", Sched::Parallel, Tier::Nightly),
    ("netd_connection_caps", Sched::Parallel, Tier::Fast),
    // Its own boot with a NIC under it, because sshd leaves at the bind on
    // every other config. Every verdict is a line of text; no clock in any.
    ("sshd_fail_closed", Sched::Parallel, Tier::Fast),
    // Serial: it measures netd's 2 s handshake deadline against the host's
    // clock, and counts how many connections survived a 48 ms paced burst
    // before that deadline could expire any of them. Both are wall-clock
    // margins, which is the definition of [`Sched::Serial`].
    ("netd_hostile_peer", Sched::Serial, Tier::Nightly),
    ("launcher_refusals", Sched::Parallel, Tier::Fast),
    ("foreign_disk_untouched", Sched::Parallel, Tier::Fast),
    ("boot_partition_identity", Sched::Parallel, Tier::Fast),
    ("double_fault_stack", Sched::Parallel, Tier::Fast),
    // Three boots of its own, ten seconds of Ring 3 spinning each, and every
    // verdict is a count the kernel printed or a line it printed: how many NMIs
    // landed at CPL 0 with a user `rsp`, against how many landed in Ring 3, and
    // both come off the same storm. No host clock is in any of it — the
    // ten seconds are how long the victim spins, not a margin anything is
    // measured against — so Parallel. Carrying `UNMEASURED_MS` until the shards
    // price it.
    ("syscall_window_nmi", Sched::Parallel, Tier::Fast),
    // Its own boot, its own feature, and it drives the guest only through
    // stdin — nothing it touches is shared with another test. Returned to
    // Fast on 2026-08-21: the 2026-08-17 drain fix took it from 52,822 ms to
    // a measured 5,049 ms on KVM (nightly run 32444411794), exactly the
    // crossing its relegation record said the next nightly would decide.
    ("idle_stack_guard", Sched::Parallel, Tier::Fast),
    // Its own boot and its own feature, and it deafens one CPU for 400 ms —
    // but the deafening is a *window*, and the verdict is whether the NMI is
    // answered inside `NMI_BUDGET_NS`, which is one millisecond. That is a
    // wall-clock margin on the host as much as on the guest: at width 12 the
    // probe missed the window and reported the NMI as never delivered, which
    // reads exactly like the defect it hunts, and it was green alone in the
    // same run and three times after it. Serial by the default rule — a
    // verdict that is a duration does not go in the parallel phase. Returned
    // to Fast on 2026-08-21: the 2026-08-17 drain fix took it from 24,625 ms
    // to a measured 6,284 ms on KVM (nightly run 32444411794), the return its
    // relegation record called the likeliest in the table.
    ("dump_nmi_probe", Sched::Serial, Tier::Fast),
    ("diskless_boot", Sched::Parallel, Tier::Fast),
    // Every verdict is a line of text or a device property, and no clock is in
    // any of them.
    ("virtio_net_no_msix", Sched::Parallel, Tier::Fast),
    // One boot, and its verdict is a line the kernel printed before any device
    // was brought up. No clock and no device in it.
    ("virtio_used_ring", Sched::Parallel, Tier::Fast),
    ("xhci_many_devices", Sched::Parallel, Tier::Fast),
    // Its whole assertion is that a keystroke injected from the host crossed a
    // USB keyboard on the *second* controller, and `input_events_run` sends
    // each one only after the guest has printed the last — so a key the host
    // never got to send is a stall it names, and never a key the driver lost.
    ("xhci_second_controller", Sched::Parallel, Tier::Fast),
    ("xhci_two_controllers", Sched::Parallel, Tier::Fast),
    // **Returned 2026-08-17**, on the same `input_events_run` the two names
    // above it run: it had `xhci_second_controller`'s sequence written out again
    // on fixed sleeps, and nothing sent the right-button release
    // `test_rs_input_events` exits on — so 30 s of its 35.2 s CI price was a
    // client waiting out a fallback deadline with every assertion already
    // satisfied. Carrying `UNMEASURED_MS` until the shards price it.
    ("xhci_msi_only", Sched::Parallel, Tier::Fast),
    ("xhci_no_interrupt", Sched::Parallel, Tier::Fast),
    ("nvme_large_device", Sched::Parallel, Tier::Fast),
    ("nvme_wide_sector", Sched::Parallel, Tier::Fast),
    ("iommu_discovery", Sched::Parallel, Tier::Nightly),
    ("readdir_bound", Sched::Parallel, Tier::Fast),
    // Two boots, and the verdict is that they answer differently. Nothing in it
    // is timed: every arm is a process exit code or a byte comparison — still
    // compute-bound, still Nightly: 11,075 ms in the sweep's final shard
    // packing (run 31705986758) is a Cost row, the same shape
    // `desktop_window_child` carries, not a reclassification.
    ("fpu_isolation", Sched::Parallel, Tier::Nightly),
    // The fourth declared kernel build, booted so that the scheduler core's
    // `feature = "check"` instruments are compiled and executed by a CI run at
    // all. One of its verdicts is a *quantile* of the guest's published
    // pass-cost distribution, which is wall clock across a scheduler pass.
    //
    // **Serial, and it used to say `Parallel` for a reason that was wrong.**
    // The old note read "a bound the guest measures against its own TSC inside
    // a single scheduler pass, which no amount of host load lengthens. A pass
    // is preempt-off by construction." Preempt-off stops the *guest's*
    // scheduler and stops nothing above it: the guest's TSC advances while the
    // host has the vCPU, which is why invariant P panicked on a KVM shard at
    // 200569 ns and why it is a measurement now. Measured here, 2026-08-17, one
    // suite: alone on a quiet host (1.02x the reference boot) cpu0 reports
    // `168 passes, p50 < 16384 ns, p90 < 131072 ns` and passes; in the same
    // run's 12-wide phase, `134 passes, p50 < 131072 ns, p90 < 262144 ns` and
    // it reds. Host contention moves this guest's median by a factor of eight,
    // which is the definition of a test that must have the machine to itself.
    ("sched_check_build", Sched::Serial, Tier::Fast),
    // What nesting a `scheduler::Operation` may and may not do, which is a law
    // with no host-side reader: the type reaches `percpu::cpu_id` and
    // `driver::current_handle`, so nothing outside a booted machine can
    // construct one. Parallel, and nothing in it is a duration — every verdict
    // is a comparison between two numbers the kernel printed, both of them
    // offsets it chose itself.
    ("operation_nesting", Sched::Parallel, Tier::Fast),
    ("short_sleep_livelock", Sched::Parallel, Tier::Fast),
    // The kernel thread and the row that says what its panic means. Two
    // boots, both headless: the second one halts on purpose — and the pair
    // measured 11.8 s on CI KVM, over the fast line, so the whole verdict is
    // nightly until the split the relegation row names.
    ("klogd_hosted", Sched::Parallel, Tier::Nightly),
    // The two dead ends of the panic path, each staged on purpose and read for
    // what the machine manages to say on its way out. **Two names because one
    // over two boots measured 12 s twelve-wide on the dev host**, against
    // `screen_late_panic`'s 5 s there for one boot of the same shape and its
    // 3,782 ms in CI — a single name would have arrived at the fast tier's line
    // with nothing to spare. Each boot dies inside the boot phases at the marker
    // the harness waits for, so neither pays for a userland. Parallel and Fast:
    // every verdict is a substring of a report the guest wrote, and there is no
    // clock in any of it.
    ("reentry_names_the_first_panic", Sched::Parallel, Tier::Fast),
    // Nightly 2026-08-21 by the margin rule: 9,120 ms committed, inside
    // `FAST_COMMIT_MS`..`FAST_CEILING_MS`. Its twin above is 5,073 ms and stays.
    ("double_panic_names_the_fault", Sched::Parallel, Tier::Nightly),
    // §9.1's conservation law across `SYS_LOG_READ`, one registered name per
    // width, and §9.2's nesting gate at one CPU. **Three names because one over
    // three boots measured 17,112 ms in CI** — over the fast tier's line, and
    // the gate the whole design turns on may not sit in the nightly tier — and
    // because the three widths are different subjects rather than one subject
    // measured three times. Parallel: every verdict is a ledger the
    // guest computes over its own records — every sequence number read or
    // counted lost, every payload regenerated byte for byte — and not one of
    // them reads a clock. A loaded host makes the producers outrun the reader
    // further, which moves records from `read` into `lost` and leaves the law
    // exactly where it was.
    ("log_conservation_smp1", Sched::Parallel, Tier::Fast),
    ("log_conservation_smp4", Sched::Parallel, Tier::Fast),
    ("log_conservation_smp8", Sched::Parallel, Tier::Fast),
    ("log_nested_emit", Sched::Parallel, Tier::Fast),
    // The same interrupt one window earlier — between a record's shard-pointer
    // read and its `xadd` — and its negative control, which is the only reader
    // `log-unbracketed-reserve` has ever had. Parallel and Fast for
    // `log_nested_emit`'s reasons: both verdicts are the guest's ledger over its
    // own records, one saying the shard kept a single order and the other that
    // it lost it by name, and no clock is in either. Carrying `UNMEASURED_MS`
    // until the shards price them.
    ("log_reserve_window", Sched::Parallel, Tier::Fast),
    ("log_reserve_window_negative", Sched::Parallel, Tier::Fast),
    // Two processes building a fixed-width line out of two `write`s each, and a
    // count of the lines that carry both of them. Parallel and Fast: the verdict
    // is a count over a fixed number of lines the guest declares, so a loaded
    // host changes when the writers run and not whether a line is whole. It boots
    // its own machine because what it reads is the console capture, which a
    // shared boot fills with everything else.
    // Nightly 2026-08-21 by the margin rule: 8,925 ms committed, inside
    // `FAST_COMMIT_MS`..`FAST_CEILING_MS`.
    ("console_line_atomicity", Sched::Parallel, Tier::Nightly),
    // What the C family is allowed to conclude from the line above being whole:
    // a guest writes a daemon-shaped line into a real capture window on purpose
    // and the real comparison ignores it, with the filter turned off as the
    // control. One boot, two `echo`s, and every verdict is a string comparison
    // the host makes over a capture — no clock in it; Nightly because its
    // *wall* clock is whatever the partition co-schedules, and it straddles the
    // fast line run to run (`src/tiers.rs` has the two measurements).
    ("c_capture_ignores_daemon_lines", Sched::Parallel, Tier::Nightly),
    // A poll on the machine's log against a *handle* going away. Parallel and
    // Fast: both halves are verdicts the guest computes — a completion count
    // immediately after a close, retried against a record arriving in the same
    // microseconds, and a completion afterwards bounded far above the two
    // scheduler passes it needs.
    ("log_poll_outlives_a_close", Sched::Parallel, Tier::Fast),
    // The same question asked of the keyboard, where two *kinds* of object name
    // one source: a poll on stdin against the keyboard claim going away, a poll
    // on the mouse claim against its own, and an injected keystroke to show the
    // first was still armed. Parallel and Fast: two of the three verdicts are
    // counts the guest takes immediately after a close on its own thread, and
    // the third is bounded far above the one interrupt it waits for.
    ("keyboard_claim_close_spares_stdin", Sched::Parallel, Tier::Fast),
    // One boot that stops dead in phase 3, read for what it managed to say.
    ("pre_idle_wedge_speaks", Sched::Parallel, Tier::Fast),
    // Returned to Fast 2026-08-21 on nightly run 32444411794's 9,509 ms, then
    // back to Nightly the same day: run 32506320411 measured 10,281 ms and the
    // 9,509 ms it returned on is inside `FAST_COMMIT_MS`..`FAST_CEILING_MS`.
    // The i8042 pacing fix did cut it from 47,121 ms; it did not buy margin.
    ("i8042_health", Sched::Parallel, Tier::Nightly),
    // And one from here to `i8042_mouse` (`I8042_TRACE`), which is why all
    // three carry the answer the last of them needs.
    //
    // None of the three measures a rate. All three keep fewer bytes in flight
    // than QEMU's PS/2 device holds, and all three do it the same way: nothing
    // goes out until the guest has reported what the injection before it
    // produced — `i8042_mouse` within [`MOUSE_LEAD`], the two keyboard ones a
    // group at a time. So a guest with less of the host is a longer run and not
    // a smaller count. **A wall clock cannot buy that bound**: the two keyboard
    // tests spaced their injections with `thread::sleep` and put 26 and 20
    // bytes against a sixteen-byte device queue, which held only as long as the
    // guest kept draining, and a stalled guest lost bytes with nothing anywhere
    // reporting a loss. `i8042_keyboard` itself held the group Nightly on a cost
    // that was really the fixed 5 s collection deadline in
    // `test_rs_i8042_keyboard`; now that the binary exits on a sentinel instead,
    // all three return.
    ("i8042_keyboard", Sched::Parallel, Tier::Fast),
    ("i8042_no_spurious_wake", Sched::Parallel, Tier::Fast),
    ("i8042_mouse", Sched::Parallel, Tier::Fast),
    // A boot each, and deliberately not a group: every one of them changes
    // the machine's layout, which `i8042_keyboard` asserts against, and a
    // wizard that exits the instant it has its answer leaves the guest with
    // nothing to run — so a later member reads a console the previous one is
    // still draining into.
    //
    // Each is a wizard conversation typed from the host, and that used to make
    // them serial on the grounds that a dropped keystroke reads like the defect
    // they exist to catch. What actually drops a keystroke is the *device*
    // queue, not the host's clock: QEMU's PS/2 controller holds sixteen bytes
    // and none of these conversations puts more than a handful in flight before
    // waiting on what the guest printed back. Every wait here is `serial_until`
    // against a marker with a twenty-second ceiling, so a slower guest is a
    // slower test and not a different verdict — which is the same argument
    // `i8042_kbd_echo` has run on at width 4 since the phase landed.
    //
    // **Returned 2026-08-17.** Eight of its 12.6 s CI price were
    // `test_rs_locale_gate layout` holding an idle keyboard open until a fixed
    // deadline expired, against half a second of injection; it exits on the End
    // key's release now, which is `i8042_keyboard`'s own sentinel and the fix
    // §7.5 made for that whole family. Carrying `UNMEASURED_MS` until the shards
    // price it.
    ("swiss_german_layout", Sched::Parallel, Tier::Fast),
    ("locale_detect", Sched::Parallel, Tier::Fast),
    ("locale_detect_unrecognized", Sched::Parallel, Tier::Fast),
    // The wizard on the two surfaces the machine actually has, rather than on
    // the stand-in `locale_gate` is. Each costs a boot of a different image.
    ("console_locale_detect", Sched::Parallel, Tier::Fast),
    ("desktop_locale_detect", Sched::Parallel, Tier::Fast),
    // Typing at the same desktop, measured rather than transcribed: it waits
    // for its eight echoes instead of asserting how many arrived in a window,
    // so a guest that is slow costs seconds and not a verdict, and the verdict
    // itself is a fraction of the screen that no amount of load moves.
    ("desktop_typing_damage", Sched::Parallel, Tier::Nightly),
    ("desktop_window_child", Sched::Parallel, Tier::Nightly),
    // The same desktop with soundd behind it: an audio client spawned by a
    // shell, which is the only place all three of its descriptors are pipes to
    // a surface. Parallel — every verdict is a marker with its own ceiling, and
    // none of them reads a clock.
    ("desktop_audio_client", Sched::Parallel, Tier::Nightly),
    // Ctrl+Alt+D on the same machine. Parallel: it waits for a marker and its
    // verdicts are counts the report has to agree with itself about, not a
    // wall-clock margin — the one duration in it is the dump's own 250 ms
    // ceiling, which the guest spends and the host never measures.
    ("blocked_dump", Sched::Parallel, Tier::Fast),
    // Two boots of one machine compared on the guest's own `Boot: complete`
    // with a 300 ms allowance, which is the whole assertion — a real-time
    // verdict, so Nightly as TimerAnchored. Returned to Fast for half a day
    // on 2026-08-21 (PR #186, on one 9,221 ms nightly sample) and bounced the
    // merge queue at 10,738 ms twice; `src/tiers.rs` carries the straddle.
    ("i8042_absent", Sched::Serial, Tier::Nightly),
    // The fault quarantines (masks) the controller's GSI within milliseconds
    // of readiness — confirmed from the serial log, before a host round trip
    // could land anything — so no sentinel can ever reach the guest and the
    // run necessarily pays `test_rs_i8042_keyboard`'s full fallback deadline.
    // A fixed wall-clock window is the verdict's floor, not its cost:
    // timer-anchored, and its price straddles the ceiling run to run (9,355 /
    // 10,568 / 11,073 ms across three measurements) for exactly that reason.
    ("i8042_quarantine", Sched::Parallel, Tier::Nightly),
    // The negative-direction half of the same gate, and the one that runs on
    // every PR: no QEMU can stage a CPU into spinning through idle on
    // purpose, so this is `idle_is_spinning` proving its teeth against a
    // crafted trace shaped like the regression, the way
    // `control_regs`/`control_regs_verdict` split the same question.
    ("i8042_quarantine_verdict", Sched::Parallel, Tier::Fast),
    ("i8042_budget_expiry", Sched::Parallel, Tier::Fast),
    ("i8042_fadt_denial", Sched::Parallel, Tier::Fast),
    ("i8042_kbd_echo", Sched::Parallel, Tier::Fast),
    // Returned 2026-08-13: relegated on this branch for the same 5 s fixed
    // collection deadline the rest of the family crossed on, now fixed at
    // the source (`test_rs_i8042_keyboard` exits on a sentinel).
    ("i8042_undecoded_bytes", Sched::Parallel, Tier::Fast),
    // Its verdict is a cadence, and its absence is the assertion — both read
    // off the guest's own `last byte at Nms` stamps. The gap it injects is
    // 3 s against a 500 ms period, so six periods of margin decide whether the
    // report is on the pin or on a timer.
    ("i8042_health_cadence", Sched::Parallel, Tier::Nightly),
    ("xhci_xecp_walk", Sched::Parallel, Tier::Fast),
    ("xhci_slot_exhaustion", Sched::Parallel, Tier::Fast),
    ("usb_storage_gate", Sched::Parallel, Tier::Nightly),
    ("usb_storage_shapes", Sched::Parallel, Tier::Nightly),
    ("usb_refused_disk_first", Sched::Parallel, Tier::Nightly),
    // The owner's freeze, staged: `device_del` on the stick carrying `/boot`
    // and `/log` while the desktop draws. Serial because both verdicts are
    // liveness ceilings — two 2 s compositor reporting intervals inside 20 s,
    // and a console round trip inside 20 s — and a guest sharing the host with
    // eleven others answers those late for reasons that are not the defect.
    ("usb_boot_stick_pulled", Sched::Serial, Tier::Nightly),
    ("usb_pool_exhausted", Sched::Parallel, Tier::Fast),
    ("usb_short_read", Sched::Parallel, Tier::Fast),
    // A plug over QMP and two host-side verdicts, neither of them a byte
    // comparison alone: the fixed 1.2 s wait against a 100 ms debounce is a
    // staged latency window the LATE_READY assertion is waited out before
    // being read, which is timer-anchored regardless of how comfortable the
    // margin looks under TCG.
    ("usb_disk_index_stable", Sched::Parallel, Tier::Nightly),
    ("usb_storage_write_error", Sched::Parallel, Tier::Fast),
    ("usb_flush_optional", Sched::Parallel, Tier::Nightly),
    ("xhci_deaf_registers", Sched::Parallel, Tier::Nightly),
    // Mirrors the kernel's `SLOW_CONNECT_NS` as a constant of its own and
    // bounds the first port line from *both* sides. Both instants are the
    // guest's own, and it is still serial: the *injection window* is 300 ms of
    // guest **boot** time, so a guest that lost its share of the host reaches
    // its controller after the ports have stopped lying and the gate refuses to
    // certify — `the controller started at 0.366 s, past the 0.3 s the ports are
    // held empty for`, measured at width 4 with four other worktrees' suites up.
    // That is the test declining to measure nothing, which is correct, and a red
    // all the same. The fix it asks for is the kernel's: anchor the window on
    // the controller's own reset rather than on boot, which is where a real root
    // hub's detection delay starts anyway.
    ("xhci_slow_connect", Sched::Serial, Tier::Nightly),
    ("xhci_portsc_rw1c", Sched::Parallel, Tier::Fast),
    // One staged break and no other, which puts the driver's recovery finishing
    // on its first try in the verdict: a retried command that reaches an
    // endpoint still halted from the staged break logs a second `transport
    // broke`, and how many tries it takes is how much of the host the guest
    // had — its own doc says one break under KVM and two under TCG off the
    // same tree, which is the race timer-anchored, not a margin, describes.
    ("usb_transport_break", Sched::Serial, Tier::Nightly),
    ("xhci_full_speed_device", Sched::Parallel, Tier::Fast),
    ("xhci_superspeed_ports", Sched::Parallel, Tier::Fast),
    // Two of the three below stage plug and unplug with fixed waits, 600-800 ms
    // against a 100 ms debounce, plus 20-200 ms sleeps pacing the input pokes
    // that follow — staged latency windows gating the verdict, so timer-
    // anchored even though every individual check is a count of what the
    // guest logged.
    ("xhci_hotplug", Sched::Parallel, Tier::Nightly),
    // `xhci_flap` is the one that genuinely races the host against the guest:
    // its two QMP writes have to land inside *one* 100 ms debounce or the state
    // under test never happens, and it says so — `no replug collapsed inside a
    // debounce, so this run never staged the race`. A host that delays the
    // second write past 100 ms turns a green machine red with that sentence,
    // which is indistinguishable from the driver defect it hunts.
    ("xhci_flap", Sched::Serial, Tier::Nightly),
    ("xhci_hid_break", Sched::Parallel, Tier::Nightly),
    ("xhci_descriptor_walk", Sched::Parallel, Tier::Fast),
    ("esp_filesystem", Sched::Parallel, Tier::Fast),
    ("toybox_cp_volume", Sched::Parallel, Tier::Nightly),
    ("kernel_log_file", Sched::Parallel, Tier::Nightly),
    // Serial: its verdict is a cadence — heartbeats against a 250 ms period —
    // and a guest sharing the host with eleven others reaches its idle loop
    // late for reasons that are not the defect.
    ("kernel_heartbeat", Sched::Serial, Tier::Nightly),
    // Both own their images and their lanes, and neither verdict is a
    // wall-clock margin: the guest's clock starts from an instant the host set
    // and the only duration either measures is how long a boot takes to reach
    // its log sink, against a bound five minutes wide. A host so loaded that
    // this failed would have failed every timed test in the phase first.
    ("wall_clock_file", Sched::Parallel, Tier::Fast),
    ("wall_clock_refusals", Sched::Parallel, Tier::Nightly),
    // `xhci_slow_connect`'s shape against the disk's port, and serial for the
    // same reason and not by association: it shares `SLOW_CONNECT_NS`, so a boot
    // that outgrows the window binds the disk in the port scan and it reports
    // `the boot scan bound a disk, so the port was not held empty`. Same
    // measurement, same afternoon.
    ("late_storage_connect", Sched::Serial, Tier::Nightly),
    ("log_backing_read_error", Sched::Parallel, Tier::Fast),
    ("boot_volume_metadata_error", Sched::Parallel, Tier::Fast),
    ("log_partition_layout", Sched::Parallel, Tier::Fast),
    ("log_partition_identity", Sched::Parallel, Tier::Fast),
    ("cache_eviction", Sched::Parallel, Tier::Fast),
    ("va_exhaustion", Sched::Parallel, Tier::Fast),
    ("heap_ceiling_recovery", Sched::Parallel, Tier::Fast),
    ("iommu_context_absent", Sched::Parallel, Tier::Fast),
    ("iommu_empty_domain", Sched::Parallel, Tier::Fast),
    // H4: soundd driving an Intel HDA controller itself, read back off the
    // device. Serial — its verdict is a wav capture, and one taken while eleven
    // other guests contend for the host measures the host.
    ("hda_tone", Sched::Serial, Tier::Nightly),
    // The T14's panic, staged: a client that stops producing for longer than
    // the DMA ring takes to come round. The verdict is soundd's own liveness
    // and its counters rather than a capture, so it runs wide.
    ("hda_client_stall", Sched::Parallel, Tier::Nightly),
    ("hda_two_live_refused", Sched::Parallel, Tier::Fast),
    ("serial_vocabulary", Sched::Parallel, Tier::Fast),
    // Host-side, no guest: the harness asking whether it can still tell a
    // suspended machine from a slow one, and whether it reports one as a
    // verdict it does not have.
    ("suspend_detector", Sched::Parallel, Tier::Fast),
    ("suspend_invalidates_a_verdict", Sched::Parallel, Tier::Fast),
    // Same again: whether a red that is a blown liveness guard still reads as
    // one by the time it reaches the summary, and whether the `ALONE:` line
    // under a red is about the run it claims to be about.
    ("stall_is_not_a_verdict", Sched::Parallel, Tier::Fast),
    ("alone_line_reports_the_alone_run", Sched::Parallel, Tier::Fast),
    // Same: whether two guests can still be handed one lane's NVMe image, which
    // is what a shared-boot reboot did to itself.
    ("nvme_image_is_held_by_one_guest", Sched::Parallel, Tier::Fast),
    // Same: the expected-failure declaration asking whether it still refuses the
    // things it exists to refuse.
    ("expected_failure_verdicts", Sched::Parallel, Tier::Fast),
    ("expected_failure_exit_status", Sched::Parallel, Tier::Fast),
    ("expected_failure_entries", Sched::Parallel, Tier::Fast),
    // Same: the control-register verdict, against the machine this tree
    // actually booted before `arch/control_regs.rs`.
    ("control_regs_verdict", Sched::Parallel, Tier::Fast),
    // Same: which of the two shared boots each binary belongs on, asked of the
    // binaries rather than of the list that claims to name them.
    ("suite_split", Sched::Parallel, Tier::Fast),
    // Same: whether a run that did not attempt most of the suite's measured cost says
    // so where its verdict is read.
    ("nightly_tier_is_announced", Sched::Parallel, Tier::Fast),
];

/// What makes an entry stale, which is the whole safety argument for having a
/// declaration at all: **an entry must not be able to outlive its defect
/// quietly.** The two answers are not interchangeable and choosing the wrong one
/// breaks the mechanism in opposite directions.
#[derive(PartialEq, Debug)]
enum Stale {
    /// **The test passing.** For a failure that fires on every run: the day it
    /// goes green, either the defect is gone or the entry was always wrong, and
    /// both want a human. The strong form — it detects the fix itself, on the
    /// run that contains it — and the one to use wherever it is true.
    OnAPass,
    /// **A date, because a pass proves nothing.** For a failure that does not
    /// fire every run. One green of an intermittent test is one sample of a
    /// rate, and this tree's audio-gate history is the standing evidence that a
    /// verdict taken from one sample is a verdict about nothing — so
    /// [`Stale::OnAPass`] here would red a tree with nothing wrong with it, on
    /// the first lucky run, and teach everybody to re-run until it went away.
    ///
    /// This does not claim to detect the fix. It claims something weaker and
    /// honest: on this date the entry reds, and somebody fixes it, deletes it,
    /// or re-justifies it in a commit that a reviewer sees. **`YYYY-MM-DD`**,
    /// refused at startup if it does not parse — a date nothing can read is an
    /// entry that never expires.
    OnThisDate(&'static str),
}

/// One test that is expected to fail, and the open defect it fails on.
///
/// **The property that makes this safe to have at all: an entry has to be able
/// to fail the build by itself**, so that the list cannot silently outlive the
/// defect. [`Stale`] is that property and it is the field to think hardest
/// about.
#[derive(PartialEq, Debug)]
struct ExpectedFailure {
    /// The registered test name, exactly. [`check_expected_failures`] refuses a
    /// name no list carries, so a renamed or deleted test takes its entry with
    /// it rather than leaving an exemption behind for whatever gets that name
    /// next.
    test: &'static str,
    /// The task the failure is pending on. There is no entry without one: an
    /// expected failure nobody is assigned to is a disabled test.
    task: u32,
    /// Where the defect is written up, in full, with its reproduction and the
    /// evidence. **The entry never restates it** — two descriptions of one
    /// defect are two things that drift apart, and this one is the copy nobody
    /// reads while investigating.
    spec: &'static str,
    /// The failure this entry covers, quoted from the test's own message: the
    /// reason must contain **one** of these or the exemption does not apply and
    /// the run is red on it. Alternatives rather than conjuncts because one
    /// defect can surface at more than one of a test's assertions; every
    /// fragment here is a distinct place the same defect has been seen to land.
    ///
    /// Quotation rather than prose, deliberately. A restatement drifts silently
    /// from what the test says; a quotation cannot — the day somebody rewords
    /// the assertion, the entry stops matching and the run goes red asking
    /// about it.
    ///
    /// **Residual risk, which no cheap matcher closes:** this pins *which*
    /// assertion failed and not *why*. A second defect that reaches the same
    /// assertion is absorbed. Where the discriminator is a property of the log
    /// rather than of the message — `desktop_window_child`'s is *silence*, and
    /// silence is not a substring — it lives in [`ExpectedFailure::spec`] and a
    /// human applies it. The `XFAIL` line prints the pointer for that reason.
    says: &'static [&'static str],
    /// What ends this entry. See [`Stale`].
    stale: Stale,
}

impl ExpectedFailure {
    /// Whether the entry has outlived its own claim on the calendar.
    ///
    /// The [`Stale::OnAPass`] half is decided in [`Outcome::verdict_against`],
    /// where the pass is; this half is decided against the run itself, because a
    /// date arrives whether or not the test ran at all.
    fn expired(&self, today: Day) -> Option<String> {
        match self.stale {
            Stale::OnAPass => None,
            Stale::OnThisDate(date) => {
                let due = Day::parse(date).expect("check_expected_failures parsed this already");
                (today >= due).then(|| {
                    format!(
                        "its review date {date} has arrived. It says nothing about whether \
                         #{} is fixed — it says nobody has looked since it was written",
                        self.task
                    )
                })
            }
        }
    }
}

/// A civil date, as days since 1970-01-01.
///
/// Days rather than seconds because that is the resolution of the only question
/// asked of it, and because a comparison against a wall clock at second
/// resolution would flip mid-run.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
struct Day(i64);

impl Day {
    fn today() -> Day {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a host clock before 1970 is a host to fix, not a date to guess at")
            .as_secs() as i64;
        Day(secs.div_euclid(86_400))
    }

    /// `YYYY-MM-DD`, or `None`. Hinnant's `days_from_civil`, which is exact for
    /// every date the proleptic Gregorian calendar has.
    fn parse(text: &str) -> Option<Day> {
        let (y, rest) = text.split_once('-')?;
        let (m, d) = rest.split_once('-')?;
        if (y.len(), m.len(), d.len()) != (4, 2, 2) {
            return None;
        }
        let (y, m, d) = (y.parse::<i64>().ok()?, m.parse::<i64>().ok()?, d.parse::<i64>().ok()?);
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(Day(era * 146_097 + doe - 719_468))
    }
}

/// Tests this tree expects to fail, and what each is pending on.
///
/// **Empty is the normal state**, and an entry is a claim with a cost: the run
/// that carries it is not a clean run and says so in its last line. See
/// [`ExpectedFailure`] for what an entry has to be able to say, and
/// [`Tally::summary`] for what a run does with one.
const EXPECTED_FAILURES: &[ExpectedFailure] = &[ExpectedFailure {
    test: "desktop_window_child",
    task: 156,
    spec: "issues/kernel/desktop-window-child-freeze.md",
    // The rule that decides this list, so that the next fragment added to it has
    // one to be judged against: **a message belongs here when its failure is the
    // desktop ceasing to answer after a window closed.** That is what both open
    // defects under this test produce — the freeze, and the shell exiting
    // instead of prompting. The test's other five messages are deliberately
    // absent because each names something else happening: the client binary
    // missing, the desktop never coming up at all, a window never being created
    // (twice — the child's and snake's), and the client leaving on its own
    // deadline. Any of those reds the run.
    says: &[
        "the windowed child never reported leaving",
        "a windowed child exited by itself and the shell never answered again",
        "GUI+Q never reached the compositor",
        "the compositor closed the window and the client did not leave",
        "snake did not leave when its window was closed in round",
        "snake's window was closed, snake left, and the shell never answered again",
    ],
    // Intermittent — it has been red alone and red wide, at a different point
    // each time — so a green is one sample of a rate and may not red the run.
    // A month: long enough that a fix already in flight lands first, short
    // enough that nobody inherits this silently.
    stale: Stale::OnThisDate("2026-09-06"),
}, ExpectedFailure {
    test: "hda_tone",
    task: 88,
    spec: "issues/audio/hda-tone-phase-check.md",
    // Only the phase check. Everything else `hda_tone` asserts — the kernel
    // binding one controller, soundd walking the codec and naming its pin, the
    // whole allow-list, a tone at full amplitude, no mid-tone silence — reds the
    // run, because each of those is the milestone rather than the open question.
    says: &["the captured tone is not one sine"],
    // Intermittent: seven runs on this host gave 8, 8, 8, 8, 8, 16 and 0 breaks,
    // so a green is one sample and may not red a healthy tree. The date is the
    // same month the entry above uses, for the same reason.
    stale: Stale::OnThisDate("2026-09-06"),
}];

/// The renderer's two text colours, as the screendump reports them.
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
const ALERT: [u8; 3] = [0xFF, 0x50, 0x50];
/// And the fill a halted machine leaves behind.
const FILL_FATAL: [u8; 3] = [0x60, 0x00, 0x00];
/// The fill a boot checkpoint leaves behind. It is the only thing that tells a
/// diagnostic boot's screen from a fatal report's — both carry the same log
/// lines, and one of them means the machine died.
const FILL_BOOT: [u8; 3] = [0x00, 0x00, 0x00];

/// The T14 Gen 2's panel as the console grids it: 1080/16 rows of 1920/8
/// columns. `Profile::Metal` caps vgamem at 8 MiB, so the most-pixels mode
/// the bootloader picks *is* this panel — the test's screen and the laptop's
/// share one geometry. Every geometry claim `screen_diag_boot` makes is made
/// against these two numbers and not against the screen it is reading.
const T14_ROWS: usize = 1080 / 16;
const T14_COLS: usize = 1920 / 8;

/// The line `SYS_DEBUG` action 3 logs immediately before halting every CPU.
/// It exists only on a `test-actuators` kernel — every other action costs the
/// caller its own process, this one costs the machine. Kept in sync with
/// `kernel/src/arch/syscall.rs` by this comment and by screen_fatal_halt
/// failing loudly if it drifts.
const FATAL_HALT_NONCE: &str = "SYS_DEBUG: fatal halt 4b1d9e2c";

/// What `apic::wait_for_log_file` says when its `LOG_FILE_DRAIN` is spent —
/// the second, degraded half of the kernel's promise about a fatal report.
///
/// `screen_fatal_halt_composited` reads it off the *panel*, because the machine
/// that wait exists for has no serial port and `/log` is the thing that did not
/// answer. Kept in sync with `kernel/src/arch/apic.rs::LOG_DRAIN_EXPIRED` by
/// this comment and by that test turning every spent budget into a red if it
/// drifts.
const LOG_DRAIN_EXPIRED: &str = "the report did not reach /log";

/// How far a corpus case gets before it stops, and what it says when it does.
///
/// There is no `Run`. Seventeen of these built at the moment this list was
/// written and nobody has ever asked whether they run; turning one on is a
/// guest slot and possibly a hung lane, so the question is filed rather than
/// answered here.
#[derive(Clone, Copy)]
enum Stage {
    /// toyos-cc refuses it, and this is what the refusal says.
    ///
    /// Quoted for the same reason `EXPECTED_FAILURES` quotes a failure
    /// message: a second defect landing on the same case must not be able to
    /// hide under the first.
    Refused(&'static str),
    /// It compiles, and the link does not resolve — this symbol.
    NoLink(&'static str),
    /// It builds. The decision is only not to run it.
    Built,
}

/// Why a case is not run.
enum Why {
    /// Considered and declined. Nothing is owed, which is why this list has no
    /// `task` field where `EXPECTED_FAILURES` requires one: an expected
    /// failure nobody is assigned to is a disabled test, and a *decline* is
    /// not owed to anybody by construction.
    Declined(&'static str),
    /// Held open by a write-up, which is where the reason lives.
    Open(&'static str),
}

impl Why {
    fn stated(&self) -> String {
        match self {
            Why::Declined(reason) => format!("declined: {reason}"),
            Why::Open(path) => format!("held open by {path}"),
        }
    }
}

/// A corpus case the suite does not run.
///
/// One list, because "is this declined or is it broken" and "how far does it
/// get" are two questions, and the two lists this replaces each answered one
/// of them for a different set of cases. `C_SKIP` was 32 names that nothing
/// ever attempted: 17 of them compiled fine, several stated a reason that was
/// not the reason — `03_struct` said `_Generic` and stopped on
/// `__attribute__((cleanup))`, `123_vla_bug` said "VLA codegen bug" and built
/// — and a name that no longer matched a file would have left a dead
/// exemption behind for ever.
///
/// **Every entry is attempted to its declared stage on every run.** Getting
/// further means the fix arrived and the entry goes; getting less far is a
/// regression. Both red the run. There is no review-date escape hatch of the
/// `Stale::OnThisDate` kind, and no need of one: a host compile is
/// deterministic, so one green here is the whole population rather than one
/// sample of an intermittent.
struct NotRun {
    /// The corpus file's stem. A name with no `.c` reds the run, so a rename
    /// takes its entry with it.
    case: &'static str,
    stage: Stage,
    why: Why,
}

/// Where the question about the `Built` set lives.
const BUILT_NOT_RUN: &str = "issues/build/c-corpus-cases-build-and-are-not-run.md";

const NOT_RUN: &[NotRun] = &[
    NotRun {
        case: "03_struct",
        stage: Stage::Refused("__attribute__((__cleanup__)) is not implemented"),
        why: Why::Declined("cleanup attributes; the entry used to say _Generic, which is 33_ternary_op's reason and not this one"),
    },
    NotRun { case: "18_include", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "78_vla_label", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "79_vla_continue", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "31_args", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "32_led", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "33_ternary_op",
        stage: Stage::Refused("_Generic type dispatch is not implemented"),
        why: Why::Declined("_Generic"),
    },
    NotRun { case: "40_stdio", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "60_errors_and_warnings",
        stage: Stage::NoLink("main"),
        why: Why::Declined("a meta-test of compiler diagnostics: every branch is behind a -D the harness does not pass, so the file preprocesses to no `main`"),
    },
    NotRun {
        case: "73_arm64",
        stage: Stage::Refused("arg 1 (v94) has type i32, expected i64"),
        why: Why::Declined("aarch64-specific, and this target is x86-64. It does not stop with a refusal by name — it stops in the verifier, on a variadic call, which is a defect of ours reached through a case we decline anyway"),
    },
    NotRun {
        case: "89_nocode_wanted",
        stage: Stage::Refused("failed to define function 'kb_wait_3'"),
        why: Why::Open("issues/build/toyos-cc-goto-out-of-a-statement-expression.md"),
    },
    NotRun {
        case: "83_utf8_in_identifiers",
        stage: Stage::Refused("unexpected character '\u{ef}' (0xef)"),
        why: Why::Declined("non-ASCII identifiers. UTF-8 in strings and comments works; the lexer stops on the byte it could not read, so nothing is dropped"),
    },
    NotRun {
        case: "85_asm_outside_function",
        stage: Stage::Refused("file-scope asm(...) is not implemented"),
        why: Why::Declined("emitting file-scope asm needs an x86-64 assembler"),
    },
    NotRun {
        case: "94_generic",
        stage: Stage::Refused("_Generic type dispatch is not implemented"),
        why: Why::Declined("_Generic"),
    },
    NotRun {
        case: "95_bitfields",
        stage: Stage::Refused("#pragma pack(push,1) is not implemented"),
        why: Why::Declined("a self-including bitfield torture test wanting #pragma pack, ms_struct, gcc_struct, aligned on a declaration specifier and packed bitfields — every one of them a deliberate refusal"),
    },
    NotRun {
        case: "95_bitfields_ms",
        stage: Stage::Refused("#pragma pack(push,1) is not implemented"),
        why: Why::Declined("the same file again, through a two-line wrapper"),
    },
    NotRun {
        case: "96_nodata_wanted",
        stage: Stage::NoLink("main"),
        why: Why::Declined("seven configurations selected by a -D from tcc's own Makefile, four of which expect compiler diagnostics. The harness compiles one configuration and compares one stdout, so no fix to toyos-cc can make it pass"),
    },
    NotRun {
        case: "98_al_ax_extend",
        stage: Stage::Refused("file-scope asm(...) is not implemented"),
        why: Why::Declined("file-scope asm again"),
    },
    NotRun {
        case: "99_fastcall",
        stage: Stage::Refused("file-scope asm(...) is not implemented"),
        why: Why::Declined("32-bit x86 — pushl %esp, pusha, __attribute((fastcall)). It stops on the file-scope asm at line 26 before reaching any of that"),
    },
    NotRun {
        case: "101_cleanup",
        stage: Stage::Refused("__attribute__((cleanup)) is not implemented"),
        why: Why::Declined("cleanup attributes"),
    },
    NotRun {
        case: "102_alignas",
        stage: Stage::Refused("expected Semi, got Alignas"),
        why: Why::Declined("_Alignas. It stops as a parse error rather than by name, which reads worse and is still a stop"),
    },
    NotRun { case: "103_implicit_memmove", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "104_inline",
        stage: Stage::Refused("unexpected token in expression: Attribute"),
        why: Why::Declined("weak symbols. The file itself compiles — the companion `104+_inline.c` is what stops, which is the stage as the harness reaches it"),
    },
    NotRun {
        case: "106_versym",
        stage: Stage::NoLink("PTHREAD_PROCESS_SHARED"),
        why: Why::Declined("pthread condition variables"),
    },
    NotRun { case: "107_stack_safe", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "108_constructor",
        stage: Stage::Refused("__attribute__((constructor)) is not implemented"),
        why: Why::Declined("constructor attributes"),
    },
    NotRun { case: "109_float_struct_calling", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "112_backtrace", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "113_btdll",
        stage: Stage::NoLink("f_1"),
        why: Why::Declined("three shared libraries built from the same file under -DDLL=1,2,3 and loaded at run time; the harness builds one object and one binary"),
    },
    NotRun {
        case: "114_bound_signal",
        stage: Stage::Refused("expected Semi, got Ident(\"sj\")"),
        why: Why::Declined("sigaction and sigjmp_buf, which no header here declares, so the declaration does not parse"),
    },
    NotRun { case: "115_bound_setjmp", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "116_bound_setjmp2", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "117_builtins",
        stage: Stage::NoLink("__builtin_abort"),
        why: Why::Declined("__builtin_abort, and the compiler implements no builtin under that name"),
    },
    NotRun {
        case: "120_alias",
        stage: Stage::Refused("__attribute__((alias)) is not implemented"),
        why: Why::Declined("symbol aliases. Its two `__asm__(_\"name\")` renames at lines 19 and 20 are refused too, but the attribute at line 9 comes first"),
    },
    NotRun { case: "122_vla_reuse", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun { case: "123_vla_bug", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "124_atomic_counter",
        stage: Stage::Refused("cannot find system include file: stdatomic.h"),
        why: Why::Declined("C11 atomics"),
    },
    NotRun {
        case: "125_atomic_misc",
        stage: Stage::Refused("cannot find system include file: stdatomic.h"),
        why: Why::Declined("C11 atomics"),
    },
    NotRun { case: "126_bound_global", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "127_asm_goto",
        stage: Stage::Refused("expected LParen, got Goto"),
        why: Why::Declined("`asm goto`, and inline asm generally"),
    },
    NotRun {
        case: "128_run_atexit",
        stage: Stage::Refused("__attribute__((constructor)) is not implemented"),
        why: Why::Declined("constructor attributes, and a -D per configuration to have a main at all"),
    },
    NotRun { case: "132_bound_test", stage: Stage::Built, why: Why::Open(BUILT_NOT_RUN) },
    NotRun {
        case: "136_atomic_gcc_style",
        stage: Stage::Refused("cannot find system include file: stdatomic.h"),
        why: Why::Declined("C11 atomics"),
    },
];

/// Discover C tests by scanning tests/testcases/tinycc/*.c.
/// Skips companion files (contain '+') and everything in [`NOT_RUN`].
fn discover_c_tests() -> Vec<String> {
    let dir = compile::testcases_dir();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_str()?.to_string();
            let stem = name.strip_suffix(".c")?;
            if stem.contains('+') {
                return None;
            }
            if NOT_RUN.iter().any(|d| d.case == stem) {
                return None;
            }
            Some(stem.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Discover Rust test binaries from build output.
/// Skips shared libraries, helper binaries, and audio tests (dedicated boot).
///
/// **A name that arrives this way is registered by nothing but its file.**
/// `tests/toyos-rust-tests/src/bin/<name>.rs` is the whole declaration — no row
/// here names it — so `src/durations.rs`'s touched-names scan reads that
/// directory beside the registration tables, on the same stem rule. Move the
/// rule and both ends move, or a new test's price verdict goes unrendered on
/// the run that introduces it.
fn discover_rust_tests(bins: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut names: Vec<String> = bins
        .iter()
        .filter_map(|(name, _)| {
            if name.ends_with(".so") {
                return None;
            }
            if RUST_SKIP.contains(&name.as_str())
                || AUDIO_TESTS.iter().any(|(audio, _)| *audio == name)
            {
                return None;
            }
            Some(name.clone())
        })
        .collect();
    names.sort();
    names
}

fn compile_c_tests(names: &[String]) -> Vec<(String, Vec<u8>)> {
    // Suppress panic messages during compilation — we handle failures via catch_unwind.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut bins = Vec::new();
    let mut broken: Vec<(&str, String)> = Vec::new();
    for name in names {
        match std::panic::catch_unwind(|| {
            let (obj, extras) = compile::compile_c(name);
            compile::link_toyos(&obj, &extras, name)
        }) {
            Ok(linked) => bins.push((name.clone(), linked)),
            Err(e) => broken.push((name.as_str(), panic_message(&e))),
        }
    }

    std::panic::set_hook(prev_hook);

    if !broken.is_empty() {
        let mut msg = String::from(
            "a C test that is not declared in NOT_RUN stopped building, and a test that does \
             not build is a test that does not run:\n",
        );
        for (name, why) in &broken {
            msg += &format!("  c::{name}: {why}\n");
        }
        panic!("{msg}");
    }

    bins
}

/// Attempt every declared case exactly as far as it says it gets.
///
/// The list this replaces was asserted in one direction for nine names and in
/// no direction at all for thirty-two. The cost of the whole pass is a fraction
/// of a second, so nothing here is bought with test time.
fn check_not_run() {
    let dir = compile::testcases_dir();
    let mut wrong: Vec<String> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();

    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    for entry in NOT_RUN {
        let case = entry.case;
        let before = wrong.len();
        if !seen.insert(case) {
            wrong.push(format!("{case}: named twice"));
            continue;
        }
        if !dir.join(format!("{case}.c")).is_file() {
            wrong.push(format!("{case}: no such file in the corpus — a rename left this behind"));
            continue;
        }
        let compiled = std::panic::catch_unwind(|| compile::compile_c(case));
        match (&entry.stage, compiled) {
            (Stage::Refused(says), Err(e)) => {
                let said = panic_message(&e);
                if !said.contains(says) {
                    wrong.push(format!(
                        "{case}: refused, but not for the declared reason.\n    \
                         declared: {says}\n    said:     {said}"
                    ));
                }
            }
            (Stage::Refused(says), Ok(_)) => wrong.push(format!(
                "{case}: compiles now — it was declared to stop with {says:?}. \
                 The fix arrived; delete the entry and let the case run."
            )),
            (_, Err(e)) => wrong.push(format!(
                "{case}: no longer compiles, and it was declared to get further: {}",
                panic_message(&e)
            )),
            (stage, Ok((obj, extras))) => {
                let linked = std::panic::catch_unwind(|| compile::link_toyos(&obj, &extras, case));
                match (stage, linked) {
                    (Stage::NoLink(symbol), Err(e)) => {
                        let said = panic_message(&e);
                        if !said.contains(symbol) {
                            wrong.push(format!(
                                "{case}: the link fails on something else.\n    \
                                 declared: undefined symbol: {symbol}\n    said:     {said}"
                            ));
                        }
                    }
                    (Stage::NoLink(symbol), Ok(_)) => wrong.push(format!(
                        "{case}: links now — it was declared to fail on {symbol:?}. \
                         The fix arrived; delete the entry and let the case run."
                    )),
                    (Stage::Built, Err(e)) => wrong.push(format!(
                        "{case}: no longer links, and it was declared to build: {}",
                        panic_message(&e)
                    )),
                    (Stage::Built, Ok(_)) => {}
                    (Stage::Refused(_), _) => unreachable!("handled above"),
                }
            }
        }
        for line in &mut wrong[before..] {
            *line += &format!("\n    ({})", entry.why.stated());
        }
    }

    std::panic::set_hook(prev_hook);

    assert!(
        wrong.is_empty(),
        "NOT_RUN no longer describes the corpus. Every entry is attempted to its declared \
         stage on every run, so this is a case that moved:\n  {}",
        wrong.join("\n  "),
    );
}

/// What a caught panic said, first line, whole. A refusal quoted in `NOT_RUN`
/// is compared against this, so nothing here may shorten it.
fn panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    let full = e
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string());
    full.lines().next().unwrap_or_default().to_string()
}

/// How many kernel lines a dead test's report is printed with.
///
/// A fault report is a header, a register dump and a bounded backtrace, and the
/// daemons keep talking beside it. Sixty holds one whole report and its
/// surroundings; past that the tail is the part that says how it ended, and the
/// line says how many it dropped rather than dropping them in silence.
const MAX_KERNEL_LINES: usize = 60;

/// The kernel's own account of a test that died, which `stdout` cannot carry.
///
/// **`exit code Some(-1)` is the kernel saying it killed the process** —
/// `recover_or_halt` answers a Ring 3 fault with `kill_process(-1)` — and every
/// word of *why* is a `log!`: the vector, `rip`, `cr2`, the resolved symbol.
/// `run_test_paced` files kernel lines under `serial` and keeps them out of
/// `stdout`, which is right for a test that passed and leaves a killed one with
/// no evidence whatsoever. `std_unwind` and `std_unwind_so` have been red in
/// every twelve-shard CI run there has been, and each one reported the same
/// eleven characters and no address.
fn kernel_account(result: &TestResult) -> String {
    let lines: Vec<&str> = result.serial.lines().filter(|l| qemu::is_kernel_line(l)).collect();
    if lines.is_empty() {
        return "\n--- the kernel said nothing while it ran ---".to_string();
    }
    let dropped = lines.len().saturating_sub(MAX_KERNEL_LINES);
    let how_many = if dropped > 0 {
        format!(" (the last {MAX_KERNEL_LINES} of {})", lines.len())
    } else {
        String::new()
    };
    format!("\n--- what the kernel said{how_many} ---\n{}", lines[dropped..].join("\n"))
}

fn check_c_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL c::{test_name}: {err}{}", kernel_account(result));
        return false;
    }

    match result.exit_code {
        Some(0) => {
            let expect_file = compile::testcases_dir().join(format!("{test_name}.expect"));
            if expect_file.exists() {
                let expected = fs::read_to_string(&expect_file).unwrap();
                // **The one comparison in this suite that reads a whole capture
                // as one program's output, on a console every process shares.**
                // `common::console::verdict` takes the lines that are some
                // other process's out of it first, and hands them back so they
                // can be printed rather than vanish. Its own doc has the two
                // ways that happens and `c_capture_ignores_daemon_lines` is the
                // gate under it.
                let verdict = common::console::c_verdict(&result.stdout, &expected);
                if !verdict.filtered.is_empty() {
                    eprintln!(
                        "  [c] {test_name}: {} console line(s) in this window were another \
                         process's and did not decide the verdict:\n    {}",
                        verdict.filtered.len(),
                        verdict.filtered.join("\n    "),
                    );
                }
                if let Some(mismatch) = verdict.mismatch {
                    eprintln!("FAIL c::{test_name}: {mismatch}");
                    return false;
                }
            }
            true
        }
        Some(code) => {
            eprintln!(
                "FAIL c::{test_name}: exit code {code}\nstdout: {}{}",
                result.stdout,
                kernel_account(result)
            );
            false
        }
        None => {
            eprintln!("FAIL c::{test_name}: no exit code{}", kernel_account(result));
            false
        }
    }
}

fn check_rust_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL rs::{test_name}: {err}{}", kernel_account(result));
        return false;
    }

    match result.exit_code {
        Some(0) => true,
        Some(code) => {
            eprintln!(
                "FAIL rs::{test_name}: exit code {code}\nstdout:\n{}{}",
                result.stdout,
                kernel_account(result)
            );
            false
        }
        None => {
            eprintln!(
                "FAIL rs::{test_name}: no exit code\nstdout:\n{}{}",
                result.stdout,
                kernel_account(result)
            );
            false
        }
    }
}

/// Checks both exit code and serial diagnostics for panic recovery.
fn check_panic_recovery(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }

    let checks: &[(&str, &str)] = &[
        ("PANIC:", "expected PANIC header"),
        ("SYS_DEBUG", "expected SYS_DEBUG in panic message"),
        ("Syscall: num=92", "expected syscall context in panic report"),
        ("User backtrace:", "expected user backtrace in panic report"),
        ("Registers:", "expected register dump from kernel fault"),
        ("SEGFAULT tid=", "expected SEGFAULT header"),
        ("deliberate_null_deref", "expected deliberate_null_deref in segfault backtrace"),
        ("+0x", "expected symbolized backtraces"),
    ];

    let mut ok = true;
    for (needle, msg) in checks {
        if !result.serial.contains(needle) {
            eprintln!("FAIL rs::panic_recovery: {msg}\nserial:\n{}", result.serial);
            ok = false;
        }
    }
    if let Err(msg) = check_tripwire_attribution(&result.serial) {
        eprintln!("FAIL rs::panic_recovery: {msg}\nserial:\n{}", result.serial);
        ok = false;
    }
    ok & check_symbols_were_read("panic_recovery", &result.serial)
}

/// The kernel names the frames of a process it loaded off a **disk**.
///
/// `check_panic_recovery` above asserts the same thing for a process loaded out
/// of the initrd, and that was the only demangled-name assertion in the tree.
/// The two paths were different code: the initrd answered
/// `FileBacking::memory_ptr` and nothing else did, so this one produced a
/// backtrace of bare `[exe+0x…]` offsets and no test could tell. Watch it red
/// with `read_backtrace_table` replaced by `SymbolTable::empty_with_bounds`.
///
/// `null_deref_run_from_disk` is this child's alone, so a `contains` over the
/// capture window cannot be satisfied by `segfault_child` running in the same
/// boot.
fn check_disk_backtrace(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }

    let checks: &[(&str, &str)] = &[
        ("SEGFAULT tid=", "expected a SEGFAULT header for the child run off /home"),
        (
            "null_deref_run_from_disk",
            "expected the faulting function's demangled name — a process loaded off a disk \
             got a backtrace with no names in it",
        ),
    ];

    let mut ok = true;
    for (needle, msg) in checks {
        if !result.serial.contains(needle) {
            eprintln!("FAIL rs::disk_backtrace: {msg}\nserial:\n{}", result.serial);
            ok = false;
        }
    }
    ok & check_symbols_were_read("disk_backtrace", &result.serial)
}

/// No line of a crash report conceded its symbol to a lock somebody else held.
///
/// **The reason every check above is allowed to be a `contains`.** A symbol
/// lookup on the fault path is a `try_lock` — it has to be, the faulting thread
/// may hold either lock itself — so "no name here" used to mean either "this
/// address has no name" or "nobody looked", and a gate asserting on a name red
/// intermittently on the second. It was not hypothetical: `fault_gates` red 2
/// of 5 full runs and `disk_backtrace` 1 of 5 on `wt/toyos-logd`, with the
/// backtrace three lines below the unresolved `rip:` naming the very symbol the
/// line above had lost.
///
/// `process::with_user_symbols` now says which, so this reds on the reason
/// instead — and the reason is worth a red, because `reap_poisoned` no longer
/// takes the process table on every idle trip, so a busy answer names a holder
/// worth finding rather than the housekeeping that used to be there.
fn check_symbols_were_read(test: &str, serial: &str) -> bool {
    const CONCEDED: &str = "<symbol unread:";
    let lines: Vec<&str> = serial.lines().filter(|l| l.contains(CONCEDED)).collect();
    if lines.is_empty() {
        return true;
    }
    eprintln!(
        "FAIL rs::{test}: the crash report could not read a symbol it was asked for, so a bare \
         address in it is a lost race and not a verdict:\n{}\nserial:\n{serial}",
        lines.join("\n"),
    );
    false
}

/// The §6.4 tripwire must fire, and its `panicked at` must name the syscall
/// that held the lock rather than the scheduler that caught it — which is the
/// only thing `#[track_caller]` on `assert_baseline` buys.
///
/// A whole-buffer `contains("arch/syscall.rs")` certifies none of that: the
/// same boot's `test_syscall_panic` panics in that file too, so the needle is
/// already present before the tripwire runs. Scope it instead to the window
/// between this panic's header and its message — `panicked at <location>` is
/// the only thing in there, and the backtrace that names every frame comes
/// after the message, so it cannot supply the answer either.
fn check_tripwire_attribution(serial: &str) -> Result<(), String> {
    const MSG: &str = "scheduler entered while a lock is held";
    const HEADER: &str = "PANIC:";
    let msg_at = serial
        .find(MSG)
        .ok_or("expected the §6.4 lock-across-switch tripwire to fire")?;
    let header_at = serial[..msg_at]
        .rfind(HEADER)
        .ok_or("tripwire message with no panic header before it")?;
    let location = &serial[header_at..msg_at];
    if !location.contains("arch/syscall.rs") {
        return Err(format!(
            "expected the tripwire to name the guilty call site, not scheduler.rs; got: {}",
            location.trim()
        ));
    }
    Ok(())
}

/// A zero CPU delta is the signature of a suspended soundd and equally of one
/// wedged with the device running, so the counter the test reads cannot tell
/// them apart on its own. The serial can: in a window where no audio client
/// ever connects, the PCM stream has no business starting.
///
/// This is bounded by what the harness captures — collection begins at
/// ===TEST_START, so a device started before then (a restored boot prime) is
/// invisible here as it is everywhere else; see `audio::check_suspend_structure`.
/// What it does catch is a start inside the window with no client to justify
/// it: soundd's `!streams.is_empty()` fill-loop gate going away, or a resume
/// fired by anything other than a connect.
fn check_audio_idle_suspend(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }
    const STARTED: &str = "virtio-sound: stream 0 started";
    if result.serial.contains(STARTED) {
        eprintln!(
            "FAIL rs::audio_idle_suspend: `{STARTED}` with no client connected — \
             soundd's zero CPU is the device left running, not a suspend\nserial:\n{}",
            result.serial
        );
        return false;
    }
    true
}

/// Two clients through the null sink, and what soundd said about each leaving.
///
/// The exit code already says both `/bin/tone` runs finished cleanly — that is
/// the test's own assertion — so this window is exactly the case soundd used to
/// misreport: `client N died` for a process that exited `code=0`, because the
/// mix loop's signal pipe broke before the control thread read the peer. What
/// it asserts is that neither outcome of that race is worded as a death, and
/// that both removals name a departure soundd actually established (§7).
///
/// **The count is per removal and stays exact**, because the vocabulary is
/// asserted per removal: a capture where no client ever left would satisfy
/// every check above it vacuously, and a range would let the second removal go
/// missing again. What used to make that count a race was the window and not
/// the number — see [`settle_null_sink_client_exits`], which is what closes it.
fn check_null_sink_client_exits(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }
    let problems = audio::check_departures(&result.serial, NULL_SINK_CLIENTS);
    if !problems.is_empty() {
        eprintln!(
            "FAIL rs::null_sink_client_exits: {}\nserial:\n{}",
            problems.join("; "),
            result.serial
        );
        return false;
    }
    true
}

/// The exit code says the child died; only the serial says *why*.
///
/// A #DE with no gate escalates to #DF, and `double_fault_handler` halts every
/// CPU — so a run that reaches this function at all already survived. What is
/// left to check is that the kernel took the fault as a #DE rather than as
/// something the escalation left behind: the report names the vector and the
/// function that raised it, and no double fault appears in the window.
fn check_fault_gates(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }

    let checks: &[(&str, &str)] = &[
        ("SIGFPE tid=", "expected a SIGFPE header for the divide by zero"),
        ("divide error", "expected the #DE report to name the vector"),
        (
            "fault_gate_child::divide_by_zero",
            "expected the faulting function in the #DE backtrace",
        ),
    ];

    let mut ok = true;
    for (needle, msg) in checks {
        if !result.serial.contains(needle) {
            eprintln!("FAIL rs::fault_gates: {msg}\nserial:\n{}", result.serial);
            ok = false;
        }
    }
    if result.serial.contains("DOUBLE FAULT") {
        eprintln!(
            "FAIL rs::fault_gates: a Ring 3 fault escalated to #DF — its vector has no gate\
             \nserial:\n{}",
            result.serial
        );
        ok = false;
    }
    ok & check_symbols_were_read("fault_gates", &result.serial)
}

/// The guest asserts its children died; this asserts *what the kernel said* —
/// and, harder, what it did not say.
///
/// **The absences are the half the exit code cannot carry.** Vector 1 used to
/// reach a debugger-session aid that dumped registers, disarmed `DR7`/`DR6`,
/// walked a backtrace and returned to resume, and a Ring 3 process reached it
/// with one instruction. A kernel that put that handler back would still let
/// `debug_trap`'s children die if some *later* instruction faulted, so the
/// verdict has to be that the report is not in the window at all. `DOUBLE FAULT`
/// is the other absence and it is not hypothetical: without `TF` in
/// `IA32_FMASK`, `popfq` followed by `syscall` takes the `#DB` at
/// `syscall_entry+0x0` with `rsp` still the user stack, and every CPU halts.
fn check_debug_trap(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }

    let mut ok = true;
    // `crash_report_exception`'s default arm for a Ring 3 fault, with
    // `vector_name(Vector::Debug)` after it. Matched as a whole line rather than
    // as a substring, because the binary's own name carries the word `debug`.
    let named = result
        .serial
        .lines()
        .any(|l| l.contains("FATAL tid=") && l.trim_end().ends_with(": debug"));
    if !named {
        eprintln!(
            "FAIL rs::debug_trap: no `FATAL tid=N: debug` line — the kernel ended the children \
             but not as a #DB, so the vector reached somewhere else\nserial:\n{}",
            result.serial
        );
        ok = false;
    }

    let absences: &[(&str, &str)] = &[
        (
            "DB TRAP",
            "the #DB handler's UART marker is in the window: a Ring 3 trap reached a kernel \
             report path",
        ),
        (
            "HARDWARE WATCHPOINT",
            "the watchpoint report is in the window: a Ring 3 trap made the kernel walk kernel \
             state and resume",
        ),
        (
            "DOUBLE FAULT",
            "a Ring 3 debug trap escalated to #DF — the #DB frame was built on a stack the CPU \
             could not write, which is `TF` missing from IA32_FMASK",
        ),
        (
            "KERNEL PANIC",
            "the kernel blamed itself for a trap a Ring 3 process raised",
        ),
    ];
    for (needle, msg) in absences {
        if result.serial.contains(needle) {
            eprintln!("FAIL rs::debug_trap: {msg}\nserial:\n{}", result.serial);
            ok = false;
        }
    }
    ok
}

/// The clients `test_rs_null_sink_client_exits` runs in series, and so the
/// number of removals soundd owes. One constant, because the wait below and the
/// count above have to be the same number or the wait is for something else.
const NULL_SINK_CLIENTS: usize = 2;

/// Wait for soundd to report the second client leaving, on the guest's liveness.
///
/// **The last removal arrives after the process whose exit produced it**, and
/// that process exiting is what ends the capture: round 1's line makes it in
/// because a whole second round follows it, and round 2's has nothing behind it
/// but `===TEST_END===`. Counting two removals over that window is an assertion
/// about scheduling, and it went red on CI twice on documentation-only branches
/// — `soundd reported 1 client removals, expected 2` — with the capture showing
/// the line never arriving rather than arriving wrong.
///
/// The wait is [`await_guest`]'s: it ends when the removals are there, or when
/// the guest stops making progress, and never on a span of host wall clock. It
/// costs nothing on a run that already had both lines — the predicate is checked
/// before anything is drained, which was true of 6 of 6 measured runs on the dev
/// host — and its expiry is not a verdict, which is why the error is dropped:
/// what fails this test is still the count, in `check_departures`'s own words.
///
/// [`audio::SOUNDD_GONE`] ends it too, for the same reason `await_null_sink`
/// reads that line: soundd exiting is a removal that is never coming, and the
/// test should say so in its own sentence rather than wait out the guard. The
/// guard is the whole of [`qemu::GUEST_WEDGED`] here and not the quiet bound —
/// this boot's kernel prints on a 10 s cadence, so the machine is never silent
/// for the 15 s that would end the wait early (measured: an unreachable
/// predicate takes 302 s). That price is paid only by a run where soundd is
/// alive and has genuinely stopped reporting departures, which is the defect
/// this test exists for.
fn settle_null_sink_client_exits(qemu: &mut QemuInstance, result: &mut TestResult) {
    let mut serial = std::mem::take(&mut result.serial);
    let _ = await_guest(qemu, &mut serial, "soundd to report both clients leaving", |seen| {
        audio::departures(seen).len() >= NULL_SINK_CLIENTS || seen.contains(audio::SOUNDD_GONE)
    });
    result.serial = serial;
}

/// Nothing to wait for: the test's own window carries everything its check
/// reads. Every name but one.
fn no_settle(_: &mut QemuInstance, _: &mut TestResult) {}

/// Select the between-the-test-and-its-check wait by name, as [`check_for`]
/// selects the check.
fn settle_for(name: &str) -> fn(&mut QemuInstance, &mut TestResult) {
    match name {
        "null_sink_client_exits" => settle_null_sink_client_exits,
        _ => no_settle,
    }
}

/// Select check function by test name convention.
fn check_for(name: &str) -> fn(&TestResult) -> bool {
    match name {
        "panic_recovery" => check_panic_recovery,
        "disk_backtrace" => check_disk_backtrace,
        "audio_idle_suspend" => check_audio_idle_suspend,
        "null_sink_client_exits" => check_null_sink_client_exits,
        "fault_gates" => check_fault_gates,
        "debug_trap" => check_debug_trap,
        _ => check_rust_result,
    }
}

/// Minimum active (non-silent) playback the 3s test tone must produce.
/// Guards against a vacuous pass when nothing plays at all.
const TONE_MIN_ACTIVE_SECS: f64 = 2.5;
/// The tone is generated at amplitude 16000; a far lower peak proves the
/// signal path is broken even if technically "active".
const TONE_MIN_PEAK: i32 = 4000;

/// Recorded per-(test, smp) baselines — gate A's thorough tier.
/// Two independent instruments per config:
/// the wav underrun histogram (`gaps`, keyed by gap length in device periods)
/// and ceilings on soundd's own counters. The wav is a rare-event detector;
/// the counters fire on nearly every run and carry the statistical power. Both
/// must hold. Re-record deliberately, never casually — and justify every
/// number in `tests/audio-baseline.toml` itself.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioBaselineEntry {
    #[serde(default)]
    gaps: BTreeMap<String, u32>,
    max_wake_lat_us: u64,
    drains: u32,
    underruns: u32,
    sample: BaselineSample,
}

/// The recorded clean-tree *sample* for one config, not a summary of it. The
/// thorough tier compares a fresh sample against this one, so it needs the
/// observations themselves — see `tests/common/stats.rs` for why a summary
/// would understate the false-red rate.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineSample {
    /// Runs whose wav was analysed (the counter arrays can be longer: a run
    /// can lose its histogram and still report counters).
    gap_sample: u32,
    /// Of `gap_sample`, how many showed at least one mid-tone dropout.
    gap_runs: u32,
    /// Of the counter runs, how many breached this config's per-run ceilings.
    ceiling_runs: u32,
    max_wake_lat_us: Vec<f64>,
    underruns: Vec<f64>,
    wakes: Vec<f64>,
    /// Recorded for re-baselining the per-run ceiling only. Deliberately not
    /// tested distributionally: it is zero on 50-90% of runs, and the ties
    /// leave a rank test with no power (measured: 0.00-0.21 against a tripling).
    drains: Vec<f64>,
}

type AudioBaseline = BTreeMap<String, BTreeMap<String, AudioBaselineEntry>>;

struct ConfigBaseline<'a> {
    gaps: BTreeMap<u32, u32>,
    counters: audio::CounterLimits,
    sample: &'a BaselineSample,
}

fn load_audio_baseline() -> AudioBaseline {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio-baseline.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Baseline for one (test, smp) config. Every config must be recorded: an
/// ungated config would pass by omission.
fn config_baseline<'a>(baseline: &'a AudioBaseline, name: &str, smp: u32) -> ConfigBaseline<'a> {
    let entry = baseline
        .get(name)
        .and_then(|per_smp| per_smp.get(&format!("smp{smp}")))
        .unwrap_or_else(|| panic!("audio-baseline.toml: no [{name}.smp{smp}] section"));
    ConfigBaseline {
        sample: &entry.sample,
        gaps: entry
            .gaps
            .iter()
            .map(|(k, &count)| {
                let periods: u32 = k.parse().unwrap_or_else(|_| {
                    panic!("audio-baseline.toml: bad gap key {k:?} for {name} smp{smp}")
                });
                (periods, count)
            })
            .collect(),
        counters: audio::CounterLimits {
            max_wake_lat_us: entry.max_wake_lat_us,
            drains: entry.drains,
            underruns: entry.underruns,
        },
    }
}

/// What one audio boot measured. Both tiers are computed from this; they
/// differ only in how many they collect and what decision they take on the
/// collection.
struct AudioRun {
    gaps: BTreeMap<u32, u32>,
    counters: audio::SounddCounters,
    /// The instrument itself is untrustworthy on this run (no tone, no dither,
    /// clicks, no stats window). Never a rare-event judgement — always fatal,
    /// in both tiers.
    broken: Vec<String>,
    /// soundd counters past this config's per-run ceilings. A counted rate in
    /// the thorough tier; printed but not a verdict in the fast tier, which
    /// judges `harm` instead.
    breaches: Vec<String>,
    /// What else the host was doing while this boot was measured. Annotation
    /// only — nothing above or below branches on it.
    host: hostload::HostLoad,
}

impl AudioRun {
    /// The capture's verdict alone. The thorough tier's dropout *rate* is
    /// defined on this and nothing else, because that is what the recorded
    /// sample counted.
    fn dropped_audio(&self) -> bool {
        !self.gaps.is_empty()
    }

    /// Silence that reached the device on this run: a mid-tone gap in the
    /// capture, or a period soundd put on the wire with no client audio behind
    /// it. Both are audio someone would have heard drop out, and together they
    /// are the fast tier's whole verdict — a counter past a ceiling says the
    /// pipeline came close, and how close is a question for a distribution.
    fn harm(&self) -> Option<String> {
        let mut evidence = Vec::new();
        if self.dropped_audio() {
            evidence.push(format!("dropout {}", audio::format_histogram(&self.gaps)));
        }
        if self.counters.underruns > 0 {
            evidence.push(format!(
                "{} of {} periods submitted with no client audio",
                self.counters.underruns, self.counters.submitted
            ));
        }
        (!evidence.is_empty()).then(|| evidence.join(", "))
    }
}

/// `--slow-usb`: give every audio boot a USB stick that answers a bulk transfer
/// in 2 ms instead of microseconds — what a real stick's erase block does, and
/// what the T14's audio pops are made of.
///
/// A switch and not a test of its own, because it changes no verdict: it makes
/// the four audio configs measure a machine the host cannot otherwise present,
/// and what it produces is an A/B against the same command without it in the
/// same session. `issues/kernel/every-wait-in-this-kernel-is-a-spin.md` is what
/// the numbers are for.
static SLOW_USB: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Boot a fresh QEMU with the given CPU count, run one in-guest audio test,
/// and measure it: soundd's in-guest counters (wake lateness, pipeline drains,
/// periods of silence submitted) and the captured wav (mid-signal silence, hard
/// sample-to-sample discontinuities, and the dither the detector needs to see
/// anything at all).
///
/// `Err` means the run produced no measurement — a boot failure, a timeout, an
/// unreadable capture. That is never a rare-event judgement call; it is fatal
/// in both tiers.
fn measure_audio_run(
    name: &str,
    smp: u32,
    baseline: &ConfigBaseline,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    // Distinguishes this boot from the others of the same config in the log
    // and in the kept capture's filename; empty for a plain single boot.
    tag: &str,
) -> Result<AudioRun, String> {
    let label = if tag.is_empty() {
        String::new()
    } else {
        format!("{tag}: ")
    };
    // Bounds every duration soundd can report: its whole life is inside this
    // process's. See `audio::check_physical`.
    let run_start = std::time::Instant::now();
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            smp,
            kernel_params: if SLOW_USB.load(std::sync::atomic::Ordering::Relaxed) {
                &["usb-slow-device"]
            } else {
                &[]
            },
            ..Default::default()
        },
    );

    let result = qemu.run_test(&format!("test_rs_{name}"), Duration::from_secs(30));
    if let Some(err) = &result.error {
        return Err(err.to_string());
    }
    match result.exit_code {
        Some(0) => {}
        Some(code) => return Err(format!("exit code {code}\nstdout:\n{}", result.stdout)),
        None => return Err(format!("no exit code\nstdout:\n{}", result.stdout)),
    }

    // The wav timeline advances in real time; give the tone tail and its
    // trailing silence context time to reach the file before reading it. The
    // same wait collects soundd's final stats flush, which races the client's
    // exit and so can arrive after ===TEST_END===.
    let serial = result.serial + &qemu.drain_serial(Duration::from_millis(500));

    let wav = audio::parse_wav(qemu.audio_wav_path())?;
    let analysis = audio::analyze(&wav);
    let rate = wav.sample_rate as f64;
    let secs = |samples: usize| samples as f64 / rate;

    // Always printed, so every run leaves comparable numbers in the log.
    let gaps = audio::gap_histogram(&analysis, wav.sample_rate);
    let counters = audio::parse_soundd_counters(&serial)?;
    // Sampled here rather than before the boot because the load averages are
    // trailing: a reading taken now covers the run, one taken before it covers
    // only what preceded it. This run's own guest is still up, so `qemu 1` is
    // the quiet reading.
    let host = hostload::HostLoad::sample();
    eprintln!(
        "        {label}{name} smp={smp} gaps: {} (baseline {}) peak {} active {:.2}s dither {:.1}% \
         pitch {:.1}Hz phase-breaks {}",
        audio::format_histogram(&gaps),
        audio::format_histogram(&baseline.gaps),
        analysis.peak,
        secs(analysis.active_samples),
        analysis.dither_ratio.unwrap_or(0.0) * 100.0,
        audio::dominant_hz(&wav).unwrap_or(0.0),
        audio::phase_breaks(&wav).len(),
    );
    eprintln!(
        "        {label}{name} smp={smp} soundd: wake_lat {}us ({:.2} pipelines, limit {}us) \
         [irq {}us + pickup {}us, {} empty wakes, batch {}, {} late of {}] \
         drains {}/{} underruns {}/{} submitted {} wakes {} batch {} windows {} — {} — {host}",
        counters.max_wake_lat_us,
        counters.max_wake_lat_us as f64 / audio::PIPELINE_DEPTH_US as f64,
        baseline.counters.max_wake_lat_us,
        counters.worst.irq_late_us,
        counters.worst.pickup_us,
        counters.worst.empty,
        counters.worst.batch,
        counters.late_wakes,
        counters.wakes,
        counters.drains,
        baseline.counters.drains,
        counters.underruns,
        baseline.counters.underruns,
        counters.submitted,
        counters.wakes,
        counters.max_batch,
        counters.windows,
        audio::boot_clocks(qemu.boot_log()),
    );

    let breaches = audio::check_counters(&counters, &baseline.counters);
    if !breaches.is_empty() {
        eprintln!(
            "        {label}{name} smp={smp} over ceiling: {} — recorded; the fast tier's \
             verdict is harm, the rate of these is the thorough tier's",
            breaches.join("; ")
        );
    }

    // A counter past a physical bound is the instrument failing, so it belongs
    // here with the other instrument checks rather than among the ceilings: it
    // must fail loudly in both tiers, and it must never be ranked against the
    // recorded sample or printed into the next baseline.
    let mut problems = audio::check_physical(&counters, run_start.elapsed().as_secs_f64());
    // soundd counts only while it has clients, so a run with no window reports
    // zero for every counter — the best numbers this gate can see, from a run
    // that measured nothing. That is the instrument dead, not a ceiling held.
    if counters.windows == 0 {
        problems.push(
            "soundd printed no stats window with clients — the tone never reached the mixer"
                .to_string(),
        );
    }
    if secs(analysis.active_samples) < TONE_MIN_ACTIVE_SECS {
        problems.push(format!(
            "tone missing: only {:.2}s of active signal (expected >= {TONE_MIN_ACTIVE_SECS}s)",
            secs(analysis.active_samples)
        ));
    }
    if analysis.peak < TONE_MIN_PEAK {
        problems.push(format!(
            "tone too quiet: peak {} (expected >= {TONE_MIN_PEAK})",
            analysis.peak
        ));
    }
    // Present, loud and continuous is not the same as right: a device consuming
    // the buffers at a rate soundd did not ask for satisfies all three and plays
    // the whole session off pitch.
    if let Some(complaint) = audio::wrong_pitch(&wav) {
        problems.push(complaint);
    }
    // Without this the gate can go green while measuring nothing: the underrun
    // detector's silence band is derived from soundd applying TPDF dither into
    // a rounding quantizer (spec §5.4). Lose the dither and silence becomes
    // exact zero everywhere, the band collapses, and dropouts stop being
    // visible — the exact failure this instrument was rebuilt to remove.
    match analysis.dither_ratio {
        Some(ratio) if ratio < audio::MIN_DITHER_RATIO => problems.push(format!(
            "dither missing: only {:.1}% of silent samples are non-zero (expected ~25%, \
             floor {:.0}%) — soundd is not dithering, so the underrun detector is blind",
            ratio * 100.0,
            audio::MIN_DITHER_RATIO * 100.0
        )),
        Some(_) => {}
        None => problems.push("no silent stretch in capture to verify dither against".to_string()),
    }
    if audio::check_gap_regression(&gaps, &baseline.gaps).is_err() {
        let mut msg = format!(
            "{} mid-signal underruns (silence >= 2ms inside the tone):",
            analysis.underruns.len()
        );
        for run in analysis.underruns.iter().take(20) {
            msg.push_str(&format!(
                "\n      at {:8.3}s len {:6.2}ms",
                secs(run.start),
                secs(run.len) * 1000.0
            ));
        }
        if analysis.underruns.len() > 20 {
            msg.push_str(&format!("\n      ... and {} more", analysis.underruns.len() - 20));
        }
        eprintln!("        {label}{name} smp={smp} {msg}");
    }
    if !analysis.clicks.is_empty() {
        let mut msg = format!("{} hard discontinuities (|delta| > 8000):", analysis.clicks.len());
        for click in analysis.clicks.iter().take(10) {
            msg.push_str(&format!(
                "\n      at {:8.3}s  {} -> {}",
                secs(click.index),
                click.from,
                click.to
            ));
        }
        if analysis.clicks.len() > 10 {
            msg.push_str(&format!("\n      ... and {} more", analysis.clicks.len() - 10));
        }
        problems.push(msg);
    }

    // §5.8 suspend structure — categorical per-run assertions, so they belong
    // with the instrument checks: fatal in both tiers, never a counted rate.
    problems.extend(audio::check_suspend_structure(&serial));

    // Keep every capture that shows something, so a dropout can be listened to
    // even when the tier's rule says one occurrence is not yet a verdict.
    if !problems.is_empty() || !breaches.is_empty() || !gaps.is_empty() {
        let suffix = if tag.is_empty() {
            String::new()
        } else {
            format!("-{tag}")
        };
        let kept = qemu
            .audio_wav_path()
            .with_file_name(format!("audio-{name}-smp{smp}{suffix}.wav"));
        match fs::rename(qemu.audio_wav_path(), &kept) {
            Ok(()) => eprintln!("        {label}{name} smp={smp} wav kept at {}", kept.display()),
            Err(e) => eprintln!(
                "        {label}{name} smp={smp} could not keep {}: {e}",
                kept.display()
            ),
        }
    }

    Ok(AudioRun {
        gaps,
        counters,
        broken: problems,
        breaches,
        host,
    })
}

/// Fast tier — one boot per config, run on every `cargo test`.
///
/// Certifies: the instrument is alive, no counter is on the wrong side of a
/// physical bound, and this build does not *reproducibly* put silence on the
/// wire. It cannot certify a *rate*; one run is one Bernoulli trial against a
/// per-config dropout rate measured at 0-7%, which discriminates nothing. That
/// is what `--audio-gate` is for.
///
/// **The verdict is harm** — a mid-tone gap in the capture, or a period soundd
/// submitted with no client audio behind it. The per-run ceilings are measured,
/// printed and kept, and fail nothing here: `drains` past its ceiling with an
/// empty histogram and zero underruns is a pipeline that recovered before
/// anyone could hear it, and one boot cannot say whether it recovers less often
/// than it used to. That question has an instrument with power, and it is the
/// thorough tier's `ceiling_runs` rate.
///
/// Harm is confirmed before it fails: a run that shows any is re-booted once,
/// and only a second failure counts. No bar is widened by this — the zero-gap
/// bar is strict on both boots. Without the confirmation the per-config dropout
/// rate alone reds one invocation in eight on a clean tree, and a gate
/// developers see every day cannot cry wolf that often. The first occurrence is
/// still printed and its capture still kept.
fn run_audio_test(
    name: &str,
    smp: u32,
    baseline: &ConfigBaseline,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let run = measure_audio_run(name, smp, baseline, test_config, c_bins, rust_bins, "")?;

    if !run.broken.is_empty() {
        return Err(run.broken.join("\n    "));
    }
    let Some(harm) = run.harm() else {
        return Ok(());
    };

    let silent_runs = baseline.sample.underruns.iter().filter(|&&u| u > 0.0).count();
    eprintln!(
        "        {name} smp={smp} HARM {harm} — rare on this tree ({} of {} recorded runs \
         dropped audio, {silent_runs} of {} submitted a silent period); re-booting once \
         to confirm",
        baseline.sample.gap_runs,
        baseline.sample.gap_sample,
        baseline.sample.underruns.len(),
    );
    let again = measure_audio_run(name, smp, baseline, test_config, c_bins, rust_bins, "confirm")?;
    if !again.broken.is_empty() {
        return Err(again.broken.join("\n    "));
    }
    match again.harm() {
        Some(again_harm) => Err(format!(
            "audio dropped out on two consecutive boots: {harm} then {again_harm}"
        )),
        None => {
            eprintln!("        {name} smp={smp} not reproduced on the confirming boot");
            Ok(())
        }
    }
}

// Thorough tier: `cargo test --test toyos-build -- --audio-gate N`

/// One config's fresh sample, accumulated over the N iterations.
#[derive(Default)]
struct GateSamples {
    max_wake_lat_us: Vec<f64>,
    underruns: Vec<f64>,
    wakes: Vec<f64>,
    drains: Vec<f64>,
    gap_runs: u32,
    ceiling_runs: u32,
}

/// A rejected statistic, ready to print.
struct Rejection {
    config: String,
    statistic: String,
    detail: String,
}

fn mwu_verdict(
    config: &str,
    statistic: &str,
    base: &[f64],
    fresh: &[f64],
    worse_is_lower: bool,
) -> Option<Rejection> {
    let z = stats::mann_whitney_z(base, fresh);
    let z = if worse_is_lower { -z } else { z };
    let med = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    (z > stats::Z_CRIT).then(|| Rejection {
        config: config.to_string(),
        statistic: statistic.to_string(),
        detail: format!(
            "median {:.0} -> {:.0} (Mann-Whitney z={z:.2} > {:.2})",
            med(base),
            med(fresh),
            stats::Z_CRIT
        ),
    })
}

fn rate_verdict(
    config: &str,
    statistic: &str,
    k1: u32,
    n1: u32,
    k0: u32,
    n0: u32,
) -> Option<Rejection> {
    let p = stats::fisher_greater(k1, n1, k0, n0);
    (p <= stats::ALPHA).then(|| Rejection {
        config: config.to_string(),
        statistic: statistic.to_string(),
        detail: format!(
            "{k1} of {n1} vs recorded {k0} of {n0} (Fisher p={p:.2e} <= {:.0e})",
            stats::ALPHA
        ),
    })
}

/// Thorough tier — N iterations of all four configs, gating on *rates* and
/// *distributions* rather than on single outcomes. The nightly runs it.
///
/// Certifies, at N=30 and the measured clean-tree distributions:
///   * wake lateness has not shifted by 25% (detected 99.9% of the time) or
///     20% (93%). A 10% shift is missed (4%).
///   * periods of silence on the wire have not risen 25% (94%) or 50% (100%).
///   * soundd is not being woken less often — the signature of completions
///     being batched because it ran late. A 5% drop is caught 99.9% of the
///     time.
///   * the mid-tone dropout *rate* has not risen 10x (100%) or 5x (71%).
///     A doubling is NOT detectable at this N and never will be at any N a
///     human waits for: separating 3% from 7% at this confidence needs ~600
///     runs per config. The counters above are the instrument with power; the
///     dropout rate is the audible symptom, kept because it is the only
///     statistic here that says "someone would have heard it".
///
/// False-red rate on a clean tree: 0.25%, measured over 2000 invocations
/// simulated from the recorded distributions.
fn run_audio_gate(
    iterations: u32,
    audio_baseline: &AudioBaseline,
    audio_to_run: &[&str],
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> bool {
    let configs: Vec<(&str, u32)> = audio_to_run
        .iter()
        .flat_map(|name| AUDIO_SMP.iter().map(move |&smp| (*name, smp)))
        .collect();
    let mut samples: BTreeMap<String, GateSamples> = BTreeMap::new();
    // Session-wide rather than per-config: the host is one host, and this is
    // the sentence a re-record has to carry beside the numbers below.
    let mut host: Vec<hostload::HostLoad> = Vec::new();
    let start = std::time::Instant::now();

    eprintln!(
        "\n[gate A] {iterations} iterations x {} configs, serial. Every per-run outcome \
         becomes a rate; the verdict is on the collection, not on any one run.",
        configs.len()
    );

    for iter in 1..=iterations {
        eprintln!("  --- iteration {iter}/{iterations} ---");
        for &(name, smp) in &configs {
            let key = format!("{name}.smp{smp}");
            let baseline = config_baseline(audio_baseline, name, smp);
            let tag = format!("iter{iter:03}");
            let run = match measure_audio_run(
                name, smp, &baseline, test_config, c_bins, rust_bins, &tag,
            ) {
                Ok(run) => run,
                Err(err) => {
                    eprintln!("\n[gate A] FAILED on iteration {iter}: {key} produced no measurement: {err}");
                    eprintln!("[gate A] A run that does not complete is not a rare event to be \
                               averaged away — every known cause of one has been fixed.");
                    return false;
                }
            };
            if !run.broken.is_empty() {
                eprintln!("\n[gate A] FAILED on iteration {iter}: {key} instrument broken: {}",
                          run.broken.join("; "));
                return false;
            }
            host.push(run.host);
            let s = samples.entry(key).or_default();
            s.max_wake_lat_us.push(run.counters.max_wake_lat_us as f64);
            s.underruns.push(run.counters.underruns as f64);
            s.wakes.push(run.counters.wakes as f64);
            s.drains.push(run.counters.drains as f64);
            s.gap_runs += u32::from(run.dropped_audio());
            s.ceiling_runs += u32::from(!run.breaches.is_empty());
        }

        // Fail-side curtailment. Adding runs can only raise a count, so once a
        // count passes the threshold for the *full* N the final verdict is
        // already decided — stopping early costs no confidence.
        if let Some(v) = curtail(&samples, audio_baseline, &configs, iterations) {
            eprintln!("\n[gate A] FAILED after {iter} of {iterations} iterations (the remaining \
                       runs cannot change this):");
            eprintln!("    {} {}: {}", v.config, v.statistic, v.detail);
            return false;
        }
    }

    let mut rejected: Vec<Rejection> = Vec::new();
    let (mut pooled_gap_k, mut pooled_gap_n) = (0, 0);
    let (mut pooled_ceil_k, mut pooled_ceil_n) = (0, 0);
    let (mut base_gap_k, mut base_gap_n) = (0, 0);
    let (mut base_ceil_k, mut base_ceil_n) = (0, 0);

    eprintln!("\n[gate A] {iterations} iterations in {:.0?}. Fresh sample vs recorded sample:\n", start.elapsed());
    eprintln!("  {}\n", hostload::summarise(&host));
    for &(name, smp) in &configs {
        let key = format!("{name}.smp{smp}");
        let base = config_baseline(audio_baseline, name, smp).sample;
        let s = &samples[&key];

        rejected.extend(mwu_verdict(&key, "wake lateness", &base.max_wake_lat_us, &s.max_wake_lat_us, false));
        rejected.extend(mwu_verdict(&key, "underruns", &base.underruns, &s.underruns, false));
        rejected.extend(mwu_verdict(&key, "wakes", &base.wakes, &s.wakes, true));
        rejected.extend(rate_verdict(&key, "dropout rate", s.gap_runs, iterations, base.gap_runs, base.gap_sample));

        pooled_gap_k += s.gap_runs;
        pooled_gap_n += iterations;
        pooled_ceil_k += s.ceiling_runs;
        pooled_ceil_n += iterations;
        base_gap_k += base.gap_runs;
        base_gap_n += base.gap_sample;
        base_ceil_k += base.ceiling_runs;
        base_ceil_n += base.max_wake_lat_us.len() as u32;

        report_config(&key, base, s, iterations);
    }
    rejected.extend(rate_verdict("pooled", "dropout rate", pooled_gap_k, pooled_gap_n, base_gap_k, base_gap_n));
    rejected.extend(rate_verdict("pooled", "per-run ceiling breaches", pooled_ceil_k, pooled_ceil_n, base_ceil_k, base_ceil_n));

    eprintln!(
        "  pooled dropouts {pooled_gap_k}/{pooled_gap_n} (recorded {base_gap_k}/{base_gap_n}), \
         ceiling breaches {pooled_ceil_k}/{pooled_ceil_n} (recorded {base_ceil_k}/{base_ceil_n})"
    );

    if rejected.is_empty() {
        eprintln!("\n[gate A] PASS — no statistic regressed at alpha={:.0e} per test.", stats::ALPHA);
        true
    } else {
        eprintln!("\n[gate A] FAILED — {} statistic(s) regressed:", rejected.len());
        for v in &rejected {
            eprintln!("    {} {}: {}", v.config, v.statistic, v.detail);
        }
        false
    }
}

/// Whether a count has already passed the threshold it would face at the full
/// iteration count. Only the yes/no statistics curtail: a rank test's outcome
/// is not monotone in the sample, so there is no honest early exit for it.
fn curtail(
    samples: &BTreeMap<String, GateSamples>,
    audio_baseline: &AudioBaseline,
    configs: &[(&str, u32)],
    iterations: u32,
) -> Option<Rejection> {
    let mut pooled_gap = 0;
    let mut pooled_ceil = 0;
    let (mut base_gap_k, mut base_gap_n) = (0, 0);
    let (mut base_ceil_k, mut base_ceil_n) = (0, 0);
    for &(name, smp) in configs {
        let key = format!("{name}.smp{smp}");
        let base = config_baseline(audio_baseline, name, smp).sample;
        let Some(s) = samples.get(&key) else { continue };
        if let Some(v) = rate_verdict(&key, "dropout rate", s.gap_runs, iterations, base.gap_runs, base.gap_sample) {
            return Some(v);
        }
        pooled_gap += s.gap_runs;
        pooled_ceil += s.ceiling_runs;
        base_gap_k += base.gap_runs;
        base_gap_n += base.gap_sample;
        base_ceil_k += base.ceiling_runs;
        base_ceil_n += base.max_wake_lat_us.len() as u32;
    }
    let n = iterations * configs.len() as u32;
    rate_verdict("pooled", "dropout rate", pooled_gap, n, base_gap_k, base_gap_n)
        .or_else(|| rate_verdict("pooled", "per-run ceiling breaches", pooled_ceil, n, base_ceil_k, base_ceil_n))
}

/// Print one config's fresh sample next to the recorded one, in a form that can
/// be pasted straight back into `tests/audio-baseline.toml` when a re-baseline
/// is deliberate. The gate's output *is* the next baseline.
fn report_config(key: &str, base: &BaselineSample, s: &GateSamples, iterations: u32) {
    let stat = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[0], v[v.len() / 2], v[v.len() - 1])
    };
    eprintln!("  {key}  (n={iterations}, recorded n={})", base.max_wake_lat_us.len());
    for (label, b, f) in [
        ("wake_lat_us", &base.max_wake_lat_us, &s.max_wake_lat_us),
        ("underruns  ", &base.underruns, &s.underruns),
        ("wakes      ", &base.wakes, &s.wakes),
        ("drains     ", &base.drains, &s.drains),
    ] {
        let (bl, bm, bh) = stat(b);
        let (fl, fm, fh) = stat(f);
        eprintln!(
            "    {label} recorded {bl:.0}/{bm:.0}/{bh:.0}   fresh {fl:.0}/{fm:.0}/{fh:.0}   (min/median/max)"
        );
    }
    eprintln!(
        "    dropouts    recorded {}/{}   fresh {}/{iterations}",
        base.gap_runs, base.gap_sample, s.gap_runs
    );
    let fmt = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let v: Vec<String> = v.iter().map(|x| format!("{x:.0}")).collect();
        format!("[{}]", v.join(", "))
    };
    eprintln!("    toml: max_wake_lat_us = {}", fmt(&s.max_wake_lat_us));
    eprintln!("    toml: underruns = {}", fmt(&s.underruns));
    eprintln!("    toml: wakes = {}", fmt(&s.wakes));
    eprintln!("    toml: drains = {}", fmt(&s.drains));
}

/// Echo what the guest actually put on screen, under `--nocapture` only —
/// it is the measurement these tests are built on, and the audio gate prints
/// its numbers for the same reason.
fn print_screen(name: &str, text: &str) {
    if !qemu::VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!("        {name} decoded screen:");
    for line in text.lines() {
        eprintln!("        | {line}");
    }
}

/// Everything a photograph of a frozen machine has to carry, asked of one
/// screendump.
///
/// The three summary strings are the answer; the absence of a `[page n/m]`
/// footer is what makes one photograph the *whole* answer, because Ctrl+Alt+D
/// paints once and never enters the pager — a report that needed two pages
/// would leave the verdict on one nobody can reach. And the fill is the report
/// having taken the panel rather than sitting on a client's screen, which is
/// the half `boot_checkpoint` deliberately will not do.
fn report_is_photographable(dump: &screen::Ppm, what: &str) -> Result<(), String> {
    let text = dump.text();
    for want in ["== VERDICT:", "cpu(s) answered", "== deadlines:"] {
        if !text.contains(want) {
            return Err(format!(
                "{what} does not carry {want:?}, so a photograph of this machine answers \
                 nothing\ndecoded screen:\n{text}"
            ));
        }
    }
    if let Some(row) = dump.rows().iter().find(|r| r.contains("[page ")) {
        return Err(format!(
            "{what} is paginated ({}), and nothing advances the page after Ctrl+Alt+D — so the \
             panel is a slice of the machine's log rather than the report\ndecoded screen:\n{text}",
            row.trim()
        ));
    }
    if dump.fill() != FILL_BOOT {
        return Err(format!(
            "{what} is on a panel whose fill is {:?} — this is a client's screen with kernel \
             text on it, not the report holding the panel",
            dump.fill()
        ));
    }
    Ok(())
}

/// Assert the colour decisions `text()` cannot see: the fill, every row an
/// `alert!` produced, and one row it did not.
///
/// **Both rows are named by their text, and that is the whole assertion.**
/// Nothing in the message says "alert" any more — the colour is the record's
/// `Level` — so a version of this that picked the ordinary row as "the first
/// one that is not red" asserted only that the palette has two colours in it,
/// and passed on a panel where every row was red. The comparison row has to be
/// chosen by something the paint cannot influence, so the caller names it.
///
/// `alert_lines` is a list because **a record is not a line**: `PanicInfo`'s
/// `Display` writes `panicked at <site>:`, a newline, and then the panic's own
/// text, so one `alert!` produces two rows and both of them are the record's.
/// A renderer that counted records where the panel counts newlines painted the
/// first red and the second white, and shifted every bit below it.
fn check_colors(
    dump: &screen::Ppm,
    fill: [u8; 3],
    alert_lines: &[&str],
    plain_line: &str,
) -> Result<(), String> {
    if dump.fill() != fill {
        return Err(format!("fill is {:?}, want {fill:?}", dump.fill()));
    }
    let rows = dump.rows();
    for alert_line in alert_lines {
        let Some(cy) = dump.row_index(alert_line) else {
            return Err(format!("{alert_line:?} not on screen\n{}", dump.text()));
        };
        if dump.row_fg(cy) != Some(ALERT) {
            return Err(format!(
                "{alert_line:?} drawn in {:?}, want alert {ALERT:?} — every row of an \
                 `alert!` record wears its level, including the ones its message wrapped \
                 or newlined onto\n{}",
                dump.row_fg(cy),
                dump.text()
            ));
        }
    }
    let Some(plain) = dump.row_index(plain_line) else {
        return Err(format!(
            "{plain_line:?} is not on screen, so there is no ordinary row to compare the \
             highlight against\n{}",
            dump.text()
        ));
    };
    if dump.row_fg(plain) != Some(WHITE) {
        return Err(format!(
            "ordinary row {:?} drawn in {:?}, want white {WHITE:?}",
            rows[plain],
            dump.row_fg(plain)
        ));
    }
    Ok(())
}

/// Assert the renderer wrapped a backtrace line rather than clipping it.
///
/// The stimulus is the panic's own bottom frame: `late_panic::Nest` is a
/// generic nested in itself, so its demangled symbol is wider than any
/// console grid and its head and tail cannot share a display row. Wrap-over-
/// clip exists precisely so the symbol at the *end* of such a line survives,
/// which is why the tail is the thing asserted.
fn check_wrap(dump: &screen::Ppm) -> Result<(), String> {
    let rows = dump.rows();
    let Some(head) = dump.row_index("late_panic::Nest") else {
        return Err(format!(
            "no `late_panic::Nest` frame on screen — no over-wide symbol to wrap\n{}",
            dump.text()
        ));
    };
    if rows[head].contains("on_screen_console_check") {
        return Err(format!(
            "the frame fit one display row ({} columns); wrap is not exercised",
            rows[head].len()
        ));
    }
    if !rows[head..].iter().take(4).any(|r| r.contains("on_screen_console_check")) {
        return Err(format!(
            "the tail of the demangled symbol never reached the screen — clipped?\n{}",
            dump.text()
        ));
    }
    Ok(())
}

/// Run one screen test. `Err` carries the decoded screen, because a failure
/// here is almost always "the text is not what I expected" and the decoded
/// grid is the only readable form of that.
fn run_screen_test(
    name: &str,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    match name {
        "screen_decoder" => {
            screen::self_test();
            Ok(())
        }
        "screen_diag_boot" => {
            // The diagnostic boot mode, on the machine shape it exists for.
            // What is under test is not that the console renders —
            // `screen_late_panic` has that — but that a *successful* boot
            // leaves its log on the glass. `boot_checkpoint` is the only
            // painter on this path and it returns immediately once anything
            // claims DEVICE_FRAMEBUFFER, so on the flashed image the answer
            // to "why is the keyboard dead" was up for about a tenth of a
            // second. This image contains no process that can claim it.
            //
            // Same config file `--diag-boot` builds from, and no test binaries
            // in the initrd, so the image booted here is the image flashed.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("diag");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                // No test-runner in this image, so the kernel's own last phase
                // line is the marker. It says the ring drained, not that the
                // paint happened, which is why the screen is polled below.
                ready_marker: "Boot: complete",
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let console = qemu.boot_log().to_string();
            qemu.screendump_until("Boot: complete", Duration::from_secs(30));

            // The window the mode exists to close: on the flashed image the
            // compositor's first output landed 48 ms after `Boot: complete`.
            // Holding two orders of magnitude longer than that is what makes
            // "indefinitely" a measurement rather than a claim.
            thread::sleep(Duration::from_secs(5));
            let dump = qemu.screendump();
            let text = dump.text();
            print_screen(name, &text);

            // A fatal report carries the same log lines. Without the fill and
            // a clean console this would go green on a kernel that panicked
            // its way to the same text.
            if dump.fill() != FILL_BOOT {
                return Err(format!(
                    "screen fill is {:?}, want the boot checkpoint's {FILL_BOOT:?}\n\
                     decoded screen:\n{text}",
                    dump.fill()
                ));
            }
            serial::Serial::named("boot console", console.as_str()).must_be_clean()?;

            for want in
                ["Boot: complete", "i8042:", common::volumes::LOG_ON_CONSOLE_AND_FILE]
            {
                if !text.contains(want) {
                    return Err(format!(
                        "{want:?} is not on screen five seconds after the boot \
                         finished\ndecoded screen:\n{text}"
                    ));
                }
            }
            // `screen_log_absent`'s control. This machine's log partition
            // mounted, so nothing here may be wearing the alert marker — a
            // kernel that painted it unconditionally would satisfy that gate
            // and mean nothing.
            if let Some(row) = (0..dump.rows().len()).find(|&i| dump.row_fg(i) == Some(ALERT)) {
                return Err(format!(
                    "an alert row on a boot where everything worked: {:?}\n\
                     decoded screen:\n{text}",
                    dump.rows()[row]
                ));
            }

            // A log longer than the screen is shown as its tail, and the rule
            // is that it may never be a *silent* tail: `paint` gives an
            // overflowing text a `[page n/m]` footer and `Page::Last` numbers
            // it as the last page. So either the whole log is up, or the
            // footer says out loud that it is not. Which branch runs is a
            // property of the log's length, not of the mode — this boot fits
            // today and the footer branch is the guard for when it stops
            // fitting, which the T14's shorter panel is already close to.
            let rows = dump.rows();
            let paged = rows.iter().find(|r| r.starts_with("[page "));
            match paged {
                Some(f) => {
                    let n: Vec<&str> = f
                        .trim_start_matches("[page ")
                        .trim_end_matches(']')
                        .split('/')
                        .collect();
                    if n.len() != 2 || n[0] != n[1] {
                        return Err(format!(
                            "a boot checkpoint paints the newest page, so its footer \
                             must read [page m/m]; got {f:?}"
                        ));
                    }
                }
                None => {
                    let Some(first) = console.lines().find(|l| qemu::is_kernel_line(l)) else {
                        return Err(format!("no kernel line on the console at all:\n{console}"));
                    };
                    // A fragment rather than the line: rows are wrapped at the
                    // screen's width, and a whole line can straddle two of them.
                    let fragment: String = first.chars().skip(20).take(24).collect();
                    if !text.contains(fragment.trim()) {
                        return Err(format!(
                            "no footer, so the screen claims to hold the whole log — \
                             but its first line {first:?} is not on it\n\
                             decoded screen:\n{text}"
                        ));
                    }
                }
            }

            // And the same claim against the panel that gets flashed, which is
            // smaller than this one in both directions.
            let i8042_row = dump.row_index("i8042:").expect("checked above");
            let last_text = rows
                .iter()
                .rposition(|r| !r.is_empty() && !r.starts_with("[page "))
                .unwrap_or(0);
            let above_end = last_text.saturating_sub(i8042_row);
            if above_end >= T14_ROWS {
                return Err(format!(
                    "the first `i8042:` line is {above_end} rows above the end of the \
                     log; the T14's panel holds {T14_ROWS}, so it would not be on the \
                     flashed machine's screen at all\ndecoded screen:\n{text}"
                ));
            }
            if let Some(wide) = rows[i8042_row..=last_text]
                .iter()
                .find(|r| r.chars().count() > T14_COLS)
            {
                return Err(format!(
                    "a row inside that window is {} columns wide against the panel's \
                     {T14_COLS}; it wraps there, which pushes the `i8042:` line further \
                     up than this screen shows: {wide:?}",
                    wide.chars().count()
                ));
            }

            eprintln!("  [diag] five seconds after Boot: complete, still on screen:");
            eprintln!("  [diag]   {}", rows[i8042_row]);
            eprintln!(
                "  [diag] {above_end} rows above the end of the log; the T14 panel holds {T14_ROWS}"
            );
            eprintln!(
                "  [diag] {}",
                match paged {
                    Some(f) => format!("log longer than the screen, footer reads {f}"),
                    None => "whole log on one screen, no footer".to_string(),
                }
            );
            Ok(())
        }
        "screen_log_absent" => {
            // The machine the log partition exists for, with the log partition
            // taken away from it: metal-sim has no serial port a person can
            // read, so a `/log` that did not mount is a fact only the panel can
            // carry. Before this it was carried the way everything else is —
            // one white row, in the middle of phase 5, among sixty-seven — and
            // the owner's report was that nothing said so at all.
            //
            // The diag config for the same reason `screen_diag_boot` uses it:
            // it contains no process that can claim the framebuffer, so the
            // last boot checkpoint's paint is still up when the screendump is
            // taken. On the flashed desktop image the compositor takes the
            // screen about 48 ms after `Boot: complete`, which is what makes
            // "it is on the panel" a claim about the checkpoint and not about
            // how fast a person can look.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("diag");
            let (image_path, _, _) = common::volumes::image_with_unnamed_log_partition(
                "log-absent-boot.img",
                &config,
                &[],
                &[],
            )?;
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                boot_image: Some(image_path.clone()),
                ready_marker: "Boot: complete",
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let console = qemu.boot_log().to_string();
            let dump = qemu.screendump_until(common::volumes::NO_LOG_ALERT, Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);

            // Non-vacuity, and it is the half that matters: a boot whose log
            // partition mounted would paint the ordinary line, and a screen
            // asserted on without this would pass on a kernel that always says
            // the alarming thing.
            if !console.contains("log-volume: not mounted") {
                return Err(format!(
                    "the kernel mounted a log volume it was never given, so nothing here is \
                     about a missing /log:\n{console}"
                ));
            }
            if console.contains("logd: this boot's kernel log is") {
                return Err(format!(
                    "logd opened a file anyway — a fallback is what this must not do:\n{console}"
                ));
            }

            if !text.contains(common::volumes::NO_LOG_ALERT) {
                return Err(format!(
                    "the panel of a machine with no /log and no console says nothing about \
                     either\ndecoded screen:\n{text}"
                ));
            }
            // Red, and the rest of the screen white. `text()` throws hue away
            // by construction, so this is the only place the difference between
            // "the line is there" and "the line stands out" exists.
            check_colors(&dump, FILL_BOOT, &[common::volumes::NO_LOG_ALERT], "Boot: complete")?;
            // And it is a boot checkpoint's paint rather than a panic's: the
            // fill above says so, and the machine is still running.
            if !text.contains("Boot: complete") {
                return Err(format!(
                    "the alert is on a screen that never reached the end of the boot\n\
                     decoded screen:\n{text}"
                ));
            }
            let _ = std::fs::remove_file(&image_path);
            let row = dump.row_index(common::volumes::NO_LOG_ALERT).expect("checked above");
            eprintln!("  [log] on the panel, in alert red: {}", dump.rows()[row]);
            Ok(())
        }
        "screen_console_shell" => {
            // The third boot mode, on the machine shape that gets flashed.
            // What is under test is the whole chain a question travels on a
            // machine with no serial port: the i8042 pin, the kernel's
            // translation, `/bin/console`, the shell's stdin, its stdout, and
            // the panel. **A test that asserted only that a prompt rendered
            // would pass on a console that cannot read the keyboard**, which
            // is exactly the path this program exists to bring up.
            //
            // Same config file `--console-boot` builds from and no test
            // binaries in the initrd, so the image booted here is the image
            // flashed — the property `screen_diag_boot` has for its mode.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("console");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                ready_marker: "console: ready",
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let console = qemu.boot_log().to_string();
            serial::Serial::named("boot console", console.as_str()).must_be_clean()?;

            let font = screen::ConsoleFont::load();
            let dump = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| d.console_text(&font).contains(CONSOLE_PROMPT),
            );
            let before = dump.console_text(&font);
            if !before.contains(CONSOLE_PROMPT) {
                return Err(format!(
                    "no {CONSOLE_PROMPT:?} on the panel 30 s after `console: ready`\n\
                     decoded screen:\n{before}"
                ));
            }

            // The seed. Claiming DEVICE_FRAMEBUFFER stops `boot_checkpoint`
            // painting for the rest of the boot, so a console that merely
            // cleared the screen would have traded the diagnostic that works
            // today for one that might — and this is the line the metal track
            // keeps having to read.
            if !before.contains("i8042:") {
                return Err(format!(
                    "no `i8042:` line above the prompt: `/boot/toyos/kernel.log` never \
                     reached the scrollback, so this console starts blank where the \
                     diagnostic boot starts with the log\ndecoded screen:\n{before}"
                ));
            }
            // Non-vacuity, and not a formality: a boot checkpoint paints the
            // same lines off the same ring, so on a boot where the console
            // never ran the assertion above could be satisfied by the kernel's
            // own paint. It cannot, because that paint is in `font8x16.bin`
            // and this screen decodes under the console's — which is a claim,
            // so it is checked here and in `console_self_test` rather than
            // assumed.
            let kernel_font = dump.text();
            if kernel_font.contains("i8042:") {
                return Err(format!(
                    "the kernel's own font decodes this screen, so what is up is a boot \
                     checkpoint and not the console's paint\ndecoded screen:\n{kernel_font}"
                ));
            }

            {
                let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                input.type_text(&format!("echo {CONSOLE_NONCE}\n"));
            }

            let dump = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| d.console_rows(&font).iter().any(|r| r.trim() == CONSOLE_NONCE),
            );
            let after = dump.console_text(&font);
            print_screen(name, &after);
            // A whole trimmed row, because the shell echoes what is typed:
            // `contains` would be satisfied by `/home/root> echo zqjxk`, which
            // says the console drew a keystroke and nothing about anything
            // having run.
            if !dump.console_rows(&font).iter().any(|r| r.trim() == CONSOLE_NONCE) {
                return Err(format!(
                    "typed `echo {CONSOLE_NONCE}` at the prompt and no row of the panel is \
                     its output; the keyboard, the shell or the console did not carry it\n\
                     decoded screen:\n{after}"
                ));
            }
            if !after.contains(&format!("{CONSOLE_PROMPT} echo {CONSOLE_NONCE}")) {
                return Err(format!(
                    "the output is on screen but the echoed command line is not, so the \
                     console is not showing what was typed\ndecoded screen:\n{after}"
                ));
            }
            let rows = dump.console_rows(&font);
            let log_rows = rows.iter().filter(|r| r.contains("[kernel ")).count();
            eprintln!(
                "  [console] {log_rows} kernel log rows above a prompt, and `echo \
                 {CONSOLE_NONCE}` typed on the i8042 answered on the panel"
            );
            Ok(())
        }
        "screen_console_clear" => {
            // `clear` is the one command whose entire output is the *absence*
            // of output, which is why nothing else in the suite covers it:
            // every other screen assertion looks for something that should be
            // on the panel, and passes whether or not anything else is up
            // there with it. This one asserts what must *not* be there, and
            // the console is the caller that has to get it right — on the
            // machine it is for there is no scrollbar to drag and no second
            // window to read from.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("console");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_features: ACTUATOR_KERNEL,
                ready_marker: "console: ready",
                ..Default::default()
            };
            let mut qemu =
                QemuInstance::boot_with_options(&config, c_bins, rust_bins, options);
            let font = screen::ConsoleFont::load();

            let before = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| d.console_text(&font).contains(CONSOLE_PROMPT),
            );
            let before_text = before.console_text(&font);
            if !before_text.contains(CONSOLE_PROMPT) {
                return Err(format!(
                    "no prompt to clear\ndecoded screen:\n{before_text}"
                ));
            }
            // The premise. Clearing a screen that was already blank asserts
            // nothing, and the seeded kernel log is what fills it.
            let filled = before.console_rows(&font).iter().filter(|r| !r.is_empty()).count();
            if filled < 10 {
                return Err(format!(
                    "only {filled} non-blank rows before `clear`, so there was nothing to \
                     leave behind\ndecoded screen:\n{before_text}"
                ));
            }

            // Draw on the glass behind the console's back, which is the state
            // `clear` exists to get a user out of and the one a damage-tracked
            // console can talk itself out of repairing.
            {
                let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                input.type_text("test_rs_test_screen_graffiti\n");
            }
            // Settle on the strip below the last cell row rather than on the
            // whole panel: the console goes on drawing -- the command echoes,
            // the shell reprints its prompt -- so most of the glass is being
            // repainted while this waits, and only the strip no cell covers
            // holds still.
            let margin = |d: &screen::Ppm| d.height % screen::GLYPH_H;
            let margin_is = |d: &screen::Ppm, c: [u8; 3]| {
                let m = margin(d);
                m > 0
                    && d.pixels[(d.height - m) * d.width..].iter().all(|p| *p == c)
            };
            let painted_over = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| margin_is(d, GRAFFITI),
            );
            // Non-vacuity, in the two places it can be lost. A panel that is a
            // whole number of glyph rows tall has no strip at all, and would
            // make half of what follows assert nothing -- 2048x2048, the
            // default this profile used to boot, is exactly that panel.
            if margin(&painted_over) == 0 {
                return Err(format!(
                    "this panel is {}x{}, a whole number of {}px glyph rows, so the strip this \
                     test is half about does not exist here",
                    painted_over.width, painted_over.height, screen::GLYPH_H
                ));
            }
            // And if the kernel never reached the glass there is nothing for
            // `clear` to fail to remove.
            let green = painted_over.pixels.iter().filter(|p| **p == GRAFFITI).count();
            if !margin_is(&painted_over, GRAFFITI) || green * 2 < painted_over.pixels.len() {
                return Err(format!(
                    "the graffiti actuator did not reach the panel: {green} of {} pixels are \
                     {GRAFFITI:?} and the {}px strip below the cells is {}",
                    painted_over.pixels.len(),
                    margin(&painted_over),
                    if margin_is(&painted_over, GRAFFITI) { "green" } else { "not" }
                ));
            }

            {
                let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                input.type_text("clear\n");
            }

            // `clear` is `ESC[2J ESC[H`, after which the shell reprints its
            // prompt at the home position. So the whole panel is one row of
            // prompt and nothing else -- wait for that, then assert it, so a
            // slow paint reads as a failure rather than as a pass on a screen
            // that had not finished.
            let only_prompt = |d: &screen::Ppm| {
                let rows = d.console_rows(&font);
                rows.first().is_some_and(|r| r.trim() == CONSOLE_PROMPT)
                    && rows[1..].iter().all(|r| r.is_empty())
            };
            let dump = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                only_prompt,
            );
            let after = dump.console_text(&font);
            print_screen(name, &after);

            // The pixel assertion first, because it is the specific one: a
            // screen still covered in paint fails the prompt check too, and
            // that message would send the next reader after the shell.
            if let Some(i) = dump.pixels.iter().position(|p| *p == GRAFFITI) {
                let (x, y) = (i % dump.width, i / dump.width);
                let m = dump.height % screen::GLYPH_H;
                let where_ = if y >= dump.height - m {
                    format!("the {m}px strip below the last cell row, which no cell covers")
                } else {
                    format!("cell ({}, {})", x / screen::GLYPH_W, y / screen::GLYPH_H)
                };
                let left = dump.pixels.iter().filter(|p| **p == GRAFFITI).count();
                return Err(format!(
                    "{left} pixels survived `clear`, the first at ({x}, {y}) — {where_}.\n\
                     ESC[2J promises a blank panel; a repaint that skips every cell whose \
                     contents already matched what it believed was there does not deliver one, \
                     and the cells it skips are exactly the ones a user cannot fix any other \
                     way\ndecoded screen:\n{after}"
                ));
            }

            let rows = dump.console_rows(&font);
            if !rows.first().is_some_and(|r| r.trim() == CONSOLE_PROMPT) {
                return Err(format!(
                    "`clear` did not leave the prompt on the home row\n\
                     decoded screen:\n{after}"
                ));
            }
            let survivors: Vec<String> = rows[1..]
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.is_empty())
                .map(|(i, r)| format!("    row {}: {r}", i + 1))
                .collect();
            if !survivors.is_empty() {
                return Err(format!(
                    "{} rows survived `clear`:\n{}\ndecoded screen:\n{after}",
                    survivors.len(),
                    survivors.join("\n")
                ));
            }

            // Not the cell grid but the pixels outside it. A panel whose
            // height is not a whole number of glyph rows has a strip along the
            // bottom that no cell covers, and a console that paints only its
            // cells never writes there -- so whatever drew last, the kernel's
            // last boot checkpoint, stays for the life of the session. Black
            // on black hides it on the machine that found this; a fill that is
            // not black does not.
            eprintln!(
                "  [clear] {}x{}: {} cell rows and a {}px strip below them, none of it left \
                 painted",
                dump.width,
                dump.height,
                dump.height / screen::GLYPH_H,
                dump.height % screen::GLYPH_H
            );
            Ok(())
        }
        "screen_console_scroll" => {
            // The standing check on the emulator's delivery: not "did the
            // right thing appear" but "is the glass exactly what the model
            // says it is", asserted over a workload built to break it.
            //
            // What closed #90 was the owner reporting prior text surviving in
            // the middle of a cleared screen, which means cells the model had
            // written off still held glyphs. `clear` was where he noticed it;
            // this asserts every row of the panel character for character
            // after the scrolling stops, so a single stale glyph fires it at
            // the batch that produced it, with no `clear` needed to expose it.
            //
            // Line lengths vary, past the panel's width as well as under it:
            // the cells a scroll must clear are the ones past the end of a
            // line that replaces a longer one, and a line wider than the panel
            // is the only way one logical line scrolls the screen twice. Batch
            // sizes drift against the row count, and the last round arrives as
            // one block.
            //
            // **The workload is sized by what it must cover, not by a line
            // count.** `test_screen_churn` documents the construction; what
            // this end of it relies on is that any `cols` consecutive lines
            // end in every column of the panel once, and that one line in
            // eight wraps twice — so three rounds walking *disjoint* stretches
            // of 260 lines between them cover both, and a longer run buys the
            // same states again at other alignments. That is not free: the
            // guest recomposes the whole panel for every batch the console
            // reads, measured at 0.21 ms per byte of output under TCG, so the
            // cost of this test is its byte count and nothing else.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("console");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_features: ACTUATOR_KERNEL,
                ready_marker: "console: ready",
                ..Default::default()
            };
            let mut qemu =
                QemuInstance::boot_with_options(&config, c_bins, rust_bins, options);
            let font = screen::ConsoleFont::load();

            let before = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| d.console_text(&font).contains(CONSOLE_PROMPT),
            );
            if !before.console_text(&font).contains(CONSOLE_PROMPT) {
                return Err(format!(
                    "no prompt to churn from\ndecoded screen:\n{}",
                    before.console_text(&font)
                ));
            }
            let rows = before.height / screen::GLYPH_H;
            let cols = before.width / screen::GLYPH_W;

            // The same lines `test_screen_churn` prints. Duplicated
            // deliberately: a reference taken from the guest would agree with
            // the guest about a defect they shared.
            let wraps = [0usize, 1, 0, 2, 0, 1, 0, 0];
            let churn_line = |i: usize| -> String {
                let body = 5 + (i * 37) % cols + cols * wraps[i % wraps.len()];
                let fill = char::from(b'a' + (i % 26) as u8);
                let mid: String = std::iter::repeat_n(fill, body).collect();
                format!("L{i:04} {mid} E{i:04}")
            };
            // A logical line wider than the panel occupies more than one row.
            // The emulator wraps when a character arrives at a full row, so a
            // line of exactly `cols` takes one row and not two.
            let display_rows = |line: &str| -> Vec<String> {
                let ch: Vec<char> = line.chars().collect();
                if ch.is_empty() {
                    return vec![String::new()];
                }
                ch.chunks(cols).map(|c| c.iter().collect()).collect()
            };

            // Disjoint stretches tiling one run longer than the panel is wide,
            // so every column of it is the last column of some line. Each
            // round prints more than a panel's worth of rows, so the screen it
            // is asserted on holds nothing from the round before.
            let rounds = [
                (1usize, 0usize, 100usize, 7usize),
                (2, 100, 60, 7),
                (3, 160, 100, 0),
            ];
            assert!(
                rounds.windows(2).all(|w| w[0].1 + w[0].2 == w[1].1)
                    && rounds.iter().map(|r| r.2).sum::<usize>() >= cols,
                "the rounds must tile one run of at least {cols} lines, or some column of \
                 the panel is never the end of a line and the cells past it are never at risk"
            );
            for (round, start, count, chunk) in rounds {
                if round == 2 {
                    // Page back into history and return, mixing the scrollback
                    // view into the same session before more live output. The
                    // view offset changes what every row of the panel means,
                    // and it is the one input the damage pass takes that the
                    // cell grid does not.
                    let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                    for _ in 0..3 {
                        input.keys(&[("pgup", true), ("pgup", false)]);
                        thread::sleep(Duration::from_millis(250));
                    }
                    for _ in 0..2 {
                        input.keys(&[("pgdn", true), ("pgdn", false)]);
                        thread::sleep(Duration::from_millis(250));
                    }
                }
                {
                    let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                    input.type_text(&format!(
                        "test_rs_test_screen_churn {start} {count} {chunk} {cols}\n"
                    ));
                }
                // When the round is over is a different question from whether
                // the panel is right, and asking the panel both at once is how
                // a broken panel used to spend the whole timeout and then
                // report that a marker never arrived. The console writes the
                // glass before it mirrors the same bytes to its own stdout, so
                // the marker on the console stream means that batch is painted
                // — whatever it painted. The prompt is not on the stream: the
                // shell writes it without a newline, so nothing line-oriented
                // ever sees it, and the bottom row is what says the child has
                // exited.
                //
                // The wait is the guest's own: a round is a hundred lines of
                // console traffic, so silence is a console that stopped and
                // never a console that is behind. It used to be 45 s of host
                // clock, and `round 1: the guest never printed CHURN-DONE` at
                // 598 s in the wide phase was that number expiring rather than
                // anything about this panel (`issues/build/`).
                let done = format!("CHURN-DONE {start} {count}");
                let mut printed = String::new();
                if let Err(why) = await_guest(
                    &mut qemu,
                    &mut printed,
                    &format!("round {round} to print `{done}`"),
                    |seen| seen.contains(&done),
                ) {
                    return Err(format!("{why}\nround {round} printed:\n{printed}"));
                }
                let settled = |d: &screen::Ppm| {
                    d.console_rows(&font)
                        .last()
                        .is_some_and(|l| l.trim_end().starts_with(CONSOLE_PROMPT))
                };
                let dump =
                    qemu.screendump_while(Duration::from_secs(15), Duration::from_millis(100), settled);
                let decoded = dump.console_rows(&font);
                let text = dump.console_text(&font);
                if !settled(&dump) {
                    return Err(format!(
                        "{STALLED} round {round}: the prompt never came back to the bottom row, \
                         so the panel was still being painted when it was read\ndecoded screen:\n\
                         {text}"
                    ));
                }
                if !decoded.iter().any(|l| l.trim() == done) {
                    return Err(format!(
                        "round {round}: `{done}` never reached the panel\ndecoded screen:\n{text}"
                    ));
                }

                // Expand every line this round printed into the rows it
                // occupies, then take the tail the panel holds. Built from the
                // whole round rather than from a guess at how many lines fit,
                // because a wrapped line makes those different numbers.
                let mut all: Vec<String> = Vec::new();
                for i in start..start + count {
                    all.extend(display_rows(&churn_line(i)));
                }
                all.push(done.clone());
                if all.len() < rows {
                    return Err(format!(
                        "round {round}: {count} lines occupy {} rows, which does not fill a \
                         {rows}-row panel — what is left on it belongs to the round before, \
                         and this round would be asserted against rows it never printed",
                        all.len()
                    ));
                }
                let want: Vec<String> = all[all.len() - (rows - 1)..].to_vec();

                for (r, expect) in want.iter().enumerate() {
                    let got = decoded[r].trim_end();
                    if got == expect.trim_end() {
                        continue;
                    }
                    let col = got
                        .chars()
                        .zip(expect.chars())
                        .position(|(a, b)| a != b)
                        .unwrap_or(expect.chars().count().min(got.chars().count()));
                    let longer = got.chars().count() > expect.trim_end().chars().count();
                    return Err(format!(
                        "round {round}: panel row {r} is not what the console holds.\n\
                         first difference at column {col}{}\n\
                         want: {expect:?}\n\
                         got:  {got:?}\n\
                         The glass disagrees with the model, so a cell was written off as \
                         delivered without being blitted\ndecoded screen:\n{text}",
                        if longer {
                            " — the row on screen is LONGER than the line that belongs there, so \
                             what is past its end is left over from before"
                        } else {
                            ""
                        }
                    ));
                }
                let last = decoded[rows - 1].trim_end();
                if !last.starts_with(CONSOLE_PROMPT) {
                    return Err(format!(
                        "round {round}: the prompt is not on the bottom row, it reads {last:?}\n\
                         decoded screen:\n{text}"
                    ));
                }
                eprintln!(
                    "  [scroll] round {round}: lines {start}..{} at {} per flush, all {} rows \
                     match the model character for character",
                    start + count,
                    if chunk == 0 { count } else { chunk },
                    rows - 1
                );
            }
            Ok(())
        }
        "screen_console_panic" => {
            // Does claiming the framebuffer silence the panic report? Read off
            // the code the answer is no — `render` ignores
            // SCREEN_OWNED_BY_USERLAND entirely and only `boot_checkpoint`
            // honours it — but nothing in the suite had ever staged the state
            // that answers it: `screen_fatal_halt` boots `tests/testcases`,
            // whose init list contains no framebuffer claimer at all, so the
            // flag is false on every screen test that panics.
            //
            // Staged the real way round: the panic is triggered *through the
            // console*, by typing at its prompt, so the screen the report has
            // to paint over is a screen a userland process drew and owns.
            // Unlike `screen_console_shell` this one carries the test binaries
            // and a kernel feature, so it is not the flashed image — what it
            // certifies is the kernel's behaviour, not the artifact.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("console");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_features: ACTUATOR_KERNEL,
                ready_marker: "console: ready",
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu =
                QemuInstance::boot_with_options(&config, c_bins, rust_bins, options);

            let font = screen::ConsoleFont::load();
            let before = qemu.screendump_while(
                Duration::from_secs(30),
                Duration::from_millis(200),
                |d| d.console_text(&font).contains(CONSOLE_PROMPT),
            );
            // The premise. Without a console-drawn screen underneath, a
            // report reaching the panel proves nothing about ownership and
            // this test would be `screen_fatal_halt` on a different config.
            if !before.console_text(&font).contains(CONSOLE_PROMPT) {
                return Err(format!(
                    "no console prompt to panic over\ndecoded screen:\n{}",
                    before.console_text(&font)
                ));
            }

            {
                let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                input.type_text("test_rs_test_panic_child 3\n");
            }

            let dump = qemu.screendump_until(FATAL_HALT_NONCE, Duration::from_secs(40));
            let text = dump.text();
            print_screen(name, &text);
            if !text.contains(FATAL_HALT_NONCE) {
                return Err(format!(
                    "the fatal report never took the screen back from the console — which \
                     would make `/bin/console` a downgrade on the machine it is for\n\
                     decoded screen (kernel font):\n{text}\n\
                     decoded screen (console font):\n{}",
                    dump.console_text(&font)
                ));
            }
            // The fill is what says the report repainted the *whole* screen
            // rather than landing in a corner of the console's.
            if dump.fill() != FILL_FATAL {
                return Err(format!(
                    "the report is on screen but the fill is {:?}, not the fatal {FILL_FATAL:?}",
                    dump.fill()
                ));
            }
            if dump.console_text(&font).contains(CONSOLE_PROMPT) {
                return Err(format!(
                    "the console's prompt survived the report, so the panic painted over \
                     part of the screen and left the rest\ndecoded screen:\n{text}"
                ));
            }
            eprintln!(
                "  [console] the fatal report took the screen back from a userland owner"
            );
            Ok(())
        }
        "screen_i8042_health" => {
            // The health verdict on the only machine that needs it on glass: no
            // 16550, no virtio-console, so the log ring has nowhere to drain and
            // the panel is the whole diagnostic. Nothing in this image claims
            // DEVICE_FRAMEBUFFER, which is the other half of the condition.
            //
            // Not a panic: `screen_late_panic` covers the fatal path, and what
            // is under test here is a *successful* boot repainting to say
            // something the last boot checkpoint could not have known yet.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                mute: true,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            metal_sim_argv_check(&argv)?;
            match argv.iter().position(|a| a == "-serial") {
                Some(i) if argv.get(i + 1).is_some_and(|v| v == "none") => {}
                _ => return Err(format!("the muted profile still has a 16550: {argv:?}")),
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            // The verdict waits for a CPU with nothing left to run, so it lands
            // after the last boot checkpoint by construction. 30s covers
            // firmware plus the initrd read off USB.
            let dump = qemu.screendump_until("never asserted", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            if !text.contains("never asserted") {
                return Err(format!(
                    "the i8042 health verdict never reached the panel of a guest with no \
                     console at all\ndecoded screen:\n{text}"
                ));
            }
            // A panic carries the log tail too, and would satisfy the search
            // above while meaning something entirely different.
            if dump.fill() != FILL_BOOT {
                return Err(format!(
                    "screen fill is {:?}, want the boot checkpoint's {FILL_BOOT:?} — this is \
                     a panic report, not a health verdict\ndecoded screen:\n{text}",
                    dump.fill()
                ));
            }
            // The line the verdict follows on from must still be there: a
            // repaint that dropped the boot log would be a worse diagnostic
            // than no repaint.
            if !text.contains("Boot: complete") {
                return Err(format!(
                    "the repaint lost the boot log it was supposed to extend\n\
                     decoded screen:\n{text}"
                ));
            }
            let row = dump.row_index("never asserted").expect("checked above");
            eprintln!("  [i8042] on the panel of a console-less guest: {}", dump.rows()[row]);
            Ok(())
        }
        "screen_panic_muted" => {
            // The machine the whole M0/M1 line exists for: metal-sim with the
            // 16550 taken away, so `uart_present()` is false, `panic_flush`
            // returns without draining anywhere, and the rendered screen is
            // the only channel the report can possibly reach. Same kernel
            // feature and same image as `screen_late_panic`, so this costs a
            // boot and no rebuild — and it is the one place the absent-UART
            // branches run at all.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                mute: true,
                kernel_params: &["test-late-panic"],
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            metal_sim_argv_check(&argv)?;
            match argv.iter().position(|a| a == "-serial") {
                Some(i) if argv.get(i + 1).is_some_and(|v| v == "none") => {}
                _ => return Err(format!("the muted profile still has a 16550: {argv:?}")),
            }
            if argv.iter().any(|a| a.contains("stdio")) {
                return Err(format!("the muted profile still has a stdio chardev: {argv:?}"));
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            // Nothing announces the panic here — there is no console for a
            // marker to arrive on — so the screen is polled until it carries
            // the report. 30s covers firmware plus the initrd read off USB.
            let dump = qemu.screendump_until("PANIC:", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["PANIC:", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!(
                        "{want:?} not on screen of a guest with no serial port at all\ndecoded screen:\n{text}"
                    ));
                }
            }
            check_colors(
                &dump,
                FILL_FATAL,
                &["PANIC:", "test-late-panic: on-screen console check"],
                "late_panic::Nest",
            )?;
            Ok(())
        }
        "screen_early_panic" => {
            // The window the console exists for: percpu is not up, mm::init
            // has not run, and on a machine with no UART nothing else can
            // report at all. render() runs before panic_flush, so the marker
            // reaching the UART proves the paint already finished — no sleep.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_params: &["test-early-panic"],
                    ready_marker: "EARLY PANIC:",
                    ..Default::default()
                },
            );
            let dump = qemu.screendump();
            let text = dump.text();
            print_screen(name, &text);
            for want in ["EARLY PANIC:", "test-early-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(
                &dump,
                FILL_FATAL,
                &["EARLY PANIC:", "test-early-panic: on-screen console check"],
                "PAT:",
            )?;
            Ok(())
        }
        "screen_late_panic" => {
            // The ordinary fatal panic, which no userland process can produce:
            // crash_report, capture, panic_flush, halt_all_cpus, render. The
            // flush drains the ring before the paint, so the snapshot capture()
            // took is the only thing left to paint from.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_params: &["test-late-panic"],
                    ready_marker: "PANIC:",
                    ..Default::default()
                },
            );
            // Here the marker reaches serial *before* the paint — the drain is
            // what emits it — so unlike the halt paths this one has to look
            // more than once. And once the report outgrows one screen the
            // pager cycles it, so the window in which any given page is up is
            // `PAGE_HOLD_NS`, not forever: the timeout has to cover a whole
            // cycle rather than just the paint.
            let dump = qemu.screendump_until("PANIC:", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["PANIC:", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(
                &dump,
                FILL_FATAL,
                &["PANIC:", "test-late-panic: on-screen console check"],
                "late_panic::Nest",
            )?;
            check_wrap(&dump)?;
            Ok(())
        }
        "screen_paged_scrollback" => {
            // The screen is smaller than the report, and on the target laptop
            // there is no key to press for the rest of it. So the claim under
            // test is not "the console renders" — `screen_late_panic` has that
            // — but "a line the report page cannot hold reaches the screen
            // anyway, with no input". Same feature and image as
            // `screen_late_panic`, so it costs a boot and no rebuild.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_params: &["test-late-panic"],
                    ready_marker: "PANIC:",
                    ..Default::default()
                },
            );

            // The first kernel line of the boot, and the one a photograph of
            // the final screen has never been able to show.
            const HEAD: &str = "panic console: armed";
            const TAIL: &str = "PANIC:";

            let mut pages: Vec<String> = Vec::new();
            let mut report: Option<String> = None;
            let mut head_seen = false;
            // A liveness ceiling on a machine that is halted and paging, so
            // there is no console to read progress off and this is the case
            // `qemu::budget` exists for.
            let deadline = Instant::now() + qemu::budget(Duration::from_secs(40));
            while Instant::now() < deadline && !(head_seen && report.is_some()) {
                let text = qemu.screendump().text();
                let Some(footer) = text.lines().rev().find(|l| l.starts_with("[page ")) else {
                    // Before the panic the screen still carries a boot
                    // checkpoint; only a paginated screen has a footer.
                    thread::sleep(Duration::from_millis(200));
                    continue;
                };
                if !pages.contains(&footer.to_string()) {
                    pages.push(footer.to_string());
                }
                if text.contains(TAIL) {
                    report = Some(text.clone());
                }
                head_seen |= text.contains(HEAD);
                thread::sleep(Duration::from_millis(200));
            }

            let seen = pages.join(" ");
            print_screen(name, &format!("footers seen: {seen}"));
            let Some(report) = report else {
                return Err(format!(
                    "{STALLED} {TAIL:?} never reached the screen; footers seen: {seen}"
                ));
            };
            // The premise. If one screen holds both ends there is nothing to
            // page and the rest of this test would pass vacuously — which is
            // the shape the metal-track review kept finding.
            if report.contains(HEAD) {
                return Err(format!(
                    "one screen holds both {HEAD:?} and {TAIL:?}; nothing to page\n{report}"
                ));
            }
            if !head_seen {
                return Err(format!(
                    "{HEAD:?} never reached the screen — the pager did not advance past the \
                     report. footers seen: {seen}\nreport page:\n{report}"
                ));
            }
            if pages.len() < 2 {
                return Err(format!(
                    "only one page footer ever appeared ({seen}); the pager is not cycling"
                ));
            }
            Ok(())
        }
        "screen_pager_keys" => {
            // The halted pager takes PageDown off the i8042 with every
            // CPU stopped, and this is the only place that claim can be made:
            // the decode is `toyos-ps2`'s and host-tested, but that a keystroke
            // reaches a machine which has stopped scheduling is a fact about
            // the controller and the poll, not about the table.
            //
            // `Profile::Metal` because QEMU routes injected keys to one handler
            // per device class: every profile with a `usb-kbd` sends them there
            // instead, and this is the only GOP machine without one.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    kernel_params: &["test-late-panic"],
                    ready_marker: "PANIC:",
                    ..Default::default()
                },
            );
            let socket = qemu.qmp_socket().to_path_buf();

            // The footer only exists once the report overflows the screen, so
            // waiting for one is waiting for the pager to be the thing on
            // screen. `page_forever` returns without looping below two pages.
            // Retried, because a dump taken while the pager is repainting
            // catches a half-written bottom row and no footer at all.
            let footer = |q: &mut QemuInstance| {
                for _ in 0..4 {
                    let text = q.screendump().text();
                    if let Some(f) = text.lines().rev().find(|l| l.starts_with("[page ")) {
                        return Some(f.to_string());
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                None
            };
            let deadline = Instant::now() + qemu::budget(Duration::from_secs(30));
            let mut last = loop {
                if let Some(f) = footer(&mut qemu) {
                    break f;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{STALLED} no `[page n/m]` footer ever appeared; nothing was paging"
                    ));
                }
            };

            // How long the unattended deadline actually takes to move the page,
            // measured before a key is pressed because the first key retires it
            // for good. This is what stops the last phase passing vacuously: a
            // guest too slow to have paged in its window would prove nothing by
            // not paging, and this is the window measured on *this* guest.
            let timing_from = Instant::now();
            let unattended_move = loop {
                let Some(now) = footer(&mut qemu) else {
                    return Err(format!(
                        "{STALLED} the footer vanished while timing the unattended deadline"
                    ));
                };
                if now != last {
                    last = now;
                    break timing_from.elapsed();
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{STALLED} the pager did not advance on its own in {:.1}s against a 3s \
                         deadline — nothing here can say whether a keystroke stops it",
                        timing_from.elapsed().as_secs_f64()
                    ));
                }
            };

            // **One keystroke, then its page, then the next keystroke.** The
            // verdict is that every one of them moved the page, and there is no
            // clock of the host's in it: a guest that is slow costs this run
            // wall clock and never a move.
            //
            // It used to inject all thirty at the host's own speed and compare
            // the moves it saw against what a 3 s deadline could have produced
            // in the elapsed time — `moved >= elapsed/3 + 1` times three. That
            // arithmetic asks a guest which has not been given time to repaint
            // once for three moves, so on a host that got through the thirty in
            // 0.3 s it demanded 3.3 of them and reported `0 page moves over 30
            // keystrokes in 0.3s`: the symptom, where the fact was that nothing
            // had run. Two agents bisected that as a kernel regression on one
            // day. Unpaced it was wrong about the wire as well — thirty
            // press/release pairs is sixty scancodes into QEMU's 16-byte
            // `PS2_QUEUE_SIZE` (`hw/input/ps2.c`), so the keys a full-panel
            // repaint had no room for were never delivered at all.
            //
            // What makes "every key moved it" the whole claim, with no rate
            // beside it, is the phase below: after the first keystroke the
            // deadline is retired for good, so it contributes no move to this
            // loop, and if it were still running the steered page would not hold.
            const SAMPLES: usize = 30;
            let started = Instant::now();
            for key in 1..=SAMPLES {
                qemu::qmp_send_keys(&socket, &[("pgdn", true), ("pgdn", false)]);
                let by = Instant::now() + qemu::budget(Duration::from_secs(20));
                loop {
                    let Some(now) = footer(&mut qemu) else {
                        return Err(format!(
                            "{STALLED} the footer vanished after {} of {SAMPLES} keystrokes",
                            key - 1
                        ));
                    };
                    if now != last {
                        last = now;
                        break;
                    }
                    if Instant::now() >= by {
                        return Err(format!(
                            "keystroke {key} of {SAMPLES} left the pager on {last:?}: a PageDown \
                             reached a halted machine and no page came of it"
                        ));
                    }
                }
            }
            let elapsed = started.elapsed();

            // Nothing is in flight — the loop above did not send a key until the
            // page the one before it moved was on the screen — so this asks only
            // that the panel is not mid-repaint before the watch starts.
            const SETTLED: Duration = Duration::from_secs(1);
            let settle_by = Instant::now() + qemu::budget(Duration::from_secs(20));
            let mut held = last;
            let mut stable_since = Instant::now();
            loop {
                let Some(now) = footer(&mut qemu) else {
                    return Err(format!(
                        "{STALLED} the footer vanished while the last page settled"
                    ));
                };
                if now != held {
                    held = now;
                    stable_since = Instant::now();
                } else if stable_since.elapsed() >= SETTLED {
                    break;
                }
                if Instant::now() >= settle_by {
                    return Err(format!(
                        "the pager never held one page for {}s after the last keystroke, so \
                         something is still moving it",
                        SETTLED.as_secs()
                    ));
                }
            }

            // And now the owner's complaint, which is the other half: a page he
            // steered to must stay up. The window is twice what the unattended
            // deadline was measured to need above, so a pager still running it
            // moves at least twice inside this and a slow guest cannot pass by
            // being slow.
            let quiet = unattended_move * 2 + Duration::from_secs(1);
            let watching_from = Instant::now();
            while watching_from.elapsed() < quiet {
                let Some(now) = footer(&mut qemu) else {
                    return Err("the footer vanished while watching a steered page".into());
                };
                if now != held {
                    return Err(format!(
                        "the page moved from {held:?} to {now:?} on its own {:.1}s into a {:.1}s \
                         watch after the last keystroke — the deadline is still running under a \
                         reader who has taken the wheel, which is what it must not do",
                        watching_from.elapsed().as_secs_f64(),
                        quiet.as_secs_f64()
                    ));
                }
            }
            print_screen(
                name,
                &format!(
                    "every one of {SAMPLES} keystrokes moved the page, in {:.1}s; unattended it \
                     moved once in {:.1}s, and after a keystroke it held {held} for {:.1}s",
                    elapsed.as_secs_f64(),
                    unattended_move.as_secs_f64(),
                    quiet.as_secs_f64(),
                ),
            );
            Ok(())
        }
        "screen_fatal_halt" => {
            // The steady-state fatal path: userland is up, the display is
            // idle, and SYS_DEBUG action 3 runs halt_all_cpus for real.
            //
            // The path this covers used to paint a *single line*: nothing had
            // panicked during boot, so the idle loop had drained the ring into
            // the console long before, and `capture` found only what was
            // logged since the last drain. It is the case that proves the ring
            // retains what serial has already collected.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_features: ACTUATOR_KERNEL,
                    ..Default::default()
                },
            );
            if !qemu.command_until(
                "run test_rs_test_panic_child 3",
                FATAL_HALT_NONCE,
                Duration::from_secs(15),
            ) {
                return Err(format!("{FATAL_HALT_NONCE:?} never reached the console"));
            }
            // Polled, not sampled once: the report is longer than a screen
            // here, so the nonce is on one page of a cycling set.
            let dump = qemu.screendump_until(FATAL_HALT_NONCE, Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            if !text.contains(FATAL_HALT_NONCE) {
                return Err(format!(
                    "{FATAL_HALT_NONCE:?} reached serial but not the screen\ndecoded screen:\n{text}"
                ));
            }
            // The teeth for ring *retention*, and the only ones in the suite:
            // this is the one screen test whose panic comes after the
            // scheduler exists, so it is the only one where the idle loop has
            // already drained the log to serial. Reading the drained cursor
            // instead of the retained window painted exactly one row here —
            // the nonce, and no context at all — which every assertion above
            // passes happily, because the nonce *was* that row.
            //
            // Counted rather than matched on a particular line: which line
            // lands on the page carrying the nonce depends on how much
            // userland printed, and the measured states are 1 row and 96, so
            // any bound between them is a five-fold margin rather than a
            // threshold anyone has to tune.
            const MIN_CONTEXT_ROWS: usize = 20;
            let filled = dump.rows().iter().filter(|r| !r.is_empty()).count();
            if filled < MIN_CONTEXT_ROWS {
                return Err(format!(
                    "the fatal report is {filled} rows: the ring kept only what serial had not \
                     taken\ndecoded screen:\n{text}"
                ));
            }
            if dump.fill() != FILL_FATAL {
                return Err(format!("fatal fill is {:?}, want {FILL_FATAL:?}", dump.fill()));
            }
            Ok(())
        }
        "screen_fatal_halt_composited" => {
            // **Can a fatal panic reach the panel once a compositor owns the
            // scanout?** Three investigations into the T14 have rested on the
            // answer being yes and nothing has ever asked it. `screen_fatal_halt`
            // boots a config with no compositor, so the screen it paints is one
            // nothing else had claimed; `screen_blocked_dump` does have a
            // compositor, but Ctrl+Alt+D paints through `paint_report`, and
            // `halt_all_cpus` paints through `render` with a different fill and
            // a different source. The owner pulled his stick, waited a minute,
            // and saw the desktop unchanged — which is what this test is for:
            // if the fatal path cannot paint over a claimed framebuffer, every
            // "nothing appeared on the panel" observation to date says nothing
            // about what the kernel did.
            // Driven by `metal-panic-probe`, which is the same kernel the owner
            // flashes: a gate that staged this with SYS_DEBUG would certify a
            // path his image does not contain.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                smp: 8,
                qmp: true,
                // The T14's literal shape, and load-bearing rather than
                // decoration: `halt_all_cpus` waits for the log sink only when
                // there is no console, because a machine with serial already
                // has the report off the box and the wait would delay the paint
                // to buy a duplicate. Muted is therefore the only configuration
                // in which this gate's second half — the report reaching
                // `/log` — tests anything at all. The probe is time-based, so
                // it needs no console to drive it.
                mute: true,
                kernel_params: &["metal-panic-probe"],
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            // Built here rather than by the boot, because `/log` is read off
            // the partition afterwards and the image gets a fresh GUID every
            // time it is built.
            let image_path = common::lane::dir().join("fatal-composited.img");
            let image = qemu::build_boot_image(&config, &[], &[], &["metal-panic-probe"]);
            std::fs::write(&image_path, &image)
                .map_err(|e| format!("write the boot image: {e}"))?;
            let (log_start, log_len) = common::volumes::log_extent(&image, &image_path)?;
            let mut qemu = QemuInstance::boot_with_options(
                &config,
                &[],
                &[],
                BootOptions { boot_image: Some(image_path.clone()), ..options },
            );

            // The compositor has the screen *before* anything panics. Asserted
            // on the fill, exactly as `screen_blocked_dump` does: every kernel
            // paint fills with `FILL_BOOT`, so anything else is userland
            // holding the panel. Without this the test would prove that a
            // fatal panic paints a screen nobody had taken, which is what the
            // suite already knew.
            let up = qemu.screendump_while(Duration::from_secs(30), Duration::from_millis(200), |d| {
                d.fill() != FILL_BOOT
            });
            if up.fill() == FILL_BOOT {
                return Err(
                    "the compositor never took the screen, so this would have retested \
                     screen_fatal_halt on a different config"
                        .to_string(),
                );
            }

            // The probe fires 5 s after the claim; the poll is for that plus
            // the pager cycling pages.
            const MARKER: &str = "metal-panic-probe";
            // **Watched for on the way past, not looked for afterwards.**
            // `halt_all_cpus` paints `Page::Last` and only then does
            // `page_forever` start cycling, so the expiry line — the newest
            // record there is — is on the panel from the paint until the
            // first `PAGE_HOLD` turns. Polling for it once this loop has
            // finished would be polling a pager that has moved on, and
            // waiting out a whole cycle to be sure costs every green run
            // `pages * PAGE_HOLD` to learn nothing.
            let expired = std::cell::Cell::new(false);
            let dump = qemu.screendump_while(
                Duration::from_secs(40),
                Duration::from_millis(100),
                |d| {
                    let text = d.text();
                    if text.contains(LOG_DRAIN_EXPIRED) {
                        expired.set(true);
                    }
                    text.contains(MARKER)
                },
            );
            let text = dump.text();
            if text.contains(LOG_DRAIN_EXPIRED) {
                expired.set(true);
            }
            print_screen(name, &text);
            if !text.contains(MARKER) {
                return Err(format!(
                    "a fatal panic never reached the panel a compositor was holding — on a \
                     machine with no serial port that is a kernel that cannot report its own \
                     death\ndecoded screen:\n{text}"
                ));
            }
            if !text.contains("PANIC:") {
                return Err(format!(
                    "the marker is on the panel without the panic banner, so this painted \
                     something other than a fatal report\ndecoded screen:\n{text}"
                ));
            }

            // **And the report reached the stick, not only the panel.** Read
            // off the boot image's own `/log` partition, so this is the
            // device's view and not the guest's — the guest is halted and has
            // no view left. Before `halt_all_cpus` waited for the sink, the
            // file ended at the last flush *before* the panic and the report
            // existed solely as a photograph; that is the state this asserts
            // against, and it is what made three investigations argue from
            // JPEGs.
            //
            // **Asserted as the disjunction the kernel actually promises.**
            // `apic::LOG_FILE_DRAIN` is a `Budget`, so its expiry is a
            // *degraded answer* and not a broken one: the kernel gives
            // `/bin/logd` half a second and, when that is spent, says
            // `LOG_DRAIN_EXPIRED` where the reader of a muted machine is. So
            // there are three outcomes and only the third is a defect — the
            // report is on the stick; it is not, and the panel says why; or it
            // is neither written nor declared, which is a machine that lost its
            // own last words in silence. Asserting the first alone made a spent
            // budget a red the kernel never promised to avoid, and it fired 1
            // in 30 on a dev host with no other guest on it (2026-08-22).
            drop(qemu);
            let (name, on_device) =
                common::volumes::newest_log(&image_path, log_start, log_len)?;
            let on_device = String::from_utf8_lossy(&on_device).into_owned();
            if !on_device.contains(MARKER) {
                return Err(format!(
                    "/log/{name} stops at {} bytes and never carries {MARKER:?} — this boot wrote \
                     no log at all, so the drain's own verdict is not what is wrong here",
                    on_device.len()
                ));
            }
            let on_the_stick = on_device.contains("PANIC:");
            if !on_the_stick && !expired.get() {
                // The third outcome, and it carries its evidence: what a red
                // here needs is where `/bin/logd` stopped and whether the
                // volume it stopped on is intact, and a muted guest has no
                // console to have said either on.
                let volume = std::fs::read(&image_path)
                    .map_err(|e| format!("read the image back: {e}"))?;
                let complaints = toyos_fat32_check::check(&volume[log_start..log_start + log_len]);
                let verdict = if complaints.is_empty() {
                    "the checker is silent on the volume".to_string()
                } else {
                    format!(
                        "the checker has something to say:\n{}",
                        toyos_fat32_check::describe(&complaints)
                    )
                };
                return Err(format!(
                    "/log/{name} carries the marker without the panic banner and the panel never \
                     said {LOG_DRAIN_EXPIRED:?} — the report was neither written nor declared \
                     lost.\nthe file is {} bytes, ending {:?}\n{verdict}\ndecoded screen:\n{text}",
                    on_device.len(),
                    on_device.lines().rev().take(3).collect::<Vec<_>>().join(" | ")
                ));
            }
            let _ = std::fs::remove_file(&image_path);
            // **Both green outcomes name themselves, because the interesting
            // number about this gate is which one it took.** A budget that is
            // spent is not a defect and is also not nothing: `durable` is the
            // word logd publishes *after* its `fsync` returns, so a spent
            // budget with the banner on the stick means logd had written past
            // the banner and had not yet said so.
            match (on_the_stick, expired.get()) {
                (true, false) => eprintln!(
                    "  [panic] the fatal report is on the panel and in /log/{name} ({} bytes)",
                    on_device.len()
                ),
                (true, true) => eprintln!(
                    "  [panic] BUDGET SPENT, and the banner reached /log/{name} anyway ({} bytes): \
                     logd wrote past it without publishing `durable` in time",
                    on_device.len()
                ),
                (false, _) => eprintln!(
                    "  [panic] BUDGET SPENT: the report is on the panel only; /log/{name} is {} \
                     bytes and the panel carries {LOG_DRAIN_EXPIRED:?}",
                    on_device.len()
                ),
            }
            if dump.fill() != FILL_FATAL {
                return Err(format!(
                    "the panel still carries {:?} rather than the fatal fill, so the compositor's \
                     screen was never taken back",
                    dump.fill()
                ));
            }
            Ok(())
        }
        "screen_blocked_dump" => {
            // Ctrl+Alt+D on the machine it exists for: metal-sim with the
            // 16550 taken away, a compositor holding the screen, and therefore
            // no channel out of the guest at all except the panel. The report
            // has to take the screen back — declining because userland owns it,
            // which is what a boot checkpoint does, would answer the owner's
            // question into a log file nothing is left running to flush.
            //
            // The verdict is asserted on the *panel* and nowhere else, and it
            // is the summary rather than any one thread: the summary is what
            // tells the three states apart, and a photograph that has it has
            // the answer.
            //
            // Twice, and the second time is the half that matters. A photograph
            // is taken seconds after the key, by a person, of a machine whose
            // userland may still be composing — so the assertion is not that
            // the paint happened but that the panel still carries it once the
            // desktop has had its turn.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopaudiocase");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                smp: 8,
                qmp: true,
                mute: true,
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            // The compositor's wallpaper first. Every kernel paint fills the
            // panel with `FILL_BOOT`, so a fill that is anything else is
            // userland holding the screen and nothing else — which is the
            // precondition, and asserting on the fill rather than on the
            // absence of boot text is what stops a merely-blank screen passing
            // for a desktop.
            let up = qemu.screendump_while(Duration::from_secs(30), Duration::from_millis(200), |d| {
                d.fill() != FILL_BOOT
            });
            if up.fill() == FILL_BOOT {
                return Err(
                    "the compositor never took the screen, so this would have tested a \
                     checkpoint rather than a report that seizes the panel"
                        .to_string(),
                );
            }

            // Retried, like every other typed handshake in this file: a
            // keystroke that lands while a desktop is still settling reaches a
            // machine that repaints over the answer, and the retry is cheaper
            // than a rule about when a desktop is finished.
            //
            // Polled on the whole verdict rather than on one string of it. A
            // screendump is not a shutter: QEMU converts the panel while the
            // guest is still drawing on it, so a capture taken across a paint
            // carries the rows already drawn and nobody's rows for the rest.
            // A predicate satisfied by `== VERDICT:` alone accepts one of those
            // and then asserts on the missing half — which is what this test
            // did, and what made it intermittent on a quiet host.
            //
            // **A count of keystrokes rather than a span of host seconds.** It
            // was `budget(40 s)` outside a `budget(4 s)` poll, which is ten
            // tries at every width and reads as forty seconds — the number the
            // reader of a red then goes looking for. Ten is the number.
            const DUMP_TRIES: usize = 10;
            let mut dump = up;
            for _ in 0..DUMP_TRIES {
                if report_is_photographable(&dump, "").is_ok() {
                    break;
                }
                {
                    let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                    input.keys(&[
                        ("ctrl", true),
                        ("alt", true),
                        ("d", true),
                        ("d", false),
                        ("alt", false),
                        ("ctrl", false),
                    ]);
                }
                dump = qemu.screendump_while(
                    Duration::from_secs(4),
                    Duration::from_millis(100),
                    |d| report_is_photographable(d, "").is_ok(),
                );
            }
            let text = dump.text();
            print_screen(name, &text);
            report_is_photographable(&dump, "the report the keystroke painted")?;

            // **A single paint is not a report.** Whoever owns the screen goes
            // on composing and has no idea the kernel drew, so the panel the
            // owner photographs is the one that survived the next client frame
            // — not the one the dump painted. Typing is the actuator: the shell
            // echoes and the terminal repaints its whole window, which is
            // exactly what was measured blanking every row of the report that
            // lay under it, inside 100 ms, leaving the four rows below the
            // window and a 40-pixel strip beside it.
            {
                let mut input = qemu::QmpInput::open(qemu.qmp_socket());
                input.type_text("echo zqjxk\n");
            }
            // Userland's turn, and the wait is the assertion's premise rather
            // than padding: measured with the hold compiled out, an idle
            // desktop changes this panel inside 1.5 s on its own, and without
            // this the check below answered on its first capture — before the
            // compositor had composed once — and passed vacuously.
            //
            // **Deliberately not `qemu::budget`.** That pays a *liveness
            // ceiling* out per guest, and this is not one: it is a settle
            // inside a window the guest itself is timing, so scaling it by the
            // phase width walks out of the window it has to stay inside.
            // Twelve wide it became 36 s against a 15 s hold, and the check
            // then measured a machine that had already given the screen back.
            // The window is guest time and a loaded guest's is *longer* in host
            // seconds, so a fixed wait errs inwards from both sides.
            std::thread::sleep(Duration::from_secs(2));
            let back = qemu.screendump_while(
                Duration::from_secs(5),
                Duration::from_millis(100),
                |d| report_is_photographable(d, "").is_ok(),
            );
            print_screen(&format!("{name} after a client repaint"), &back.text());
            report_is_photographable(&back, "the report after a client repainted over it")?;

            let row = back.row_index("== VERDICT:").expect("checked above");
            eprintln!(
                "  [dump] on the panel of a guest with no console, and still there after the \
                 desktop repainted: {}",
                back.rows()[row].trim()
            );
            Ok(())
        }
        "screen_recoverable_untouched" => {
            // The negative of screen_fatal_halt, and the property that makes
            // the capture/render split worth having: a panic the kernel
            // recovers from must not clobber a live display. Action 0 panics
            // in syscall context, which the handler recovers from, so it
            // never reaches halt_all_cpus and must leave every pixel alone.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    // Action 0 is a `SYS_DEBUG` arm, and a kernel that ships
                    // has none: the child would be answered `InvalidArgument`
                    // and exit 0, which is this test's own red for a reason
                    // that is not about the screen at all.
                    kernel_features: ACTUATOR_KERNEL,
                    ..Default::default()
                },
            );
            let before = qemu.screendump();
            let result = qemu.run_test("test_rs_test_panic_child", Duration::from_secs(15));
            // The premise, not a formality: a timeout returns exit_code None,
            // which the old `!= Some(0)` check accepted — so a panic that
            // never fired left two identical screendumps and a green test.
            if let Some(err) = &result.error {
                return Err(format!("the recoverable panic never completed: {err}"));
            }
            if result.exit_code == Some(0) {
                return Err("recoverable panic did not kill the child".to_string());
            }
            if !result.serial.contains("SYS_DEBUG: kernel panic triggered by userspace") {
                return Err(format!(
                    "no kernel panic in the child's output\nserial:\n{}",
                    result.serial
                ));
            }
            let after = qemu.screendump();
            if !before.identical_to(&after) {
                return Err("recovering panic changed the screen".to_string());
            }
            // A screen that was blank to begin with would pass the diff for
            // the wrong reason.
            let text = before.text();
            print_screen(name, &text);
            if !text.contains("Boot: complete") {
                return Err(format!("nothing on screen to preserve\ndecoded screen:\n{text}"));
            }
            Ok(())
        }
        other => Err(format!("unknown screen test {other}")),
    }
}

/// Run a test that owns its QEMU, turning a panic into a failed test.
///
/// Every way the harness reports a dead or unreachable guest is a panic —
/// `wait_for_ready`'s boot timeout, `assert_alive`'s exit status, `Qmp`'s
/// connect and read asserts. Uncaught, one of those unwinds out of `main` and
/// the suite exits 101 with no failure list, no remaining tests and no screen:
/// the worst report for the failure class these tests exist to catch.
fn catching(f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|e| {
        Err(e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the boot panicked".to_string()))
    })
}

/// The boot a run of adjacent machine tests shares.
struct Boot {
    group: &'static str,
    qemu: QemuInstance,
    /// What the group has collected off the console since the ready marker.
    ///
    /// **A console is a stream and `drain_serial` consumes it.** The first
    /// member to wait for a line the compositor prints once takes that line
    /// away from every later member — which cost `metal_sim_window_caps` the
    /// `compositor: at most` line the first time these four shared a boot. So
    /// the group holds the console and the members that read boot-time lines
    /// read *this*, which carries the same text each of them got when it owned
    /// the boot. It is not everything the guest ever said: a member wanting a
    /// window that starts empty still drains for itself.
    console: String,
}

/// The boot a run of adjacent machine tests shares, if one is up.
type Grouped = Option<Boot>;

const METAL_SIM_DESKTOP: &str = "metal-sim desktop";
const I8042_TRACE: &str = "i8042 trace";

/// The line `tests/toyos-rust-tests/src/bin/i8042_keyboard.rs` prints once it
/// holds the keyboard claim, and the line every injection into that binary is
/// timed off. Eight callers wait for it, and one — `i8042_undecoded_bytes` —
/// also reads its capture *from* it: it is the boundary between what the
/// machine did on its own and what this test staged.
const I8042_READY: &str = "===I8042_READY===";

impl Boot {
    /// Drain for `dur` into the group's console, and hand back the whole of it.
    fn drain(&mut self, dur: Duration) -> &str {
        let more = self.qemu.drain_serial(dur);
        self.console.push_str(&more);
        &self.console
    }
}

/// The shared boot this machine test runs on, or `None` if it owns its own.
///
/// **Two conditions decide membership and neither is cost.** No member may kill
/// the guest, because the rest of the group is queued behind it; and no member
/// may leave state a later one reads. `readdir_bound` is the standing
/// counter-example — it fills `/tmp` to the VFS listing limit and would refuse
/// every later `read_dir` in that guest — and it is why the answer has to be
/// obviously no rather than probably. Where a member does write something the
/// compositor holds, the group's order is the argument: the observer runs
/// against an untouched desktop and the window cap runs before anything else
/// has taken a window.
///
/// Adjacency in [`MACHINE_TESTS`] is what makes a group one boot rather than
/// two: a non-member between two members takes the guest down, because only one
/// may exist at a time (see [`run_machine_test`]).
fn group_of(name: &str) -> Option<&'static str> {
    match name {
        "metal_sim_compositor"
        | "metal_sim_scanout_wc"
        | "metal_sim_window_caps"
        | "metal_sim_ipc_hostile_peer"
        | "metal_sim_compositor_stall"
        | "metal_sim_client_death" => Some(METAL_SIM_DESKTOP),
        "i8042_keyboard" | "i8042_no_spurious_wake" | "i8042_mouse" => Some(I8042_TRACE),
        _ => None,
    }
}

/// The machine every member of `group` runs on, booted by the first member to
/// ask for it.
fn group_boot<'a>(
    held: &'a mut Grouped,
    group: &'static str,
    boot: impl FnOnce() -> QemuInstance,
) -> &'a mut Boot {
    if held.is_none() {
        let qemu = boot();
        let console = qemu.boot_log().to_string();
        *held = Some(Boot { group, qemu, console });
    }
    let up = held.as_mut().expect("just booted");
    assert_eq!(up.group, group, "run_machine_test releases a boot before another group asks");
    up
}

/// `tests/metalcase` on [`qemu::Profile::Metal`]: the T14's device shape with a
/// compositor on the firmware framebuffer, carrying the client binaries its
/// members run.
///
/// Those and not the whole rust set — metalcase's initrd is four programs and
/// the rest would add tens of megabytes to a boot that needs these.
fn boot_metal_sim_desktop(rust_bins: &[(String, Vec<u8>)]) -> QemuInstance {
    const CLIENTS: [&str; 4] =
        ["window_caps", "ipc_hostile_peer", "compositor_stall", "compositor_client_death"];
    let missing: Vec<&str> = CLIENTS
        .iter()
        .copied()
        .filter(|want| !rust_bins.iter().any(|(name, _)| name == want))
        .collect();
    assert!(missing.is_empty(), "the metal-sim clients were not built: {missing:?}");
    let bins: Vec<(String, Vec<u8>)> = rust_bins
        .iter()
        .filter(|(name, _)| CLIENTS.contains(&name.as_str()))
        .cloned()
        .collect();

    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options)).unwrap_or_else(|e| panic!("{e}"));
    QemuInstance::boot_with_options(&config, &[], &bins, options)
}

/// Metal-sim with the i8042 driver's per-drain trace on and QMP open, which is
/// how a test injects a key or a pointer packet and then reads what the driver
/// made of it.
fn boot_i8042_trace(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> QemuInstance {
    // On metal-sim, because that is the machine the driver is for and the
    // absent USB HID is what makes these tests measure anything: QEMU routes
    // injected input to one handler per device class, and with a usb-kbd
    // present that handler is not the PS/2 one.
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        qmp: true,
        kernel_params: &["i8042-trace"],
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options)).unwrap_or_else(|e| panic!("{e}"));
    QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options)
}

/// The machine the layout and wizard tests run on.
///
/// `Profile::Metal` for the same reason `boot_i8042_trace` uses it: QEMU
/// activates one input handler per device class, so with a USB HID present the
/// injected keys would not reach the i8042 — and these tests are about which
/// HID usage a physical key position reports. `tests/testcases` boots neither
/// the compositor nor `/bin/console`, so the keyboard claim is free for
/// `locale_gate` to take — which it does, because it is standing in for a
/// surface and a surface holds the keyboard.
fn boot_locale(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> QemuInstance {
    let options =
        BootOptions { profile: qemu::Profile::Metal, qmp: true, ..Default::default() };
    metal_sim_argv_check(&qemu::profile_argv(&options)).unwrap_or_else(|e| panic!("{e}"));
    QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options)
}

/// Every negative claim `Profile::Metal` makes, read off the argv QEMU is
/// launched with. A claim about which devices do *not* exist is a claim about
/// this list and nothing else — no console line and no screendump can see a
/// device that is present but unused.
fn metal_sim_argv_check(argv: &[String]) -> Result<(), String> {
    if let Some(bad) = argv.iter().find(|a| a.contains("virtio")) {
        return Err(format!("metal-sim passed a virtio device to QEMU: {bad}"));
    }
    // The mechanism, not two names. `xhci::device::scan_ports` binds any
    // boot-protocol HID — keyboard, mouse or tablet — so an enumeration of the
    // two device names that happen to be in the tree today would let a
    // `usb-mouse` added for debugging break the profile's only negative claim
    // while the assertion stayed green. The boot stick is the one USB device
    // this machine has.
    let hid = argv
        .windows(2)
        .filter(|w| w[0] == "-device")
        .map(|w| w[1].as_str())
        .find(|v| v.starts_with("usb-") && !v.starts_with("usb-storage"));
    if let Some(bad) = hid {
        return Err(format!("metal-sim passed a USB device that is not the boot stick: {bad}"));
    }
    // Without this QEMU adds an e1000e with a slirp backend, an ide-cd and an
    // isa-parallel that nothing declared — and the NIC is enough to make netd
    // claim a device on the machine whose whole point is that it has none.
    // None of them appears in argv, so this flag is the only observable form
    // of their absence here; `query-pci` is the direct one.
    if !argv.iter().any(|a| a == "-nodefaults") {
        return Err("metal-sim did not pass -nodefaults; QEMU's default-device pass is back".to_string());
    }
    Ok(())
}

/// One `key=value` out of a `compositor: frames=…` line.
///
/// Every compositor gate reads this line, so they read it the same way: a key
/// that is not there is a compositor whose instrument changed shape, which is
/// a different failure from a number that is too large and says so.
fn compositor_field(stats: &str, key: &str) -> Result<u64, String> {
    let raw = stats
        .split_whitespace()
        .find_map(|f| f.strip_prefix(key))
        .ok_or_else(|| format!("no {key} in the compositor's stats line: {stats}"))?;
    raw.parse::<u64>().map_err(|_| format!("{key}{raw} is not a number: {stats}"))
}

/// How many pixels the compositor said it was given, off its own startup line.
///
/// Read rather than assumed: every damage gate is a fraction of the screen, and
/// a fraction of a number the harness hardcoded would keep agreeing with itself
/// on a machine whose panel is a different size.
fn compositor_screen_px(console: &str) -> Result<u64, String> {
    let (w, h) = compositor_screen_size(console)?;
    Ok(w as u64 * h as u64)
}

/// Which processes survive the T14's device shape, in their own words.
///
/// The compositor claims a firmware framebuffer and says what it got; netd finds
/// no NIC and exits rather than panic; soundd finds no audio device and stays up
/// on a null sink rather than exiting (hardware absence is a routing state — a
/// no-device machine still serves audio clients, discarding what they play); and
/// sshd, which has no device of its own, finds no netd to bind through and says
/// so instead of dumping a tokio backtrace across the boot. The earlier version
/// read the bottom pixel row instead, which says nothing about any of them and
/// stayed green with their graceful behavior reverted.
///
/// **All four are init's children and nothing supervises them**, so the message
/// is the entire diagnostic and its absence is the whole defect — which is why
/// each is asserted by its own text rather than by anything surviving.
///
/// First in its group, and that is the assertion talking: `cursor == frames`
/// and the stats line are read off a desktop no client has connected to yet.
fn metal_sim_compositor(boot: &mut Boot) -> Result<(), String> {
    // init spawns all four programs without waiting, so test-runner's
    // ready marker races the daemons' own lines. Keep draining until
    // every line has been said or the window closes.
    const WANT: [&str; 4] = [
        "compositor: ready",
        "soundd: no audio device, presenting a null sink",
        "netd: no NIC on this machine, exiting",
        "sshd: no network on this machine, exiting",
    ];
    let stalled = await_guest(&mut boot.qemu, &mut boot.console, "every daemon's own line", |c| {
        WANT.iter().all(|w| c.contains(w))
    })
    .err();
    for want in WANT {
        if !boot.console.contains(want) {
            return Err(format!(
                "{}{want:?} never reached the console:\n{}",
                stalled.map(|why| format!("{why}\n")).unwrap_or_default(),
                boot.console
            ));
        }
    }
    // The compositor's periodic self-measurement, which is how the
    // T14 reports what compositing cost it once it is off the serial
    // port and the log is only a file on the stick. It is emitted from
    // a composited frame, so its absence is a compositor that stopped
    // drawing as much as an instrument that never ran.
    //
    // Three of them, not one: the first covers the boot, which repaints the
    // whole screen, and what the idle gate below is about is every interval
    // after that.
    let intervals = |c: &str| {
        c.lines().filter(|l| l.contains("compositor: frames=") && l.contains("windows=")).count()
    };
    let stalled = await_guest(&mut boot.qemu, &mut boot.console, "three frame batches", |c| {
        intervals(c) >= 3
    })
    .err();
    if let Some(why) = stalled {
        return Err(format!(
            "{why}\nthe compositor reported {} of the three frame batches this reads:\n{}",
            intervals(&boot.console),
            boot.console
        ));
    }
    // One more drain so the tail of that line cannot still be in
    // flight when it is parsed.
    boot.drain(Duration::from_millis(250));
    let console = &boot.console;
    // The compositor reports the mode it was handed, which is the
    // proof it claimed a real firmware framebuffer rather than
    // starting on nothing.
    let Some(mode) = console
        .lines()
        .find_map(|l| l.split("compositor: wallpaper ").nth(1))
    else {
        return Err(format!(
            "the compositor never said what framebuffer it got:\n{console}"
        ));
    };
    let Some(stats) = console.lines().find(|l| l.contains("compositor: frames=")) else {
        return Err(format!(
            "the compositor never reported a composited frame:\n{console}"
        ));
    };
    let frames = compositor_field(stats, "frames=")?;
    let min_us = compositor_field(stats, "composite_us_min=")?;
    let max_us = compositor_field(stats, "composite_us_max=")?;
    let total_us = compositor_field(stats, "composite_us_total=")?;
    let cursor = compositor_field(stats, "cursor=")?;
    // Read for their presence and their shape; what they measure is the cost
    // of moving bytes to a panel, which QEMU's host-RAM framebuffer cannot
    // show. There is deliberately no scanout *read* figure: the compositor
    // holds the mapping as a `window::Screen`, which returns no pixel and
    // hands out no pointer, so a counter for it could only ever be zero.
    compositor_field(stats, "scanout_wr_bytes=")?;
    compositor_field(stats, "scanout_blits=")?;
    compositor_field(stats, "back_rd_bytes=")?;
    compositor_field(stats, "rects=")?;
    compositor_field(stats, "damage_px=")?;
    compositor_field(stats, "windows=")?;
    if frames == 0 || total_us == 0 {
        return Err(format!("the compositor reported a dead instrument: {stats}"));
    }
    if min_us > max_us || max_us > total_us {
        return Err(format!("min/max/total do not order: {stats}"));
    }
    // GOP hands out no hardware cursor (`flags: 0`), so the compositor draws
    // one itself — into the back buffer, and only into frames whose damage
    // reaches it. The first frame repaints the whole screen, so it does. That
    // it is *not* every frame is the point: a cursor nobody moved does not
    // need repainting, and drawing it per frame is what a compositor that
    // composed straight onto the panel had to do.
    if cursor == 0 || cursor > frames {
        return Err(format!(
            "{frames} frames on a shape with no hardware cursor drew {cursor} cursors: \
             {stats}"
        ));
    }

    // What one second of an idle desktop costs. Nothing is on this screen but
    // the wallpaper and the taskbar, and the only thing that changes is the
    // clock — so the largest frame in a settled interval is the readout's own
    // box and nothing else.
    //
    // One percent of the screen is the line because the two shapes it
    // separates are far apart: the readout box is 0.46% of a 1920x1080 panel,
    // the whole taskbar strip is 2.96%, and a full repaint is 100%. The
    // taskbar redrawing whole once a second is what the owner saw flicker.
    let screen_px = compositor_screen_px(console)?;
    let settled: Vec<&str> = console
        .lines()
        .filter(|l| l.contains("compositor: frames="))
        .skip(1)
        .collect();
    let Some(idle) = settled.last() else {
        return Err(format!(
            "the compositor reported one interval and no more, so nothing here saw a settled \
             desktop:\n{console}"
        ));
    };
    let windows = compositor_field(idle, "windows=")?;
    if windows != 0 {
        return Err(format!(
            "this desktop was supposed to have no windows on it, and has {windows}: {idle}"
        ));
    }
    let biggest = compositor_field(idle, "damage_px_max=")?;
    if biggest * 100 > screen_px {
        return Err(format!(
            "an idle desktop's largest frame repainted {biggest} of {screen_px} pixels — over a \
             percent of the screen for a clock tick: {idle}"
        ));
    }
    // And nothing panicked on the way. A daemon mishandling its
    // absent device fails the positive check above; this catches the
    // rest of the boot dying instead.
    serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
    eprintln!("  [metal-sim] compositor up on {}", mode.trim());
    eprintln!("  [metal-sim] {}", stats.trim());
    eprintln!(
        "  [metal-sim] idle: {biggest} px is the biggest frame of {screen_px} on screen — {}",
        idle.trim()
    );
    eprintln!("  [metal-sim] soundd on a null sink, netd exited — both handled their absent device");
    Ok(())
}

/// The scanout's memory type, from the MSR to the mapping the compositor
/// writes through.
///
/// **The speed this exists for is not measurable here and no line below tries
/// to be.** QEMU's framebuffer is host RAM, where a store costs the same under
/// every memory type; what a guest can be held to is the *decision*, and it has
/// three parts that fail independently. `IA32_PAT` must hold WC in the entry
/// the page tables select, which is per-CPU MSR state no page table records.
/// The kernel must combine that entry with the MTRR it read and reach WC — SDM
/// Vol. 3A Table 11-7 gives WC for a WC PAT entry under every MTRR type, so a
/// UC range register has no veto and a boot reporting UC here is one where the
/// entry never landed. And the process holding the scanout must have been given
/// the same type the kernel gave itself, which is the part that decides what a
/// frame costs: the compositor writes through its own page tables.
fn metal_sim_scanout_wc(boot: &mut Boot) -> Result<(), String> {
    const PAT: &str = "PAT: IA32_PAT=";
    const SCANOUT: &str = "GOP: scanout memory type ";
    const MAPPED: &str = "mapped WriteCombining into pid ";

    let _ = await_guest(&mut boot.qemu, &mut boot.console, "the three memory-type lines", |c| {
        [PAT, SCANOUT, MAPPED].iter().all(|w| c.contains(w))
    });
    let console = &boot.console;

    let Some(pat) = console.lines().find(|l| l.contains(PAT)) else {
        return Err(format!("no boot programmed IA32_PAT:\n{console}"));
    };
    let Some(entry) = pat.split(" = ").nth(1) else {
        return Err(format!("{pat:?} names no type for the entry it wrote"));
    };
    if entry.trim() != "WC" {
        return Err(format!(
            "the entry the scanout's pages select reads back {entry:?}, not WC: {pat}"
        ));
    }

    let Some(scanout) = console.lines().find_map(|l| l.split(SCANOUT).nth(1)) else {
        return Err(format!("GOP never reported the scanout's memory type:\n{console}"));
    };
    // Firmware's, and deliberately not asserted: under test is that whatever
    // the range registers say combines to WC, never what OVMF chose.
    let Some(mtrr) = scanout.split("(MTRR ").nth(1).and_then(|s| s.split(',').next()) else {
        return Err(format!("{scanout:?} does not say what the MTRR held"));
    };
    let effective = scanout.split(' ').next().unwrap_or("");
    if effective != "WC" {
        return Err(format!(
            "the scanout came out {effective} over an MTRR that says {mtrr}: {scanout}"
        ));
    }

    let Some(handed) = console.lines().find(|l| l.contains(MAPPED)) else {
        return Err(format!(
            "no process was handed a write-combining mapping, so whatever the kernel gave \
             itself, the compositor is still writing through the default:\n{console}"
        ));
    };

    eprintln!("  [metal-sim] {}", pat.trim());
    eprintln!("  [metal-sim] scanout {effective} over an MTRR that says {mtrr}");
    eprintln!("  [metal-sim] {}", handed.trim());
    Ok(())
}

/// Pixels one relative pointer count is worth, on a screen of this size.
///
/// `kernel/src/mouse.rs` scales a count into the square 0..32767 space by
/// `REL_SCALE * short / axis`, and the compositor maps that space back by the
/// axis — so the axis cancels and a count is `REL_SCALE * short / 32768` px on
/// both, which is the whole reason the scaling is per-axis. Duplicated here
/// because a test cannot link the kernel, and *checked* rather than trusted:
/// the calibration press in [`metal_sim_window_drag`] is where the cursor
/// actually is, and it fails by name if this arithmetic put it somewhere else.
fn px_per_count(screen_w: u32, screen_h: u32) -> f64 {
    const REL_SCALE: f64 = 64.0;
    REL_SCALE * screen_w.min(screen_h) as f64 / 32768.0
}

/// A window dragged across the desktop by its title bar, and what that cost.
///
/// The owner's report was that moving a window redraws everything. Two things
/// made it true and both are visible from here: the press that starts a drag
/// marked the whole screen dirty, and every damaged pixel was written to the
/// panel more than once because the desktop was composed *onto* the panel. The
/// gate is the compositor's own `damage_px_max`, which is the largest single
/// frame of an interval — the frame the press produced, if the press is still
/// repainting the screen.
///
/// Nothing here aims at the title bar from constants. The client reports the
/// content-local name of every pixel the host presses, so the window's origin
/// is measured, and the same press repeated after the drag is what proves the
/// window moved rather than that the injection was ignored.
fn metal_sim_window_drag(rust_bins: &[(String, Vec<u8>)]) -> Result<(), String> {
    let bins: Vec<(String, Vec<u8>)> =
        rust_bins.iter().filter(|(name, _)| name == "window_drag").cloned().collect();
    if bins.is_empty() {
        return Err("the window_drag client was not built".to_string());
    }

    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
    let options =
        BootOptions { profile: qemu::Profile::Metal, qmp: true, ..Default::default() };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &bins, options);

    // The compositor announces its screen after the ready marker, so this
    // waits for the line rather than reading a boot log that cannot have it.
    //
    // And it waits for the first *stats* line too, which is the one carrying
    // the frame that painted the desktop for the first time: a gate about what
    // a drag costs must not be handed the boot's own full-screen repaint.
    let mut boot_log = qemu.boot_log().to_string();
    await_marker(&mut qemu, &mut boot_log, "compositor: frames=", "the boot's own repaint interval")
        .map_err(|why| format!("{why}\n{boot_log}"))?;
    boot_log.push_str(&qemu.drain_serial(Duration::from_millis(250)));
    let screen_px = compositor_screen_px(&boot_log)?;
    let (screen_w, screen_h) = compositor_screen_size(&boot_log)?;
    let ppc = px_per_count(screen_w, screen_h);

    // Where the host presses, twice: the middle of the screen, which is where
    // the compositor centres a window it was given a size for.
    let probe_x = screen_w / 2;
    let probe_y = screen_h / 2;
    // How far the drag carries the window. Big enough that a rounded count is
    // not most of it, small enough that the pressed pixel is still inside the
    // content afterwards, which is what the second press reads.
    const DRAG_DX: u32 = 120;
    const DRAG_DY: u32 = 60;

    let result = qemu.run_test_hooked(
        "test_rs_window_drag",
        Duration::from_secs(120),
        "===DRAG_READY===",
        |socket| {
            let mut input = qemu::QmpInput::open(socket);
            let counts = |px: f64| (px / ppc).round() as i32;
            // A packet nobody could produce by hand teleports the cursor, and
            // the compositor damages where it was and where it went — so an
            // injection that moves it a screen at a time makes a frame this
            // gate then reads as a defect. Every step here is a plausible
            // flick of a real mouse.
            const STEP_PX: f64 = 120.0;
            let travel = |input: &mut qemu::QmpInput, dx: i32, dy: i32| {
                let step = counts(STEP_PX).max(1);
                let steps = (dx.abs().max(dy.abs()) + step - 1) / step;
                for i in 0..steps.max(1) {
                    let from_x = i * dx / steps.max(1);
                    let to_x = (i + 1) * dx / steps.max(1);
                    let from_y = i * dy / steps.max(1);
                    let to_y = (i + 1) * dy / steps.max(1);
                    input.mouse(to_x - from_x, to_y - from_y, None);
                    thread::sleep(Duration::from_millis(25));
                }
            };
            // Everything is relative to a pointer at the origin, and the
            // kernel clamps its accumulator there, so driving into the corner
            // is a way to know where it is without being told. One screen is
            // the distance: the cursor is on it, so that reaches both edges.
            let home = |input: &mut qemu::QmpInput| {
                travel(input, -counts(screen_w as f64), -counts(screen_h as f64));
            };
            let click = |input: &mut qemu::QmpInput| {
                input.mouse(0, 0, Some(("left", true)));
                thread::sleep(Duration::from_millis(60));
                input.mouse(0, 0, Some(("left", false)));
                thread::sleep(Duration::from_millis(60));
            };

            // One: name the pixel under the middle of the screen.
            home(&mut input);
            travel(&mut input, counts(probe_x as f64), counts(probe_y as f64));
            click(&mut input);

            // Two: up onto the title bar and drag. The window is centred and
            // its content is `CLIENT_H` tall, so the middle of the screen is
            // `CLIENT_H/2` below the content's top edge give or take the few
            // pixels by which the taskbar and the title bar differ — and a
            // little further up is the strip a person grabs to move a window.
            // If this lands in the content instead, the client reports a third
            // press and the assertions below say so by name.
            travel(&mut input, 0, -counts(CLIENT_H as f64 / 2.0 + TITLE_PROBE_PX));
            input.mouse(0, 0, Some(("left", true)));
            thread::sleep(Duration::from_millis(60));
            travel(&mut input, counts(DRAG_DX as f64), counts(DRAG_DY as f64));
            input.mouse(0, 0, Some(("left", false)));
            thread::sleep(Duration::from_millis(120));

            // Three: name the same screen pixel again. It is a different
            // pixel of the window now, by exactly what the drag carried.
            home(&mut input);
            travel(&mut input, counts(probe_x as f64), counts(probe_y as f64));
            click(&mut input);
        },
    );

    if result.error.is_some() || result.exit_code != Some(0) {
        // **`{:?}` on the verdict is what this arm used to say**, and a `Debug`
        // of a multi-line report is one line of `\n` escapes — the kernel's own
        // account of a death, rendered unreadable by a format specifier. It is
        // printed as itself now.
        let why = match &result.error {
            Some(err) => err.to_string(),
            None => String::from("it finished and its exit code is the finding"),
        };
        return Err(format!(
            "window_drag exited {:?}: {why}\n{}",
            result.exit_code, result.stdout
        ));
    }

    // The client ends on the host's second press, so the interval the drag is
    // in is still open when it exits. Waiting for the line that closes it keeps
    // a slower guest a longer run rather than a different verdict.
    let mut text = result.serial;
    text.push_str(
        &qemu.drain_until(Duration::from_secs(10), |l| l.contains("compositor: frames=")),
    );
    if !text.contains(&format!("drag probe: {CLIENT_W}x{CLIENT_H} window up")) {
        return Err(format!(
            "the client did not report a {CLIENT_W}x{CLIENT_H} window, so the aim below is for a \
             window that is not there:\n{text}"
        ));
    }
    let presses: Vec<(i64, i64)> = text
        .lines()
        .filter_map(|l| l.split("drag probe: press at ").nth(1))
        .filter_map(|rest| rest.trim().split_once(','))
        .filter_map(|(x, y)| Some((x.trim().parse().ok()?, y.trim().parse().ok()?)))
        .collect();
    if presses.len() != 2 {
        return Err(format!(
            "the client was pressed inside its content {} times, not twice — the injected \
             pointer never reached it:\n{text}",
            presses.len()
        ));
    }
    let (before, after) = (presses[0], presses[1]);
    // The window moved, so the screen pixel the host pressed is now nearer the
    // window's top-left corner by what the drag carried.
    let moved_x = before.0 - after.0;
    let moved_y = before.1 - after.1;
    let slack = 8;
    if (moved_x - DRAG_DX as i64).abs() > slack || (moved_y - DRAG_DY as i64).abs() > slack {
        return Err(format!(
            "the drag was supposed to carry the window {DRAG_DX},{DRAG_DY} px and carried it \
             {moved_x},{moved_y} — the press missed the title bar, or the drag was not followed:\
             \n{text}"
        ));
    }

    // A fifth of the screen. The window is 400x160 with its chrome, so a drag
    // of it damages the place it left and the place it arrived — well under a
    // tenth of a 1920x1080 panel. A press that still marks the screen dirty is
    // 100%, which is what this separates.
    let mut biggest = 0;
    let mut lines = 0;
    for line in text.lines().filter(|l| l.contains("compositor: frames=")) {
        lines += 1;
        biggest = biggest.max(compositor_field(line, "damage_px_max=")?);
    }
    if lines == 0 {
        return Err(format!("the compositor reported no interval during the drag:\n{text}"));
    }
    if biggest * 5 > screen_px {
        return Err(format!(
            "dragging a {CLIENT_W}x{CLIENT_H} window repainted {biggest} of {screen_px} pixels in \
             one frame — over a fifth of the screen:\n{text}"
        ));
    }

    eprintln!(
        "  [metal-sim] drag carried the window {moved_x},{moved_y} px; biggest frame {biggest} \
         of {screen_px} px over {lines} intervals"
    );
    Ok(())
}

/// How far above its content the host reaches for a window's title bar.
///
/// Not the compositor's title-bar height — this is a probe, and what it needs
/// is to land inside a strip whose size it does not know. Twelve pixels is
/// above any border and inside any title bar a person could grab, and either
/// kind of miss is caught by name: too little and the client reports a third
/// press, too much and the window never moves.
const TITLE_PROBE_PX: f64 = 12.0;

/// The window `window_drag` asks for, which is how the host knows where to
/// press. Asserted against the client's own report rather than assumed.
const CLIENT_W: u32 = 400;
const CLIENT_H: u32 = 160;

/// The screen the compositor said it was given, off its own startup line.
fn compositor_screen_size(console: &str) -> Result<(u32, u32), String> {
    let mode = console
        .lines()
        .find_map(|l| l.split("compositor: wallpaper ").nth(1))
        .and_then(|rest| rest.split("scaling to ").nth(1))
        .ok_or_else(|| format!("the compositor never said what screen it got:\n{console}"))?;
    let (w, h) = mode
        .trim()
        .split_once('x')
        .ok_or_else(|| format!("unreadable screen size {mode:?}"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("unreadable width in {mode:?}"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("unreadable height in {mode:?}"))?;
    Ok((w, h))
}

/// End's release: the sentinel `test_rs_i8042_keyboard` exits on
/// (`tests/toyos-rust-tests/src/bin/i8042_keyboard.rs`). Every caller that
/// injects through a fresh connection sends this after its last injection
/// instead of running out the binary's fallback deadline, except
/// `i8042_health_cadence` — whose verdict is a report cadence over a real span,
/// not a delivered key. The two callers that hold one connection open for the
/// whole run ([`i8042_keyboard`], [`i8042_no_spurious_wake`]) send the same two
/// transitions as the last group of their own script: a `-qmp …,server` socket
/// serves one monitor at a time, so a second one opened here would block.
fn send_i8042_sentinel(socket: &Path) {
    qemu::qmp_send_keys(socket, &[("end", true), ("end", false)]);
}

/// What [`i8042_keyboard`] types, as the groups it may have in flight at once,
/// each with the number of `kev` lines the guest owes for it.
///
/// **A group is the unit of pacing, and its size is bounded by
/// [`QEMU_PS2_QUEUE`].** The device holds sixteen set-1 bytes and
/// `ps2_queue()` drops the seventeenth *silently and one byte at a time*; the
/// kernel never learns of it, so its `dropped`/`lost edges`/`overruns`
/// counters all read zero on a stream with a hole in it. What a lost byte
/// costs is not one transition: a lost make leaves its break to be filtered by
/// `handle_key` — a break for a usage nothing holds queues nothing — so the
/// whole key disappears, and a lost `0xE0` leaves the break to decode as an
/// unrelated keypad code that also holds nothing, so a press survives with no
/// release. Sending the script on a wall clock and hoping the guest keeps up
/// is what put 26 bytes against those 16 and produced both of those shapes on
/// CI. The largest group below is four bytes.
const KEYBOARD_SCRIPT: &[(&[(&str, bool)], usize)] = &[
    (&[("h", true), ("h", false)], 2),
    (&[("e", true), ("e", false)], 2),
    (&[("l", true), ("l", false)], 2),
    (&[("l", true), ("l", false)], 2),
    (&[("o", true), ("o", false)], 2),
    // One command, so the chord arrives as a chord rather than as a race.
    (&[("shift", true), ("b", true), ("b", false), ("shift", false)], 4),
    (&[("left", true), ("left", false)], 2),
    (&[("esc", true), ("esc", false)], 2),
    // A modifier on its own, so a stuck one is visible.
    (&[("shift", true)], 1),
    (&[("shift", false)], 1),
    // The sentinel the guest exits on; see [`send_i8042_sentinel`].
    (&[("end", true), ("end", false)], 2),
];

/// A key injected at the controller, decoded, mapped and delivered to a
/// userland process — IRQ delivery, set-1 decode, the HID mapping and the
/// shared translate/layout path, in one run.
///
/// **Paced against the guest's own report**, for [`i8042_mouse`]'s reason and
/// [`KEYBOARD_SCRIPT`]'s: a group goes out only once every `kev` line the one
/// before it owed has come back, so at most four bytes are ever outstanding at
/// a device that holds sixteen. A guest that stalls costs this test wall clock
/// and never a verdict.
fn i8042_keyboard(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let boot = qemu.boot_log().to_string();
    if !boot.contains("i8042: kbd set2+xlat (readback 0x41)") {
        return Err(format!("the PS/2 keyboard never came up:\n{boot}"));
    }

    let sent = std::cell::Cell::new(0usize);
    let seen = std::cell::Cell::new(0usize);
    let result = {
        let mut input: Option<qemu::QmpInput> = None;
        qemu.run_test_paced(
            "test_rs_i8042_keyboard",
            Duration::from_secs(20),
            |socket, line| {
                if line.contains(I8042_READY) {
                    input = Some(qemu::QmpInput::open(
                        socket.expect("i8042_keyboard needs BootOptions { qmp }"),
                    ));
                }
                if line.contains("kev usage=") {
                    seen.set(seen.get() + 1);
                }
                let Some(input) = input.as_mut() else { return };
                // What everything already sent owes. Nothing new goes out
                // until the guest has reported all of it, which is what bounds
                // the bytes outstanding at the device to one group's worth.
                let owed: usize = KEYBOARD_SCRIPT[..sent.get()].iter().map(|(_, n)| n).sum();
                if seen.get() < owed {
                    return;
                }
                if let Some((keys, _)) = KEYBOARD_SCRIPT.get(sent.get()) {
                    input.keys(keys);
                    sent.set(sent.get() + 1);
                }
            },
        )
    };
    let (sent, seen) = (sent.get(), seen.get());
    if let Some(err) = &result.error {
        // The guard, not the verdict: under the pacing the host is *waiting*
        // for the guest when this fires, so what it establishes is that the run
        // stopped and never that the machine dropped a key.
        let owed: usize = KEYBOARD_SCRIPT.iter().map(|(_, n)| n).sum();
        return Err(format!(
            "{STALLED} {err} — {sent} of {} groups sent and {seen} of {owed} key events back \
             when the host gave up waiting for the next\n{}",
            KEYBOARD_SCRIPT.len(),
            result.stdout
        ));
    }

    let events = parse_key_events(&result.stdout);
    if events.is_empty() {
        return Err(format!("no key event reached userland:\n{}", result.stdout));
    }
    // Presses spell the injected text: IRQ delivery, set-1 decode,
    // the HID mapping, the shared translate/layout path, and arrival
    // in a userland process, in one assertion.
    let typed: String = events
        .iter()
        .filter(|e| e.modifiers & 0x10 == 0)
        .map(|e| e.translated.as_str())
        .collect();
    if !typed.contains("hello") {
        return Err(format!("typed {typed:?}, want it to contain \"hello\""));
    }
    if !typed.contains('B') {
        return Err(format!("typed {typed:?} — Shift+b did not produce a capital"));
    }
    if !typed.contains("\u{1b}[D") {
        return Err(format!("typed {typed:?} — Left arrow produced no escape sequence"));
    }
    for want in [0x29u8, 0x50, 0xE1] {
        if !events.iter().any(|e| e.usage == want) {
            return Err(format!("no event for HID usage {want:#04x} in {events:?}"));
        }
    }
    // Every press is matched by a release — **the sentinel's included**. `0x4D`
    // is End, and the guest exits on its release, so a run that never receives
    // it runs out that binary's own five-second fallback instead and every
    // assertion above still passes: a green test six seconds slower than its
    // price, which is what a lost sentinel used to look like and why it was read
    // off the `durations` gate rather than off a verdict.
    for usage in [0x0Bu8, 0x08, 0x0F, 0x12, 0x05, 0x29, 0x50, 0xE1, 0x4D] {
        let presses = events.iter().filter(|e| e.usage == usage && e.modifiers & 0x10 == 0).count();
        let releases = events.iter().filter(|e| e.usage == usage && e.modifiers & 0x10 != 0).count();
        if presses == 0 || presses != releases {
            return Err(format!(
                "usage {usage:#04x}: {presses} presses, {releases} releases"
            ));
        }
    }
    // Nothing is left held: the bare Shift came back up.
    let last = events.last().unwrap();
    if last.modifiers & !0x10 != 0 {
        return Err(format!("a modifier is stuck down: last event {last:?}"));
    }
    // And they came from the i8042, not from somewhere else.
    let drained: usize = qemu
        .boot_log()
        .lines()
        .chain(result.serial.lines())
        .filter_map(trace_keys)
        .filter(|&k| k > 0)
        .sum();
    if drained == 0 {
        return Err("no i8042 drain reported a key event".to_string());
    }
    eprintln!(
        "  [i8042] {} events to userland, {drained} from the driver; {sent} groups, none sent \
         before the one before it came back",
        events.len()
    );
    Ok(())
}

/// Swiss German end to end: the real command selects the layout, and the keys
/// a Swiss keyboard has arrive as the characters a Swiss keyboard prints.
///
/// Injection is by *position*: QEMU's qcodes name the US legend of a physical
/// key, so `y` is the key a Swiss board prints `Z` on and `bracket_left` is
/// the one it prints `ü` on. That is exactly the substitution the layout
/// exists to make, so asserting on the characters that come out is asserting
/// on the table, the modifier levels, the ISO key and the dead-key machine at
/// once.
fn swiss_german_layout(qemu: &mut QemuInstance) -> Result<(), String> {
    let result = qemu.run_test_hooked(
        "test_rs_locale_gate layout",
        Duration::from_secs(30),
        "===SWISS_READY===",
        |socket| {
            let mut input = qemu::QmpInput::open(socket);
            let mut tap = |events: &[(&str, bool)]| {
                input.keys(events);
                thread::sleep(Duration::from_millis(25));
            };
            let plain = |k: &'static str| vec![(k, true), (k, false)];
            let shifted =
                |k: &'static str| vec![("shift", true), (k, true), (k, false), ("shift", false)];
            let altgr =
                |k: &'static str| vec![("alt_r", true), (k, true), (k, false), ("alt_r", false)];

            // QWERTZ: the two letters that swap.
            tap(&plain("y"));
            tap(&plain("z"));
            // The three dedicated umlauts, and the accented vowel Shift gives.
            tap(&plain("bracket_left"));
            tap(&plain("semicolon"));
            tap(&plain("apostrophe"));
            tap(&shifted("apostrophe"));
            // The AltGr layer.
            tap(&altgr("2"));
            tap(&altgr("e"));
            tap(&altgr("bracket_left"));
            // The ISO key, all three levels the reference gives it a legend for.
            tap(&plain("less"));
            tap(&shifted("less"));
            tap(&altgr("less"));
            // Dead keys: compose, compose with Shift, the capital umlaut this
            // layout has no dedicated key for, the bare form before a space,
            // an AltGr dead key, and one that composes with nothing.
            tap(&plain("equal"));
            tap(&plain("e"));
            tap(&plain("equal"));
            tap(&shifted("e"));
            tap(&plain("bracket_right"));
            tap(&shifted("u"));
            tap(&plain("equal"));
            tap(&plain("spc"));
            tap(&altgr("minus"));
            tap(&plain("e"));
            tap(&plain("equal"));
            tap(&plain("q"));
            // And the key the wizard asks about.
            tap(&plain("grave_accent"));
            // The sentinel `test_rs_locale_gate layout` exits on — the same End
            // key and the same reason as [`send_i8042_sentinel`], sent through
            // the connection this hook already holds because a `-qmp …,server`
            // socket serves one connection at a time. Nothing above presses
            // End, so its release is unambiguous; without it every green run
            // waited out the binary's whole 8 s fallback against half a second
            // of typing.
            tap(&plain("end"));
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if !result.stdout.contains("locale: Keyboard layout set to 'swiss-german'") {
        return Err(format!("the real command did not select the layout:\n{}", result.stdout));
    }
    // And the surface was told, and re-read the config rather than being sent
    // a name it had to trust. Without this the assertion below would pass on a
    // gate binary that had simply been built with the layout hard-coded.
    if !result.stdout.contains("surface: layout is now swiss-german") {
        return Err(format!(
            "the surface hosting `locale` never re-read the config it wrote:\n{}",
            result.stdout
        ));
    }

    let events = parse_key_events(&result.stdout);
    if events.is_empty() {
        return Err(format!("no key event reached userland:\n{}", result.stdout));
    }
    let typed: String = events
        .iter()
        .filter(|e| e.modifiers & 0x10 == 0)
        .map(|e| e.translated.as_str())
        .collect();
    // Modifier presses translate to nothing, so the characters are contiguous.
    let want = "zyüöäà@€[<>\\êÊÜ^é^q§";
    if !typed.contains(want) {
        return Err(format!("typed {typed:?}\n  want it to contain {want:?}"));
    }
    // The ISO key really was HID 0x64 and not something the profile faked.
    if !events.iter().any(|e| e.usage == 0x64) {
        return Err(format!("no event for the ISO key in {events:?}"));
    }
    eprintln!("  [swiss-german] {} events, typed {typed:?}", events.len());
    Ok(())
}

/// Keys nothing is listening for, after the ones that are.
///
/// The wizard exits within milliseconds of its last answer, and on a machine
/// that then has nothing to do the kernel's log ring sits one line behind — so
/// the runner's `===TEST_END===` stays in it and the harness waits out its
/// whole timeout for a test that finished. Escape presses after the wizard has
/// gone are discarded by the next reader, and each one is an i8042 interrupt
/// that keeps the ring draining. `i8042_no_spurious_wake` records the same
/// property from the other side: a guest polling its handle keeps it moving.
fn keep_the_ring_moving(input: &mut qemu::QmpInput) {
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(150));
        input.keys(&[("esc", true), ("esc", false)]);
    }
}

/// The wizard, answered as a Swiss keyboard's owner would answer it.
fn locale_detect(qemu: &mut QemuInstance) -> Result<(), String> {
    let result = qemu.run_test_hooked(
        "test_rs_locale_gate detect",
        Duration::from_secs(30),
        "Press the key labelled",
        |socket| {
            let mut input = qemu::QmpInput::open(socket);
            // The key a Swiss board prints `Z` on, then the one it prints `§`
            // on, then Enter to confirm.
            for key in ["y", "grave_accent", "ret"] {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(60));
            }
            keep_the_ring_moving(&mut input);
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    for want in [
        "detect: Press the key labelled  Z",
        "detect: Press the key labelled  \u{a7}",
        "detect: That is 'swiss-german'",
        "detect: Keyboard layout set to 'swiss-german'",
        // The wizard held the surface's keys for the whole conversation, and
        // the surface acted on the config it wrote. Both are new: this ran on
        // a machine whose keyboard the gate binary claims, which is the state
        // that used to make the wizard refuse.
        "surface: client",
        "surface: layout is now swiss-german",
    ] {
        if !result.stdout.contains(want) {
            return Err(format!("no {want:?} in:\n{}", result.stdout));
        }
    }
    eprintln!("  [locale detect] identified swiss-german in two presses");
    Ok(())
}

/// The negative control, in the guest: presses no layout agrees with must end
/// in a refusal, never in a verdict.
fn locale_detect_unrecognized(qemu: &mut QemuInstance) -> Result<(), String> {
    let result = qemu.run_test_hooked(
        "test_rs_locale_gate detect",
        Duration::from_secs(30),
        "Press the key labelled",
        |socket| {
            let mut input = qemu::QmpInput::open(socket);
            // `y` is a QWERTZ answer; `d` is where no layout puts `§`.
            for key in ["y", "d"] {
                input.keys(&[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(60));
            }
            keep_the_ring_moving(&mut input);
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if !result.stdout.contains("detect: Unrecognized") {
        return Err(format!("the wizard did not refuse:\n{}", result.stdout));
    }
    if result.stdout.contains("Keyboard layout set to") {
        return Err(format!("the wizard applied a layout it could not identify:\n{}", result.stdout));
    }
    Ok(())
}

/// A round trip through a guest that is **demonstrably up**, corrected for how
/// fast this host is and not for how many guests it is running.
///
/// [`qemu::budget`] is the ceiling on a guest that might be wedged, and it
/// multiplies by the width because a guest with a twelfth of the machine takes
/// longer over everything. This is the other case, and the width is wrong for
/// it: what these callers wait on is the shell echoing a line it has not run
/// yet, which is microseconds of guest time however little of the machine the
/// guest has. Ten of those establishing nothing is a keystroke path that is not
/// working, and a width-scaled ceiling turns that into four minutes of a lane —
/// measured, on the run this was written from: 285 s of a terminal parked on a
/// pipe it had been parked on since 1.4 s.
fn round_trip(one_guest: Duration) -> Duration {
    let (_, _, num, den) = qemu::host_speed();
    one_guest * num / den
}

/// Keep collecting serial into `log` until `marker` shows up.
///
/// **A pace, not a guard.** The two remaining callers retype at the guest and
/// ask whether the answer came back in the meantime, so `false` is an ordinary
/// step of the loop rather than a finding. Anything waiting on a guest to do
/// something wants [`await_marker`], whose ceiling is the guest going quiet.
fn serial_until(
    qemu: &mut QemuInstance,
    log: &mut String,
    marker: &str,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        log.push_str(&qemu.drain_serial(Duration::from_millis(200)));
        if log.contains(marker) {
            return true;
        }
    }
    false
}

/// [`serial_until`] over what arrives *after* `from`.
///
/// For a marker a test asks for more than once. `serial_until` scans the whole
/// capture, so the second ask is answered by the first ask's line and the test
/// carries on against a guest that has not done the thing yet — which is the
/// same defect as reusing a nonce, one layer down.
fn serial_until_new(
    qemu: &mut QemuInstance,
    log: &mut String,
    marker: &str,
    from: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        log.push_str(&qemu.drain_serial(Duration::from_millis(200)));
        if log[from.min(log.len())..].contains(marker) {
            return true;
        }
    }
    false
}

/// The wizard's two answers, as a Swiss keyboard's owner gives them: the key
/// that prints `Z`, the key that prints `§`, then Enter to confirm.
const SWISS_ANSWERS: [&str; 3] = ["y", "grave_accent", "ret"];

/// Type `line` and press Enter, at whatever prompt is in front of the guest.
fn type_line(input: &mut qemu::QmpInput, line: &str) {
    input.type_text(line);
    input.keys(&[("ret", true), ("ret", false)]);
}

/// Wait until keys typed at the guest reach a shell and come back out.
///
/// **There is no prompt to wait for.** `/bin/shell` writes `"{cwd}> "` with no
/// newline and the harness's serial reader is line-based, so the prompt is not
/// a line and never will be — which is why `screen_console_shell` reads the
/// panel instead. The handshake here is a command whose *echo* is a line, and
/// it is retried because the first keystrokes can land before the shell has
/// its stdin.
fn shell_answers(qemu: &mut QemuInstance, log: &mut String) -> Result<(), String> {
    shell_echoes(qemu, log, "surface-up-zqjxk")
}

/// [`shell_answers`] with the nonce named, for a caller that asks more than
/// once.
///
/// The nonce must differ between asks. `serial_until` scans everything
/// captured so far, so a later question with an earlier answer still in the
/// log is answered by that one whatever the shell is doing.
///
/// **Two waits, because the two ways this fails are different questions.** The
/// first is "has the terminal come up", and it used to be answered by retyping
/// against `qemu::budget(20 s)` — a guess at how long a desktop takes to come up
/// on the host of the day, which is exactly the shape `issues/design-debt/`
/// bills for: `desktop_audio_client` 385 s wide against 13 s alone, and a
/// landing gate that is a coin toss. The terminal knows when it is up and now
/// says so, so this asks it and waits on the guest's own liveness. The second is
/// "does a keystroke reach the shell", and it starts from a machine that is
/// demonstrably up — a ceiling on *that* is a claim about the guest.
fn shell_echoes(qemu: &mut QemuInstance, log: &mut String, nonce: &str) -> Result<(), String> {
    // Whichever surface owner this config put a shell behind, printed once its
    // screen exists and the shell's stdin is a pipe it holds. Before that a
    // keystroke lands nowhere and leaves no trace. Both, because `shell_answers`
    // is asked of a terminal under the compositor and of `/bin/console` on the
    // raw framebuffer, and the question is the same one.
    const SURFACE_UP: [&str; 2] = ["terminal: ready", "console: ready"];
    // **And the state in which it is never coming.** `/bin/terminal` exits when
    // it loses the race with the compositor (`issues/kernel/`), which is a fact
    // the log states outright at 0.6 s — so waiting for a ready marker that
    // cannot arrive is not a slow guest but a defect, and the only thing a
    // ceiling decides there is how many minutes of a lane it costs to say so.
    // Measured on the run this came from: 305 s, against a terminal that had
    // exited before the compositor was ready.
    const SURFACE_GONE: [&str; 2] = ["exit: terminal ", "exit: console "];
    let up = |log: &str| SURFACE_UP.iter().any(|m| log.contains(m));
    let gone = |log: &str| SURFACE_GONE.iter().any(|m| log.contains(m));
    await_guest(qemu, log, "a surface to say it is up", |log| up(log) || gone(log))?;
    if !up(log) {
        return Err(
            "the surface owner exited before it ever said it was ready — /bin/terminal races \
             the compositor at boot, `issues/kernel/`"
                .to_string(),
        );
    }

    // Retyping rather than waiting longer: a keystroke injected between two of
    // the terminal's polls is dropped, and a dropped one leaves nothing to wait
    // for.
    //
    // **A count of attempts, not a span of host seconds.** This used to be a
    // flat twenty, which is a fixed number of round trips on the host it was
    // written on and a different number on any other. Ten is the number, and
    // each gets a round trip scaled to this host — see [`round_trip`] for why
    // that and not the phase width.
    const TRIES: usize = 10;
    for _ in 0..TRIES {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            type_line(&mut input, &format!("echo {nonce}"));
        }
        if serial_until(qemu, log, nonce, round_trip(Duration::from_secs(2))) {
            return Ok(());
        }
    }
    Err(format!("{TRIES} typed lines and none of them came back"))
}

/// A shell must get its prompt back when a windowed child's window goes.
///
/// The owner opened snake, closed its window with the X button, and never saw
/// a prompt again. Both readings of his log are testable here and the two
/// probes separate them: the first ends the child by *its own* exit, the
/// second by the compositor taking its window away while it is alive —
/// GUI+Q, which is the same `windows.remove` + `MSG_WINDOW_CLOSE` + drop the
/// X button runs and is a keystroke rather than a guess at where the button
/// is.
///
/// The client is a bare `window::Window`, so a reproduction here is about the
/// shell, the terminal and the window protocol, and a clean run narrows the
/// defect to what winit does that this does not.
/// Close the focused window with GUI+Q, retrying until the compositor says a
/// window went.
///
/// **Never blind, and what it waits for is the close itself.** A keystroke
/// injected while the guest is busy is lost, so one attempt is not enough on a
/// loaded host; but a second GUI+Q *after* one worked closes the next window
/// down, which here is the terminal, and that takes the shell and the whole
/// desktop with it. The compositor emits `window closed` from the close, so
/// this waits on the event it caused. Waiting on the `windows=N` count instead
/// is what made this re-send: that count is a sample taken every two seconds,
/// so it answers about an interval rather than about this injection — and the
/// wait was `serial_until`, which scans the whole capture, so the *previous*
/// probe's `windows=1` returned it immediately and the loop hammered GUI+Q at
/// the speed of a QMP round trip.
///
/// The ceiling is the guest's own liveness rather than a phase-scaled clock,
/// and here that cuts both ways: #156 is a *freeze*, so the machine this
/// retries against goes silent, and the wait ends in fifteen seconds instead of
/// spending `qemu::budget(20 s)` — up to four minutes at width 12 — hammering
/// GUI+Q at a desktop that has stopped. `issues/design-debt/` names that
/// cost as a lane this test holds for a quarter of every run, which is what puts
/// whichever desktop is dispatched beside it into a red nobody acts on.
fn close_focused_window(qemu: &mut QemuInstance, log: &mut String, new: usize) -> bool {
    const CLOSED: &str = "compositor: window closed";
    let mut live = qemu::Liveness::new(Duration::from_secs(15), Duration::from_secs(60));
    while !log[new..].contains(CLOSED) && live.working(log) {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            input.keys(&[("meta_l", true), ("q", true), ("q", false), ("meta_l", false)]);
        }
        serial_until_new(qemu, log, CLOSED, new, Duration::from_secs(4));
    }
    log[new..].contains(CLOSED)
}

/// How many times snake is opened and closed. One green round says very little
/// about a report that arrived once.
const SNAKE_ROUNDS: usize = 3;
/// Turns played in the last round, at four keys each, so that round's snake is
/// a program that has been running and drawing rather than one a second old.
const SNAKE_TURNS: usize = 8;

/// Gate: doom's music reaches the device, with the SoundFont this tree ships.
///
/// **The wiring is all this measures, and the wiring is the part nothing else
/// can.** `src/soundfont.rs`'s host tests say the committed bank covers every
/// instrument `assets/DOOM1.WAD` selects, and the subset was measured to render
/// bit-exact against the full bank through this same
/// `mus2mid.c` and this same rustysynth. Neither can say the file got into an
/// initrd, that doom opened it, or that what came out reached an audio device.
/// Those three are what `b8b0749` broke for a cycle with the suite green.
///
/// Three verdicts, none of them a clock:
///
/// 1. **doom opened the file this tree committed.** The guest prints the byte
///    count it read, and the host compares it against `assets/soundfont.sf2` on
///    disk. A stale initrd, a truncated asset and a second SoundFont from
///    somewhere else all fail here rather than turning into quiet silence.
/// 2. **It played to the end of the check.** The actuator counts the audio
///    callback's own periods, so a host that stopped this guest cannot shorten
///    what the capture is judged on.
/// 3. **Music reached the wire.** The device capture carries signal across most
///    of its length — which separates music from the one thing a broken
///    soundfont path still produces, a stream of zeroes.
fn doom_music(rust_bins: &[(String, Vec<u8>)]) -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let shipped = fs::metadata(root.join(toyos_build::soundfont::SOUNDFONT_PATH))
        .map_err(|e| format!("{}: {e}", toyos_build::soundfont::SOUNDFONT_PATH))?
        .len();

    let config = root.join("tests/doommusiccase");
    let mut qemu = QemuInstance::boot_with_options(&config, &[], rust_bins, BootOptions::default());

    let result = qemu.run_test("test_rs_doom_music", Duration::from_secs(120));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "doom could not play its own music (exit {:?}):\n{}",
            result.exit_code, result.stdout
        ));
    }

    let opened = result
        .stdout
        .lines()
        .find(|line| line.contains("[doom-sound] /share/soundfont.sf2:"))
        .ok_or_else(|| {
            format!(
                "doom said nothing about the SoundFont, so this image has none:\n{}",
                result.stdout
            )
        })?;
    let bytes: u64 = opened
        .split_whitespace()
        .find_map(|token| token.parse().ok())
        .ok_or_else(|| format!("no byte count in {opened:?}"))?;
    if bytes != shipped {
        return Err(format!(
            "doom opened a {bytes}-byte SoundFont and this tree ships {shipped} bytes: the \
             image is not carrying {}",
            toyos_build::soundfont::SOUNDFONT_PATH
        ));
    }

    let played = result
        .stdout
        .lines()
        .find(|line| line.contains("[music-check] lump="))
        .ok_or_else(|| format!("doom printed no [music-check] line:\n{}", result.stdout))?
        .to_string();

    let _ = qemu.drain_serial(Duration::from_millis(500));
    let wav = audio::parse_wav(qemu.audio_wav_path())?;
    let analysis = audio::analyze(&wav);

    // Seconds of signal, not a fraction of the capture: the capture runs from
    // soundd opening the stream to the harness closing the file, so a fraction
    // measures the harness as much as the music. Three of these four runs
    // measured 1.19 s over a 3.11 s capture — the material's own dynamics, not
    // a shortfall, since 500 LSB is -36 dBFS and E1M1's riff drops through it
    // between notes.
    const MIN_SIGNAL_SECS: f64 = 0.8;
    let signal = analysis.active_samples as f64 / wav.sample_rate as f64;
    if signal < MIN_SIGNAL_SECS {
        return Err(format!(
            "{signal:.2} s of the capture carries signal, under {MIN_SIGNAL_SECS} s: what doom \
             rendered is not what the device played\n{played}"
        ));
    }
    // A floor and not a band: how loud E1M1 is at a given moment is the
    // arrangement's business, and what this excludes is a dither floor being
    // read as music. Measured 13547.
    const MIN_PEAK: i32 = 6000;
    if analysis.peak < MIN_PEAK {
        return Err(format!(
            "the device peaked at {} (expected at least {MIN_PEAK}): the music is inaudible\
             \n{played}",
            analysis.peak
        ));
    }

    // Underruns are reported and fail nothing: whether music *stutters* is gate
    // A's question and it has the statistics to ask it, where one boot of one
    // track is one sample of an intermittent.
    eprintln!(
        "  [doommusiccase] {}, {signal:.2} s of signal in a {:.2} s capture at peak {}, \
         {} underrun(s)",
        played.trim(),
        wav.mono.len() as f64 / wav.sample_rate as f64,
        analysis.peak,
        analysis.underruns.len(),
    );
    Ok(())
}

fn desktop_window_child(rust_bins: &[(String, Vec<u8>)]) -> Result<(), String> {
    let bins: Vec<(String, Vec<u8>)> =
        rust_bins.iter().filter(|(name, _)| name == "window_child").cloned().collect();
    if bins.is_empty() {
        return Err("the window_child client was not built".to_string());
    }
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopcase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        qmp: true,
        ready_marker: "compositor: ready",
        // The T14's core count. A desktop's teardown is four processes handing
        // pipes back to each other, and on two cores most of that is ordered
        // by having nowhere else to run.
        smp: 8,
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &bins, options);
    let mut log = qemu.boot_log().to_string();
    match window_child_probes(&mut qemu, &mut log) {
        Ok(()) => Ok(()),
        Err(message) => Err(format!("{message}\n{}", freeze_report(&mut qemu, &mut log))),
    }
}

/// What a desktop that stopped answering is asked, in the order that survives
/// being asked.
///
/// Every vCPU's registers first. `HLT=1` with `IF` set in `RFL` is a machine
/// with nothing to run rather than one wedged below the interrupt layer, and
/// that is the question #156 turns on — but Ctrl+Alt+D revives a halted CPU,
/// so the dump destroys the evidence it is taken to explain. Asked in the
/// other order, both halves describe the repaired machine.
fn freeze_report(qemu: &mut QemuInstance, log: &mut String) -> String {
    let registers = {
        let mut monitor = qemu::QmpMonitor::open(qemu.qmp_socket());
        monitor.human("info registers -a")
    };
    let before = log.len();
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        input.keys(&[
            ("ctrl", true),
            ("alt", true),
            ("d", true),
            ("d", false),
            ("alt", false),
            ("ctrl", false),
        ]);
    }
    let whole = serial_until(qemu, log, "=== end of dump ===", Duration::from_secs(30));
    format!(
        "--- info registers -a, taken before this report injected anything ---\n{registers}\n\
         --- Ctrl+Alt+D{} ---\n{}",
        if whole { "" } else { ", which produced no complete report" },
        &log[before.min(log.len())..]
    )
}

fn window_child_probes(qemu: &mut QemuInstance, log: &mut String) -> Result<(), String> {
    if let Err(why) = shell_answers(qemu, log) {
        return Err(format!(
            "{why}\nnothing typed at the terminal window reached a shell:\n{log}"
        ));
    }

    // A windowed child that leaves on its own. The shell is in `waitpid` and
    // the compositor never touches its connection, so this is the plain case
    // and it has to work before the second probe means anything.
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "test_rs_window_child exit");
    }
    if !serial_until(qemu, log, "WINDOW-CHILD-GONE", qemu::budget(Duration::from_secs(20))) {
        return Err(format!("the windowed child never reported leaving:\n{log}"));
    }
    if let Err(why) = shell_echoes(qemu, log, "after-own-exit-zqjxk") {
        return Err(format!(
            "{why}\na windowed child exited by itself and the shell never answered again:\n{log}"
        ));
    }

    // The owner's case: the process is alive and the compositor takes its
    // window away underneath it.
    let started = log.len();
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "test_rs_window_child");
    }
    // Its own marker, not the one the probe above already printed.
    if !serial_until_new(
        qemu,
        log,
        "WINDOW-CHILD-UP",
        started,
        qemu::budget(Duration::from_secs(20)),
    ) {
        return Err(format!("the windowed child never got a window:\n{log}"));
    }
    // GUI+Q closes the focused window, and a window the compositor has just
    // created is the focused one. Re-injected until the compositor says the
    // window went — a keystroke that lands while the guest is busy is lost —
    // and never blind, because a second GUI+Q after one worked would close the
    // terminal's window instead.
    let before = log.len();
    if !close_focused_window(qemu, log, before) {
        return Err(format!(
            "GUI+Q never reached the compositor:\n{}",
            &log[before.min(log.len())..]
        ));
    }
    if !serial_until_new(qemu, log, "WINDOW-CHILD-GONE", before, qemu::budget(Duration::from_secs(20))) {
        return Err(format!(
            "the compositor closed the window and the client did not leave:\n{}",
            &log[before.min(log.len())..]
        ));
    }
    if log[before..].contains("WINDOW-CHILD-TIMEOUT") {
        return Err(format!(
            "the client left on its own deadline, so it never saw the close:\n{}",
            &log[before..]
        ));
    }
    if let Err(why) = shell_echoes(qemu, log, "after-window-closed-zqjxk") {
        return Err(format!(
            "{why}\nthe compositor closed a child's window and the shell never answered again \
             — this is the owner's snake report, reproduced:\n{log}"
        ));
    }
    // And the program he actually ran. Everything above is a `window::Window`
    // and nothing else; snake is that under winit and softbuffer, which is the
    // only difference left between this test and his session.
    //
    // Three rounds, and the last one is played first: his snake had run 39 s
    // and spent 22.4 s of CPU when he closed it, and a window closed one
    // second after it opened exercises a quieter program than that. One green
    // round would say very little about a report that arrived once.
    for round in 0..SNAKE_ROUNDS {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            type_line(&mut input, "snake");
        }
        // snake prints nothing of its own, so the compositor's second window
        // is what says it is up — and a window it has just created is the
        // focused one, which is what GUI+Q then closes.
        let opened = log.len();
        if !serial_until_new(qemu, log, "windows=2", opened, qemu::budget(Duration::from_secs(20))) {
            return Err(format!("snake never got a window in round {round}:\n{log}"));
        }
        if round + 1 == SNAKE_ROUNDS {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            for _ in 0..SNAKE_TURNS {
                for key in ["left", "down", "right", "up"] {
                    input.keys(&[(key, true), (key, false)]);
                    thread::sleep(Duration::from_millis(120));
                }
            }
        }
        let before = log.len();
        if !close_focused_window(qemu, log, before) {
            return Err(format!(
                "GUI+Q never reached the compositor in round {round}:\n{}",
                &log[before.min(log.len())..]
            ));
        }
        if !serial_until_new(qemu, log, "exit: snake", before, qemu::budget(Duration::from_secs(20))) {
            return Err(format!(
                "snake did not leave when its window was closed in round {round}:\n{}",
                &log[before.min(log.len())..]
            ));
        }
        if let Err(why) = shell_echoes(qemu, log, &format!("after-snake-{round}-zqjxk")) {
            return Err(format!(
                "{why}\nsnake's window was closed, snake left, and the shell never answered \
                 again (round {round}) — the owner's report, reproduced:\n{log}"
            ));
        }
    }

    eprintln!(
        "  [desktop] a windowed child and {SNAKE_ROUNDS} snakes each left both ways and the \
         shell kept its prompt"
    );
    Ok(())
}

/// What a typed character costs the desktop.
///
/// The owner's report, in his words: entering one character into the terminal
/// redraws the entire terminal. It did, and the mechanism was that `MSG_PRESENT`
/// carried no damage — the emulator already blits one cell into the shared
/// buffer, and the compositor, told only that something had changed, repainted
/// the whole window. The terminal here fills most of the screen, so that was
/// nine tenths of the panel per keystroke.
///
/// The gate is the compositor's own `damage_px_max`, the largest single frame
/// of a reporting interval, over the intervals in which the typing happened.
/// The clock's readout is 0.46% of this screen and is in every interval; a
/// typed character is a two-cell span, far below it; a repainted window is 89%.
/// Two percent sits between them by a factor of forty either way.
fn desktop_typing_damage() -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopcase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        qmp: true,
        ready_marker: "compositor: ready",
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = qemu.boot_log().to_string();
    if let Err(why) = shell_answers(&mut qemu, &mut log) {
        return Err(format!(
            "{why}\nnothing typed at the terminal window reached a shell:\n{log}"
        ));
    }
    let screen_px = compositor_screen_px(&log)?;

    // Let the interval carrying the boot's full-screen repaint and the
    // terminal's first paint close before anything here is measured. Those are
    // real frames and they are not what this is about.
    await_marker(&mut qemu, &mut log, "compositor: frames=", "the compositor to report an interval")
        .map_err(|why| format!("{why}\n{log}"))?;
    log.push_str(&qemu.drain_serial(Duration::from_secs(3)));
    let before = log.len();

    // Eight lines, each typed a character at a time — the shell's echo of each
    // keystroke is a present of its own, which is the thing being measured.
    //
    // Eight and not more because the terminal must not scroll while this runs.
    // A scroll changes every cell and is honestly a whole-window repaint, so it
    // would fail this gate for the one reason that is not a defect. The window
    // is 58 text rows on this screen, `shell_answers` leaves under ten of them
    // used, and eight commands echoed and answered are twenty-four.
    const NONCE: &str = "typing-damage-gate";
    // **Guest-paced.** The eight lines used to go in on a 250 ms host cadence
    // and the sixteen appearances were then waited for; the waiting was already
    // right and the typing was not. A keystroke injected faster than the guest
    // drains its keyboard is a keystroke that never damages a cell, so on a
    // contended host this measured whatever fraction survived — 2 of 16 on a
    // four-guest CI runner, and the message it produced named the shortfall
    // rather than the cause. Each line now waits for its own echo before the
    // next goes in, which costs a slow guest wall clock and never the stimulus.
    for line in 0..8u32 {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            type_line(&mut input, &format!("echo {NONCE}"));
        }
        // Two: the shell echoes the command as it is typed and again as its
        // output. The same arithmetic the verdict below makes.
        let want = ((line + 1) * 2) as usize;
        let mut live = qemu::Liveness::new(Duration::from_secs(15), Duration::from_secs(60));
        while log[before..].matches(NONCE).count() < want && live.working(&log) {
            let seen = qemu.drain_serial(Duration::from_millis(100));
            log.push_str(&seen);
        }
    }
    log.push_str(&qemu.drain_serial(Duration::from_secs(3)));

    let typed = &log[before..];
    // Sixteen: the shell echoes the command as it is typed and again as its
    // output, so eight lines are sixteen appearances. Counting the echo alone
    // would pass on a terminal that painted the keystrokes and never ran them.
    let echoes = typed.matches(NONCE).count();
    if echoes < 16 {
        return Err(format!(
            "{echoes} of the sixteen appearances the eight typed lines owe reached the console, \
             so most of what this measures never happened:\n{typed}"
        ));
    }
    let mut biggest = 0;
    let mut intervals = 0;
    for line in typed.lines().filter(|l| l.contains("compositor: frames=")) {
        intervals += 1;
        biggest = biggest.max(compositor_field(line, "damage_px_max=")?);
    }
    if intervals == 0 {
        return Err(format!("the compositor reported no interval while typing:\n{typed}"));
    }
    if biggest * 50 > screen_px {
        return Err(format!(
            "a keystroke's frame repainted {biggest} of {screen_px} pixels — over two percent of \
             the screen for one character:\n{typed}"
        ));
    }
    eprintln!(
        "  [desktop] eight lines typed, {echoes} appearances; biggest frame {biggest} of \
         {screen_px} px over {intervals} intervals"
    );
    Ok(())
}

/// The wizard under `/bin/console`, which is the whole of the surface tree on
/// a machine with no compositor — and the image that gets flashed.
///
/// This is one of the two tests that replaced the refusal gate. `/bin/console`
/// claims the keyboard for its entire run, which is exactly the state that
/// used to make `locale detect` print "cannot read the keyboard directly" and
/// stop; the wizard now asks the console for the transitions instead. The
/// closing assertion is that the console's *own* translator moved with the
/// config: the key a US board prints `[` on types `ü` afterwards, and nothing
/// but a re-read of the file this wizard wrote can do that.
fn console_locale_detect() -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("console");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        qmp: true,
        ready_marker: "console: ready",
        ..Default::default()
    };
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = qemu.boot_log().to_string();
    if let Err(why) = shell_answers(&mut qemu, &mut log) {
        return Err(format!("{why}\nnothing typed at /bin/console reached a shell:\n{log}"));
    }

    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "locale detect");
        await_marker(
            &mut qemu,
            &mut log,
            "Press the key labelled",
            "the wizard to ask for a key under /bin/console — the console did not lend it \
             the keyboard",
        )
        .map_err(|why| format!("{why}\n{log}"))?;
        for key in SWISS_ANSWERS {
            input.keys(&[(key, true), (key, false)]);
            thread::sleep(Duration::from_millis(120));
        }
    }

    for want in ["That is 'swiss-german'", "Keyboard layout set to 'swiss-german'"] {
        await_marker(&mut qemu, &mut log, want, &format!("{want:?} under /bin/console"))
            .map_err(|why| format!("{why}\n{log}"))?;
    }
    // The console acted on the notification. A prefix, not the whole line: the
    // console is shared and not line-atomic, so a kernel line lands inside
    // this one often enough to matter (it did, first time this ran). *Which*
    // layout it re-read is the assertion below, which does not depend on a
    // line surviving intact.
    await_marker(
        &mut qemu,
        &mut log,
        "console: keyboard layout",
        "the console to re-read the config the wizard wrote",
    )
    .map_err(|why| format!("{why}\n{log}"))?;

    // And the layout is in force for what is typed next. `bracket_left` is the
    // key a US board prints `[` on and a Swiss one prints `ü` on, so this is
    // the substitution the whole exercise exists to make, taken through the
    // console's translator and the shell.
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        input.type_text("echo ");
        input.keys(&[("bracket_left", true), ("bracket_left", false)]);
        input.keys(&[("ret", true), ("ret", false)]);
    }
    await_marker(&mut qemu, &mut log, "\u{fc}", "the `[` key to produce `ü`")
        .map_err(|why| format!(
            "{why}\ntyping the `[` key after the wizard did not produce `ü`, so the console is \
             still translating with the layout it booted with\n{log}"
        ))?;
    eprintln!("  [console] the wizard identified swiss-german and the console adopted it");
    Ok(())
}

/// The wizard under `/bin/terminal`, on a desktop.
///
/// The other half of the refusal gate's replacement, and the deepest the
/// surface tree goes: the compositor claims the keyboard and forwards whole
/// transitions to the focused window, `window::Window` holds the terminal's
/// translator, and the terminal lends the transitions to the wizard three
/// processes below it. Every one of those hops is a place the old design had
/// nothing but translated bytes.
fn desktop_locale_detect() -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopcase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        qmp: true,
        ready_marker: "compositor: ready",
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = qemu.boot_log().to_string();
    if let Err(why) = shell_answers(&mut qemu, &mut log) {
        return Err(format!(
            "{why}\nnothing typed at the terminal window reached a shell:\n{log}"
        ));
    }

    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "locale detect");
        await_marker(
            &mut qemu,
            &mut log,
            "Press the key labelled",
            "the wizard to ask for a key inside a terminal — the compositor or the terminal \
             did not carry the transitions",
        )
        .map_err(|why| format!("{why}\n{log}"))?;
        for key in SWISS_ANSWERS {
            input.keys(&[(key, true), (key, false)]);
            thread::sleep(Duration::from_millis(120));
        }
    }

    for want in ["That is 'swiss-german'", "Keyboard layout set to 'swiss-german'"] {
        await_marker(&mut qemu, &mut log, want, &format!("{want:?} inside a terminal"))
            .map_err(|why| format!("{why}\n{log}"))?;
    }

    // The same substitution as the console gate, one surface deeper: the
    // config went up to the compositor and came back down to this window's
    // translator.
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        input.type_text("echo ");
        input.keys(&[("bracket_left", true), ("bracket_left", false)]);
        input.keys(&[("ret", true), ("ret", false)]);
    }
    await_marker(&mut qemu, &mut log, "\u{fc}", "the `[` key to produce `ü`")
        .map_err(|why| format!(
            "{why}\ntyping the `[` key after the wizard did not produce `ü`, so the \
             compositor's broadcast never reached the terminal's translator\n{log}"
        ))?;
    eprintln!("  [desktop] the wizard ran three processes below the compositor");
    Ok(())
}

/// A shell-spawned audio client on a device-less desktop, and the desktop
/// afterwards.
///
/// The machine `metal_sim_null_audio` and `null_sink_shipped_client` both miss:
/// they spawn the client from a test binary whose stdio is the console, and the
/// T14 spawns it from a shell inside a terminal inside the compositor, so every
/// one of the client's three descriptors is a pipe to a surface. Three verdicts
/// on one boot, in the order the T14 lost them:
///
/// 1. **A client finishes.** `tone` writes a second of audio to the null sink
///    and prints its own completion line.
/// 2. **A second client connects while the first is streaming.** The T14's log
///    shows soundd's control thread printing `opening stream` for the second
///    with no `client N connected` behind it, so the connect is what has to be
///    observed, not just the exit.
/// 3. **The desktop survives them.** A terminal opened afterwards reaches a
///    shell that answers — the verdict the owner's machine failed while the
///    compositor was still painting, which is why nothing that reads pixels or
///    counts frames would have caught it.
fn desktop_audio_client() -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopaudiocase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        // The T14's core count: the suite's default of two serialises threads
        // this shape is about the wakes between.
        smp: 8,
        qmp: true,
        ready_marker: "compositor: ready",
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = qemu.boot_log().to_string();

    const NULL_LINE: &str = "soundd: no audio device, presenting a null sink";
    await_marker(&mut qemu, &mut log, NULL_LINE, "soundd to present a null sink")
        .map_err(|why| format!("{why}\n{log}"))?;
    if let Err(why) = shell_answers(&mut qemu, &mut log) {
        return Err(format!(
            "{why}\nnothing typed at the terminal window reached a shell:\n{log}"
        ));
    }

    // One client, start to finish. `tone: done` is the client's own last line,
    // so it is the client saying it got its callbacks and left — not the shell
    // saying it launched something.
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "tone 440 1");
    }
    await_marker(
        &mut qemu,
        &mut log,
        "tone: done",
        "a shell-spawned tone to finish on a device-less desktop",
    )
    .map_err(|why| format!("{why}\n{log}"))?;

    // Two clients overlapping, each under its own shell in its own terminal.
    // The shell has no job control, so the long tone holds its terminal and the
    // second one has to be typed somewhere else — which is exactly how the T14
    // reached two live clients, and why the second terminal is part of the
    // stimulus rather than only part of the verdict.
    let before_second = log.len();
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "tone 660 8");
    }
    await_marker_new(
        &mut qemu,
        &mut log,
        "tone: 660Hz",
        before_second,
        "the long tone to start",
    )
    .map_err(|why| format!("{why}\n{}", &log[before_second..]))?;
    open_terminal(&mut qemu, &mut log, "overlap-terminal-jc4t")?;
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "tone 440 1");
    }
    // **The count is the verdict and the wait is not.** Both of these used to be
    // `budget(60 s)`, which is a claim that a desktop with two audio clients on
    // it finishes inside a minute times the width — and at 385 s wide against
    // 13 s alone it was the single most expensive entry in `issues/design-debt/`. What
    // ends the wait now is soundd going quiet, and what fails it is still the
    // number of connects.
    if let Err(why) = await_guest(&mut qemu, &mut log, "soundd to take up both connects", |log| {
        connects_since(log, before_second) >= 2
    }) {
        return Err(format!(
            "{why}\nsoundd applied {} of the two connects — a client that opened a stream \
             was never taken up by the mixer:\n{}",
            connects_since(&log, before_second),
            &log[before_second..]
        ));
    }
    // Both of them out again, counted in the same window. Waiting for `null
    // sink idle` would not do: that line is already in the log from the first
    // client, and a marker an earlier phase produced is not a verdict about
    // this one.
    if let Err(why) = await_guest(&mut qemu, &mut log, "both clients to leave the mixer", |log| {
        removals_since(log, before_second) >= 2
    }) {
        return Err(format!(
            "{why}\n{} of the two overlapping clients left the mixer — the other one is \
             still streaming to a sink that stopped draining it:\n{}",
            removals_since(&log, before_second),
            &log[before_second..]
        ));
    }

    // The desktop afterwards: a process created after every one of the clients
    // above, focused the moment it maps its window. This is the verdict the
    // owner's machine failed while the compositor was still painting, which is
    // why nothing that reads pixels or counts frames would have caught it.
    open_terminal(&mut qemu, &mut log, "post-audio-desktop-vqmz")?;
    eprintln!("  [desktop] three shell-spawned audio clients ran and the desktop still answers");
    Ok(())
}

/// Ctrl+N at the compositor, and a shell in the window it opens that answers.
///
/// The nonce is per call because the verdict is that *this* terminal answered:
/// a marker an earlier one already produced would pass on a window that never
/// came up. [`shell_echoes`]'s split applies for the same reason it does there,
/// and `terminal: ready` is looked for after `before` rather than anywhere,
/// because every terminal already up has printed one.
fn open_terminal(qemu: &mut QemuInstance, log: &mut String, nonce: &str) -> Result<(), String> {
    let before = log.len();
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        input.keys(&[("ctrl", true), ("n", true), ("n", false), ("ctrl", false)]);
    }
    await_marker_new(qemu, log, "terminal: ready", before, "Ctrl+N to open a terminal")
        .map_err(|why| format!("{why}\n{}", &log[before..]))?;

    // The same count-of-attempts as [`shell_echoes`], and for the same reason.
    const TRIES: usize = 10;
    for _ in 0..TRIES {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            type_line(&mut input, &format!("echo {nonce}"));
        }
        if serial_until(qemu, log, nonce, round_trip(Duration::from_secs(2))) {
            return Ok(());
        }
    }
    Err(format!(
        "a terminal opened with Ctrl+N never reached a shell that answers in {TRIES} typed \
         lines:\n{}",
        &log[before..]
    ))
}

/// Ctrl+Alt+D at a live desktop: every CPU answers, and the two halves of the
/// report agree.
///
/// The instrument `issues/diagnostics/` files against, built because QEMU cannot
/// stage the T14's audio wedge and a question the owner can answer beats a fix
/// nobody can verify. Until this landed the dump listed the *calling* CPU's
/// parked threads and named them by scheduler key, so it could confirm a park
/// and never rule one out — and the three states that look identical from
/// outside (parked on a deadline that did not fire, parked on a deadline
/// nothing could reach, held by no CPU at all) were not distinguishable at all.
///
/// Eight CPUs, because "machine-wide" is not testable at the suite's default of
/// two: one CPU short of the whole machine is what the old dump already did.
///
/// **The verdict is the instrument, not the guest's health.** A deadline that
/// has passed and whose pass has not yet run is a legitimate microsecond-wide
/// state, so asserting zero of them would be asserting a race. What is asserted
/// is that the report is complete and that its halves cannot disagree: every
/// CPU is present, the deadline classes sum to the parked count, and the
/// process table knows at least as many threads as the schedulers hold.
fn blocked_dump() -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/desktopaudiocase");
    let options = BootOptions {
        profile: qemu::Profile::Metal,
        smp: 8,
        qmp: true,
        ready_marker: "compositor: ready",
        ..Default::default()
    };
    metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = qemu.boot_log().to_string();
    if let Err(why) = shell_answers(&mut qemu, &mut log) {
        return Err(format!(
            "{why}\nnothing typed at the terminal window reached a shell:\n{log}"
        ));
    }

    let before = log.len();
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        input.keys(&[
            ("ctrl", true),
            ("alt", true),
            ("d", true),
            ("d", false),
            ("alt", false),
            ("ctrl", false),
        ]);
    }
    await_marker_new(&mut qemu, &mut log, "=== end of dump ===", before, "the whole report")
        .map_err(|why| format!(
            "{why}\nCtrl+Alt+D produced no complete report:\n{}",
            &log[before..]
        ))?;
    let report = log[before..].to_string();

    // Every CPU printed its own line. This is the whole of "machine-wide": the
    // count in the summary is derived, these are the CPUs actually answering.
    let missing: Vec<usize> =
        (0..8).filter(|c| !report.contains(&format!("cpu{c} running"))).collect();
    if !missing.is_empty() {
        return Err(format!(
            "cpu(s) {missing:?} never reported — the dump reached {} of 8:\n{report}",
            8 - missing.len()
        ));
    }
    if !report.contains("8/8 cpu(s) answered") {
        return Err(format!("the report does not claim a whole machine:\n{report}"));
    }
    // On a settled desktop the table is free, so the half of the verdict that
    // only the census can produce must be there. A report that answered two of
    // three questions is worth having on the owner's panel and is not worth
    // accepting from a gate.
    if !report.contains(" unheld, ") || !report.contains(" never ran") {
        return Err(format!(
            "the verdict lost its census half on a settled machine:\n{report}"
        ));
    }

    // A parked line names a process, not a scheduler key.
    let named = report
        .lines()
        .filter(|l| l.contains("pid=") && l.contains("tid=") && l.contains(" parked "))
        .count();
    if named == 0 {
        return Err(format!("no parked task was named by pid and tid:\n{report}"));
    }

    // **All three kernel threads, by name.** They are almost always blocked, so
    // the parked lines above carry them as a pid and a tid and nothing else —
    // and on a machine that has gone quiet the question is *which* of the three
    // is stuck. `sched::dump`'s census tags a kernel thread whatever it is
    // doing, which is C6's gate: three kernel threads split the work —
    // `klogd` the console drain, `usbd` the xHCI port machine, `iod` the
    // write-back queue — precisely so that one of them wedging does not stop
    // the other two. A report that cannot tell them apart cannot say which did.
    //
    // Matched with the ` cpu=` that follows the name on the census line, because
    // a bare name appears in every one of these programs' own log lines and
    // `/bin/init` speaks in a program's name before that program runs
    // (`tests/CLAUDE.md`).
    let unnamed: Vec<&str> = ["klogd", "usbd", "iod"]
        .into_iter()
        .filter(|name| !report.contains(&format!(" {name} cpu=")))
        .collect();
    if !unnamed.is_empty() {
        return Err(format!(
            "the report never names kernel thread(s) {unnamed:?}, so it cannot say which \
             of them is stuck:\n{report}"
        ));
    }

    // The two halves must agree, which is what makes the verdict mean anything:
    // every parked task falls into exactly one deadline class, and every task a
    // scheduler holds is a thread the process table knows.
    let parked = dump_field(&report, "== sched:", "parked")?;
    let classes = dump_field(&report, "== deadlines:", "event-only,")?
        + dump_field(&report, "== deadlines:", "pending,")?
        + dump_field(&report, "== deadlines:", "OVERDUE,")?
        + dump_field(&report, "== deadlines:", "ABSURD")?;
    if parked != classes {
        return Err(format!(
            "{parked} parked task(s) but {classes} classified — the report contradicts \
             itself:\n{report}"
        ));
    }
    let threads = dump_field(&report, "== census:", "thread(s)")?;
    if threads < parked {
        return Err(format!(
            "the schedulers hold {parked} task(s) and the process table knows {threads} \
             thread(s) — the census cannot see what the CPUs do:\n{report}"
        ));
    }

    let verdict = report
        .lines()
        .find(|l| l.contains("== VERDICT:"))
        .ok_or_else(|| format!("no verdict line:\n{report}"))?;
    eprintln!(
        "  [dump] {threads} threads, {parked} parked, all 8 cpus answered;{}",
        verdict.split("VERDICT:").nth(1).unwrap_or("").trim_end()
    );
    Ok(())
}

/// The number the report writes immediately before `word`, on the line that
/// carries `marker`. Read from the word a person sees rather than from a
/// column, so a reordered line does not silently read the wrong field.
fn dump_field(report: &str, marker: &str, word: &str) -> Result<u32, String> {
    let line = report
        .lines()
        .find(|l| l.contains(marker))
        .ok_or_else(|| format!("no {marker:?} line in the report:\n{report}"))?;
    let head = line
        .split(word)
        .next()
        .filter(|h| h.len() < line.len())
        .ok_or_else(|| format!("no {word:?} on {line:?}"))?;
    head.split_whitespace()
        .next_back()
        .and_then(|w| w.parse().ok())
        .ok_or_else(|| format!("no number before {word:?} on {line:?}"))
}

/// Connects the mixer has *applied* since `from`, which is a different event
/// from the control thread's `opening stream` — the T14 log carries the second
/// without the first.
fn connects_since(log: &str, from: usize) -> usize {
    soundd_clients_since(log, from, " connected")
}

/// Clients the mixer has ramped out and dropped since `from`.
fn removals_since(log: &str, from: usize) -> usize {
    soundd_clients_since(log, from, " removed")
}

fn soundd_clients_since(log: &str, from: usize, verb: &str) -> usize {
    log[from..]
        .lines()
        .filter(|l| l.contains("soundd: client ") && l.contains(verb))
        .count()
}

/// The direct regression for the readiness defect: a stimulus that produces
/// bytes and no events must produce no wake. Pause is that stimulus — six
/// bytes, deliberately swallowed.
///
/// It drives the same in-guest reader as [`i8042_keyboard`], and not only for
/// the userland half of the assertion: on a fully idle machine the kernel's
/// log ring flushes one line behind, so the last trace line would never reach
/// the console (filed in `issues/`). A guest polling its handle keeps the ring
/// moving.
///
/// **The zero-event drain is arranged, not hoped for.** What a drain carries is
/// whatever the ISR found in the ring, so a host that injects on a wall clock
/// is asserting on a batching it does not control: a guest that does not drain
/// between the Pause and the key that follows it takes both in one drain, and
/// this test's whole precondition is gone. It also puts more bytes in flight
/// than [`QEMU_PS2_QUEUE`] holds — twenty against sixteen — and the device
/// drops the excess silently, one byte at a time. So each piece goes out only
/// once the guest has reported what the piece before it produced: the Pause is
/// paid by a drain the driver logged, a real key by its two `kev` lines. Six
/// bytes outstanding at most, and a slow guest costs wall clock.
fn i8042_no_spurious_wake(boot: &mut Boot) -> Result<(), String> {
    /// What the guest owes for one injected group before the next goes out.
    enum Owed {
        /// A drain the driver reported. The only thing a swallowed sequence
        /// produces, and therefore the only thing that can pay for one.
        Drain,
        /// `n` `kev` lines: a real key's make and break.
        Keys(usize),
    }

    const SCRIPT: &[(&[(&str, bool)], Owed)] = &[
        (&[("pause", true), ("pause", false)], Owed::Drain),
        (&[("a", true), ("a", false)], Owed::Keys(2)),
        (&[("pause", true), ("pause", false)], Owed::Drain),
        (&[("a", true), ("a", false)], Owed::Keys(2)),
        // The sentinel the guest exits on; see [`send_i8042_sentinel`].
        (&[("end", true), ("end", false)], Owed::Keys(2)),
    ];

    let qemu = &mut boot.qemu;
    let sent = std::cell::Cell::new(0usize);
    let result = {
        let mut input: Option<qemu::QmpInput> = None;
        let mut drains = 0usize;
        let mut keys = 0usize;
        // The counters as they stood when the group still outstanding was sent.
        let mut at_drains = 0usize;
        let mut at_keys = 0usize;
        qemu.run_test_paced(
            "test_rs_i8042_keyboard",
            Duration::from_secs(20),
            |socket, line| {
                if line.contains(I8042_READY) {
                    input = Some(qemu::QmpInput::open(
                        socket.expect("i8042_no_spurious_wake needs BootOptions { qmp }"),
                    ));
                }
                if trace_keys(line).is_some() {
                    drains += 1;
                }
                if line.contains("kev usage=") {
                    keys += 1;
                }
                let Some(input) = input.as_mut() else { return };
                let paid = match SCRIPT.get(sent.get().wrapping_sub(1)) {
                    None => true,
                    Some((_, Owed::Drain)) => drains > at_drains,
                    Some((_, Owed::Keys(n))) => keys >= at_keys + n,
                };
                if !paid {
                    return;
                }
                if let Some((group, _)) = SCRIPT.get(sent.get()) {
                    input.keys(group);
                    at_drains = drains;
                    at_keys = keys;
                    sent.set(sent.get() + 1);
                }
            },
        )
    };
    let sent = sent.get();
    if let Some(err) = &result.error {
        // The guard, not the verdict: the host is waiting on the guest here.
        return Err(format!(
            "{STALLED} {err} — {sent} of {} groups sent when the host gave up waiting for what \
             the last one owed\n{}",
            SCRIPT.len(),
            result.stdout
        ));
    }

    let mut zero_event_drains = 0;
    let mut key_drains = 0;
    for line in result.serial.lines() {
        let Some(keys) = trace_keys(line) else { continue };
        let woke = line.contains("woke_kb=1");
        if keys == 0 {
            zero_event_drains += 1;
            if woke {
                return Err(format!("a drain with no events woke the queue: {line}"));
            }
        } else {
            key_drains += 1;
            if !woke {
                return Err(format!("a drain with events did not wake the queue: {line}"));
            }
        }
    }
    if zero_event_drains == 0 {
        // Not "the stimulus never landed": every Pause above was paid for by a
        // drain before the next injection went out, so one *did* land and one
        // drain did report it. What is left is a drain that took the Pause and
        // produced an event out of it — which is the readiness defect itself.
        return Err(format!(
            "{sent} groups sent, each after the last was reported, and no drain produced zero \
             events — every drain that took a swallowed Pause claimed an event:\n{}",
            result.serial
        ));
    }
    if key_drains == 0 {
        return Err(format!("no drain produced any event:\n{}", result.serial));
    }
    // And the swallowed bytes stayed swallowed all the way out.
    let events = parse_key_events(&result.stdout);
    if events.iter().any(|e| e.usage == 0x48) {
        return Err(format!("Pause reached userland as a key: {events:?}"));
    }
    if !events.iter().any(|e| e.usage == 0x04) {
        return Err(format!("the real key never arrived: {events:?}"));
    }
    eprintln!(
        "  [i8042] {zero_event_drains} zero-event drains, none woke; {key_drains} real ones, all \
         did; {sent} groups, each paid for before the next"
    );
    Ok(())
}

/// QEMU's `PS2_QUEUE_SIZE` (`hw/input/ps2.c`) — what the device will hold. Not
/// the 256-byte `PS2_BUFFER_SIZE` array behind it, which is a migration format
/// and not a capacity.
///
/// **Past it the device drops, silently and one byte at a time.** Measured on
/// QEMU 11.1: twenty-two key transitions in a single `input-send-event` — one
/// QMP command, so the BQL is held for the whole of it and no vCPU can read
/// port 0x60 while it runs — is 26 set-1 bytes, and the guest's driver reported
/// `drain bytes=16` and nothing else, with `0 dropped, 0 overruns, 0 lost
/// edges, 0 discarded`. A key sequence is *not* queued atomically the way a
/// command reply is (`ps2_queue_2`/`_3`/`_4` refuse to split; `ps2_put_keycode`
/// does not), so the hole lands mid-sequence: the run above delivered Left's
/// `0xE0 0x4B` make and lost its `0xE0 0xCB` break. Nothing on the guest side
/// can see this, which is why every injection test here is paced against the
/// guest's own report rather than against a wall clock.
const QEMU_PS2_QUEUE: usize = 16;

/// No group of [`KEYBOARD_SCRIPT`] may outrun the device queue even if every
/// transition in it is an `0xE0`-prefixed two-byte one, which is the widest a
/// non-Pause set-1 transition gets.
const _: () = {
    let mut i = 0;
    while i < KEYBOARD_SCRIPT.len() {
        assert!(
            KEYBOARD_SCRIPT[i].0.len() * 2 <= QEMU_PS2_QUEUE,
            "an i8042_keyboard group can outrun QEMU's PS/2 queue, which drops what it \
             cannot hold one byte at a time and says nothing"
        );
        i += 1;
    }
};

/// A PS/2 pointer packet. Three bytes, because the driver's aux init sends no
/// IntelliMouse knock and QEMU therefore frames a plain mouse.
const MOUSE_PACKET: usize = 3;

/// How far the host may run ahead of the guest while it feeds the framer.
///
/// A packet the guest has reported is a packet whose bytes have left the
/// device's queue, so the lead bounds that queue's occupancy — which is the
/// only thing that makes an injected command a packet. Past the bound QEMU
/// stops queueing motion and starts *accumulating* it, and the merged deltas
/// come back as one packet or, if they cancel, as none at all.
const MOUSE_LEAD: usize = 4;

const _: () = assert!(
    MOUSE_PACKET * MOUSE_LEAD <= QEMU_PS2_QUEUE,
    "the lead outruns QEMU's PS/2 queue, which merges the motion it cannot hold"
);

/// Moves the staged merge puts in one command: more than one, and few enough
/// that their sum stays inside the packet's signed byte.
const MERGE_MOTIONS: usize = 4;

/// The TrackPoint path, and a thousand packets through the framer after it,
/// each sent only once the one before it has come out of the guest.
///
/// The pacing is the design, and [`MOUSE_LEAD`] is what makes it one: a host
/// injecting at its own speed measures how fast the guest drains and reads the
/// shortfall as a driver defect. Staying inside what the device holds leaves no
/// loss to tolerate: every packet injected is a packet that arrived, or the run
/// stalls and says how far it got. It is also what makes the driver's
/// `discarded`/`dropped` counters mean something a slow guest cannot account
/// for.
fn i8042_mouse(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let boot = qemu.boot_log().to_string();
    if !boot.contains("i8042: aux rate=100") {
        return Err(format!("the TrackPoint path never came up:\n{boot}"));
    }

    const BURST: usize = 1000;
    let injected = std::cell::Cell::new(0usize);
    let arrived = std::cell::Cell::new(0usize);
    let result = {
        let mut input: Option<qemu::QmpInput> = None;
        let mut burst = 0usize;
        let mut clicked = false;
        let mut merged = false;
        let mut counted = false;
        let mut ended = false;
        qemu.run_test_paced("test_rs_i8042_mouse", Duration::from_secs(60), |socket, line| {
            if line.contains("===I8042_MOUSE_READY===") {
                let mut open =
                    qemu::QmpInput::open(socket.expect("i8042_mouse needs BootOptions { qmp }"));
                // Off the origin first: the position clamps at 0, so a
                // move up from there would be invisible.
                open.mouse(100, 100, None);
                open.mouse(40, -30, None);
                open.mouse(0, 0, Some(("left", true)));
                open.mouse(0, 0, Some(("left", false)));
                injected.set(4);
                input = Some(open);
            }
            if line.contains("mev buttons=") {
                arrived.set(arrived.get() + 1);
            }
            counted |= clicked && line.contains("discarded");
            let Some(input) = input.as_mut() else { return };
            if ended {
                return;
            }
            // One command per packet, because QEMU syncs input once per
            // command: `BURST` commands is `BURST` packets and three times that
            // many bytes through the framer. Refilling the window on every
            // arrival is what keeps the stream continuous under the pacing.
            while burst < BURST && injected.get() < arrived.get() + MOUSE_LEAD {
                input.mouse(if burst.is_multiple_of(2) { 1 } else { -1 }, 0, None);
                burst += 1;
                injected.set(injected.get() + 1);
            }
            if burst < BURST || arrived.get() < injected.get() {
                return;
            }
            if !clicked {
                input.mouse(0, 0, Some(("left", true)));
                input.mouse(0, 0, Some(("left", false)));
                injected.set(injected.get() + 2);
                clicked = true;
                return;
            }
            // What [`MOUSE_LEAD`] exists to stay clear of, staged where it can
            // do no harm: the queue is empty here, so the merge is the device's
            // one-sync-per-command rule and nothing else.
            if !merged {
                input.mouse_merged(1, MERGE_MOTIONS);
                injected.set(injected.get() + 1);
                merged = true;
                return;
            }
            // The driver reports its counters from a scheduler pass, and the
            // client polling its handle is what keeps passes running: the line has
            // to arrive before the client is told to stop.
            if !counted {
                return;
            }
            // The only right button in the sequence, and the client's signal to
            // exit. It stops on the release, so both halves are printed and the
            // framing assertion still reads a pointer with nothing held down.
            input.mouse(0, 0, Some(("right", true)));
            input.mouse(0, 0, Some(("right", false)));
            injected.set(injected.get() + 2);
            ended = true;
        })
    };
    let (injected, arrived) = (injected.get(), arrived.get());
    if let Some(err) = &result.error {
        // The guard, not the count: the pacing means the host is *waiting* on a
        // packet when this fires, so what it has established is that the run
        // stopped, never that the machine dropped one.
        return Err(format!(
            "{STALLED} {err} — {arrived} of the {injected} packets injected had come back out \
             when the host gave up waiting for the next\n{}",
            result.stdout
        ));
    }

    let events = parse_mouse_events(&result.stdout);
    // The host never had more outstanding than the device holds, so a shortfall
    // is a packet the machine lost and never a host that outran it.
    if events.len() != injected {
        return Err(format!(
            "{} pointer events reached userland out of {injected} packets injected, never more \
             than {MOUSE_LEAD} of them ({} bytes) outstanding against a {QEMU_PS2_QUEUE}-byte \
             device queue",
            events.len(),
            MOUSE_LEAD * MOUSE_PACKET,
        ));
    }
    // The step one packet moves the pointer, off the first two of the burst.
    let step = (events[5].x as i32 - events[4].x as i32).abs();
    // Third from last: the staged merge, then the right button's two halves.
    let merge = events.len() - 3;
    let jump = (events[merge].x as i32 - events[merge - 1].x as i32).abs();
    if step == 0 || jump != step * MERGE_MOTIONS as i32 {
        return Err(format!(
            "{MERGE_MOTIONS} moves in one command moved the pointer {jump} against a one-move \
             step of {step}: QEMU no longer sums motion between syncs, and `MOUSE_LEAD` is \
             derived from the fact that it does"
        ));
    }
    // A sign error in dy is invisible to any test that only checks
    // "it moved", and the PS/2 wire points the opposite way to the
    // screen — so both directions are asserted separately.
    if !events.windows(2).any(|w| w[1].x > w[0].x) {
        return Err("the pointer never moved right".to_string());
    }
    if !events.windows(2).any(|w| w[1].y < w[0].y) {
        return Err(format!(
            "the pointer never moved up — dy inverted? ys: {:?}",
            events.iter().take(8).map(|e| e.y).collect::<Vec<_>>()
        ));
    }
    // PS/2 bit 0 is left, and so is HID boot-mouse bit 0.
    if !events.iter().any(|e| e.buttons == 0x01) {
        return Err(format!(
            "no left-button-down event; buttons seen: {:?}",
            events.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()
        ));
    }
    // And after 3000 bytes of packets the framer is still aligned:
    // the last click is reported as a click, not as motion or as the
    // wrong button.
    let last_press = events.iter().rposition(|e| e.buttons == 0x01);
    let Some(last_press) = last_press else {
        return Err("no button press at all".to_string());
    };
    if events[last_press..].last().map(|e| e.buttons) != Some(0x00) {
        return Err(format!(
            "framing drifted: after the final click the button state is {:?}",
            events.last()
        ));
    }
    // The T14's line, staged. Its log read
    //   `6 bytes, 0 keys, 2 motion, no event from
    //    [aux 0x08, aux 0x06, aux 0x08, aux 0x0e]`
    // on a pointer that was framing perfectly: two whole packets, and
    // the four bytes named were their heads and first body bytes. That
    // sent a field investigation after a desync that had not happened.
    // Three thousand bytes of healthy packets is the same claim with
    // three orders of magnitude more of it: a driver that cannot tell a
    // byte it is holding from a byte it threw away names two thirds of
    // them here.
    let named: Vec<&str> =
        result.serial.lines().filter(|l| l.contains("no event from")).collect();
    if !named.is_empty() {
        return Err(format!(
            "{BURST} clean packets and the driver still named bytes as undecodable:\n{}",
            named.join("\n")
        ));
    }
    // And the counts that say so directly, off the driver's own line. A
    // discard is the byte-level resync and nothing else, so an intact stream
    // owes zero of them — which is what makes any non-zero value on the T14's
    // next boot mean the pointer really did lose the frame. `dropped` is the
    // ring overflowing and `lost edges` an interrupt no pass ever accounted
    // for; [`MOUSE_LEAD`] is what leaves a slow guest unable to produce
    // either.
    let counters = result
        .serial
        .lines()
        .rfind(|l| l.contains("discarded"))
        .ok_or_else(|| format!("the driver never reported its counters:\n{}", result.serial))?;
    for owed in ["0 discarded", "0 overruns", "0 dropped", "0 lost edges"] {
        if !counters.contains(owed) {
            return Err(format!(
                "{injected} packets, none of them sent before the one before it arrived, and the \
                 driver does not report `{owed}`: {counters}"
            ));
        }
    }
    eprintln!("  [i8042] {}", counters.trim());
    eprintln!(
        "  [i8042] {} packets injected, {} out, last button state {:#04x}",
        injected,
        events.len(),
        events.last().unwrap().buttons
    );
    Ok(())
}

/// The compositor's window cap, end to end, on the only config that boots a
/// compositor an in-guest binary can talk to.
///
/// The assertion that matters is not "a refusal arrived" — it is that the
/// number the compositor *derived* from total memory and the screen is the
/// number of windows a client actually gets. A constant on both sides would
/// agree with itself forever; this fails if the derivation and the enforcement
/// ever drift apart.
///
/// Runs before the two clients that abuse the compositor, because a cap is
/// only countable from a desktop with every window still free.
fn metal_sim_window_caps(boot: &mut Boot) -> Result<(), String> {
    // The compositor announces what it derived. Read rather than
    // recomputed here: recomputing it would copy the formula into the
    // test and stop asking whether the compositor uses it. Off the group's
    // console, because the compositor says it once and an earlier member of
    // the group has already drained the line off the wire.
    let _ = await_marker(&mut boot.qemu, &mut boot.console, "compositor: at most ", "the window cap");
    let Some(declared) = boot
        .console
        .lines()
        .find_map(|l| l.split("compositor: at most ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return Err(format!(
            "the compositor never said how many windows it would hold:\n{}",
            boot.console
        ));
    };
    if declared == 0 {
        return Err("the compositor derived a cap of zero windows".to_string());
    }

    let result = boot.qemu.run_test("test_rs_window_caps", Duration::from_secs(120));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "window_caps exited {:?}:\n{}",
            result.exit_code, result.stdout
        ));
    }

    let Some(granted) = result
        .stdout
        .split("oversized refused, ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return Err(format!("window_caps printed no count:\n{}", result.stdout));
    };
    if granted != declared {
        return Err(format!(
            "the compositor declared a cap of {declared} windows and granted \
             {granted} — the derivation and the enforcement disagree:\n{}",
            result.stdout
        ));
    }
    eprintln!("  [metal-sim] compositor cap {declared} windows, {granted} granted then refused");
    Ok(())
}

/// A client that lies about its frame lengths.
///
/// The guest binary carries the assertions — it is the only side that can see
/// whether the compositor closed the connection it ruled on — so the host's
/// job is to boot it and to insist the count it reports is the whole case
/// list. A guest that skipped cases would otherwise exit 0 having proved
/// nothing.
fn metal_sim_ipc_hostile_peer(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let result = qemu.run_test("test_rs_ipc_hostile_peer", Duration::from_secs(120));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "ipc_hostile_peer exited {:?}:\n{}",
            result.exit_code, result.stdout
        ));
    }
    let Some(refused) = result
        .stdout
        .split("hostile peer: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return Err(format!(
            "ipc_hostile_peer printed no count:\n{}",
            result.stdout
        ));
    };
    // The guest's own case list, restated here so a case deleted on
    // one side is a red run rather than a quieter test.
    const CASES: usize = 3;
    if refused != CASES {
        return Err(format!(
            "the compositor refused {refused} malformed frames, not {CASES}:\n{}",
            result.stdout
        ));
    }
    eprintln!("  [metal-sim] {refused} malformed frames refused, compositor still serving");
    Ok(())
}

/// A client that stops talking, stops listening, or never stops.
///
/// The guest carries the "is it still answering" half, because only it can put
/// a deadline on the answer; the host carries the half the guest cannot see —
/// whether the desktop is still *painting*, and whether every client the
/// compositor got rid of was named.
///
/// The two halves are not redundant. A compositor parked on one client answers
/// nobody, so the guest catches that; a compositor livelocked on one client
/// answers everybody and draws nothing, which only the frame counter shows.
///
/// Last in its group: it is the one that abuses the compositor hardest, and
/// its own final assertion is that the desktop is still compositing after it.
fn metal_sim_compositor_stall(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let result = qemu.run_test("test_rs_compositor_stall", Duration::from_secs(240));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "compositor_stall exited {:?}:\n{}",
            result.exit_code, result.stdout
        ));
    }
    // The guest's own case list, restated here so a case deleted on
    // one side is a red run rather than a quieter test.
    const CASES: usize = 6;
    if !result
        .stdout
        .contains(&format!("compositor stall: {CASES} stalls survived"))
    {
        return Err(format!(
            "the guest did not report {CASES} survived stalls:\n{}",
            result.stdout
        ));
    }

    let frames = |text: &str| text.matches("compositor: frames=").count();

    // Starvation, which is the one shape the guest cannot see. Between
    // these two markers one window is sending on every pass; a drain
    // loop that ends only when nothing is ready never gets to `redraw`
    // and this window holds zero frames.
    let Some(stream) = result
        .stdout
        .split("compositor stall: stream start")
        .nth(1)
        .and_then(|rest| rest.split("compositor stall: stream end").next())
    else {
        return Err(format!(
            "the guest never bracketed its streaming window:\n{}",
            result.stdout
        ));
    };
    if frames(stream) == 0 {
        return Err(format!(
            "the compositor composited nothing while one client streamed:\n{stream}"
        ));
    }

    // Dropped by name, never silently. Three connections never finish
    // a first frame, and one window stops reading its mail.
    const TIMED_OUT: &str = "it never finished its first message";
    let timed_out = result.stdout.matches(TIMED_OUT).count();
    if timed_out < 3 {
        return Err(format!(
            "three connections went quiet mid-handshake and {timed_out} were named:\n{}",
            result.stdout
        ));
    }
    const NOT_READING: &str = "it is not reading";
    if !result.stdout.contains(NOT_READING) {
        return Err(format!(
            "a window stopped reading and the compositor never said so:\n{}",
            result.stdout
        ));
    }

    // And it is still painting once every stall is behind it, on a
    // capture that starts empty — so this counts frames the compositor
    // produced *after* the last case, not frames it produced before
    // the first. Its reporting interval is 2 s.
    //
    // **Two batches, not twenty seconds.** The count is the verdict; what
    // ended the wait was a flat 20 s of host clock, which at width 12 asks
    // a compositor with a twelfth of the machine for ten intervals' work
    // in one.
    let mut after = String::new();
    if let Err(why) =
        await_guest(qemu, &mut after, "two more frame batches", |seen| frames(seen) >= 2)
    {
        return Err(format!(
            "{why}\nthe compositor reported {} frame batches after the last stall:\n{after}",
            frames(&after)
        ));
    }

    let console = format!("{}\n{after}", result.serial);
    serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
    eprintln!(
        "  [metal-sim] {CASES} stalls survived, {timed_out} handshakes timed out by name, \
         desktop still compositing"
    );
    Ok(())
}

/// A client that dies, or asks for something the kernel refuses on its behalf,
/// must cost the compositor that client and nothing else.
///
/// The guest half runs the cases and probes after each; this half asserts what
/// the guest cannot see — that the desktop is still painting, and that the
/// clients dropped along the way were named. A compositor that panics fails
/// this at the probe, at the frame count and at the console check, which is
/// what it should do.
fn metal_sim_client_death(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let result = qemu.run_test("test_rs_compositor_client_death", Duration::from_secs(240));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "compositor_client_death exited {:?}:\n{}",
            result.exit_code, result.stdout
        ));
    }
    // The guest's own case list, restated here so a case deleted on one side
    // is a red run rather than a quieter test.
    const CASES: usize = 5;
    if !result
        .stdout
        .contains(&format!("compositor client death: {CASES} deaths survived"))
    {
        return Err(format!(
            "the guest did not report {CASES} survived deaths:\n{}",
            result.stdout
        ));
    }

    // Non-vacuity, and the case that motivated the whole run: the compositor
    // has to have met a request whose creator the kernel no longer knows. The
    // guest orders that by construction — reap, then release the process
    // holding the socket — so a run without this line is a defect and never a
    // lost race.
    //
    // **The line is the compositor serving that request, where it used to be
    // the compositor saying the process had exited.** The grant that killed the
    // desktop named a pid; a buffer is a handle now and the connection is what
    // it travels over, so a reaped creator costs its heir nothing and the
    // refusal this once asserted on cannot happen.
    const VANISHED: &str = "a reaped creator's connection still got a window";
    if !result.stdout.contains(VANISHED) {
        return Err(format!(
            "the compositor never served a request from a reaped creator, so this run says \
             nothing about what replaced the grant:\n{}",
            result.stdout
        ));
    }

    // Still painting once every case is behind it, on a capture that starts
    // empty — so this counts frames produced *after* the last case. The
    // compositor's reporting interval is 2 s.
    // The count is the verdict and the wait is a guard, as in
    // `metal_sim_compositor_stall`.
    let frames = |text: &str| text.matches("compositor: frames=").count();
    let mut after = String::new();
    if let Err(why) =
        await_guest(qemu, &mut after, "two more frame batches", |seen| frames(seen) >= 2)
    {
        return Err(format!(
            "{why}\nthe compositor reported {} frame batches after the last client died:\n{after}",
            frames(&after)
        ));
    }

    let console = format!("{}\n{after}", result.serial);
    serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
    eprintln!(
        "  [metal-sim] {CASES} client deaths survived, a reaped creator's request served \
         anyway, desktop still compositing"
    );
    Ok(())
}

/// Run one machine-shape test. Like `run_screen_test`, each of these owns its
/// QEMU — the machine shape *is* the test — except for the runs of adjacent
/// names that share one through `held` (see [`group_boot`]).
fn run_machine_test(
    name: &str,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    held: &mut Grouped,
) -> Result<(), String> {
    // **Only one QEMU may be up at a time in this process.** Every instance
    // shares one QMP socket path and one `test-bootable.img` under the pid's
    // temp dir, so a guest still running when the next one starts takes that
    // one's socket and it exits before its first line — which is what every
    // test after a group reported the first time a group outlived its members.
    // (It is also what parallel boots in this process would have to fix
    // first.)
    if group_of(name) != held.as_ref().map(|up| up.group) {
        *held = None;
    }
    match name {
        // Body in `tests/common/storage.rs`, so the hunk in this shared file
        // stays one line.
        "foreign_disk_untouched" => storage::foreign_disk_untouched(test_config, c_bins, rust_bins),
        // Body in `tests/common/gpt.rs`, same reason.
        "boot_partition_identity" => common::gpt::boot_partition_identity(test_config, c_bins, rust_bins),
        // Bodies in `tests/common/usb.rs`, for the same reason.
        "usb_storage_gate" => usb::usb_storage_gate(test_config, c_bins, rust_bins),
        "usb_storage_shapes" => usb::usb_storage_shapes(test_config, c_bins, rust_bins),
        "usb_boot_stick_pulled" => usb::usb_boot_stick_pulled(test_config, c_bins, rust_bins),
        "usb_refused_disk_first" => {
            usb::usb_refused_disk_first(test_config, c_bins, rust_bins)
        }
        "usb_pool_exhausted" => usb::usb_pool_exhausted(test_config, c_bins, rust_bins),
        "usb_short_read" => usb::usb_short_read(test_config, c_bins, rust_bins),
        "usb_disk_index_stable" => usb::usb_disk_index_stable(test_config, c_bins, rust_bins),
        // Body in `tests/common/volumes.rs`, same reason.
        "esp_filesystem" => common::volumes::esp_filesystem(test_config, c_bins, rust_bins),
        // Body in `tests/common/toybox.rs`, same reason.
        "toybox_cp_volume" => common::toybox::cp_volume(test_config, c_bins, rust_bins),
        "kernel_log_file" => common::volumes::kernel_log_file(test_config, c_bins, rust_bins),
        "kernel_heartbeat" => {
            // The instrument for a machine whose log cannot say whether it was
            // alive: ten of the owner's boots are byte-identical between the
            // ones that froze and the ones that did not. The gate has to prove
            // three things a `must_say` cannot — that the lines *keep coming*,
            // that no CPU drops out of the mask on a machine with nothing to
            // do, and that no window between two lines is wide enough to hide a
            // death.
            //
            // The second is why this asserts a *constant full* mask where the
            // old gate asserted a *varying* one — and the old gate's assertion
            // was satisfied by the defect, so it certified it. Same guest with
            // the tick removed: 10 of 11 lines below `alive=8/8`, six of them at
            // `alive=2/8`, 56 lines naming a silent CPU and one silent for
            // 2.811 s. Every line is `8/8` with the tick.
            //
            // The gap bound below is the one with no demonstrated teeth here:
            // without the tick the widest gap was still 0.260 s, because QEMU's
            // devices keep waking *someone* even when they wake nobody in
            // particular. It is carried for the metal log, where the same code
            // left gaps of 14 s to 102 s.
            //
            // **What none of it establishes**: a QEMU guest is never as quiet as
            // the owner's laptop. This proves the tick arms, fires and re-arms,
            // and that the instrument reads a full mask when nothing is wrong.
            // It cannot prove the T14's LAPIC keeps counting through whatever
            // its firmware does with a halted core.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                smp: 8,
                kernel_params: &["heartbeat"],
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let mut log = qemu.boot_log().to_string();
            log.push_str(&qemu.drain_serial(Duration::from_secs(3)));

            // **A heartbeat and the `i8042: line` under it are one reading**,
            // and `heartbeat::poll` emits them as two `log!`s — so a capture can
            // end between them. Run `31273373928` on `main` did: twelve beats,
            // eleven pin readings, and the last beat was the last line of the
            // log. Counting the two kinds against each other reads that as a pin
            // whose state was unreadable, which is the one thing this pairing
            // exists to detect. So the unit is the pair, and a beat with nothing
            // after it at all is a reading this capture does not hold.
            let captured: Vec<&str> = log.lines().collect();
            let at: Vec<usize> = captured
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains("heartbeat: t="))
                .map(|(i, _)| i)
                .collect();
            let torn = at.last().is_some_and(|&i| i + 1 == captured.len());
            let at = &at[..at.len() - usize::from(torn)];
            let beats: Vec<&str> = at.iter().map(|&i| captured[i]).collect();
            // Three seconds of drain at a 250 ms period is twelve; a guest that
            // spends some of it booting produces fewer. Four is "the machine
            // kept saying it was alive" with room, and zero or one is the
            // failure this exists to make impossible.
            if beats.len() < 4 {
                return Err(format!(
                    "{} whole heartbeat line(s) in ~3 s at a 250 ms period — the instrument does \
                     not keep reporting, so a log that stops says nothing\n{log}",
                    beats.len()
                ));
            }
            // Each pair, positionally: `report_line` is the statement after the
            // heartbeat's `log!`, and another CPU's line may land between the
            // two commits, so what is asserted is one pin reading before the
            // next beat rather than the line immediately below.
            let pin_in = |from: usize, to: usize| {
                captured[from..to].iter().filter(|l| l.contains("i8042: line ")).count()
            };
            let unpaired: Vec<String> = at
                .iter()
                .enumerate()
                .filter(|&(n, &i)| pin_in(i + 1, at.get(n + 1).copied().unwrap_or(captured.len())) != 1)
                .map(|(_, &i)| captured[i].to_string())
                .collect();
            if !unpaired.is_empty() {
                return Err(format!(
                    "{} of {} heartbeats carry no `i8042: line` of their own — the pin's state is \
                     what separates a machine with nothing to do from one whose input died, and it \
                     has to be readable at every heartbeat or the pairing is a guess\n{}\n{log}",
                    unpaired.len(),
                    beats.len(),
                    unpaired.iter().take(4).cloned().collect::<Vec<_>>().join("\n"),
                ));
            }
            // **What a clear bit has to mean is "that CPU stopped", and what
            // makes that readable is that it does not come back.** `diag-tick`
            // caps a sleep at 100 ms against a 250 ms line, so a healthy CPU
            // contributes two or three passes to every one; a CPU absent from
            // two consecutive lines has missed five wakes and is the shape the
            // T14 produces — the mask thins CPU by CPU and stays thin, 56 lines
            // naming a silent CPU and one silent for 2.811 s. A single line a
            // CPU is missing from and is back on is the other thing entirely,
            // and this guest has eight vCPUs on a runner's four cores: run
            // `31283095698` rep 2 reported `cpu6 last reached one 0.349s ago`
            // once, between eight lines of `8/8` either side of it. That is the
            // host declining to run a halted thread, which no instrument in this
            // guest claims anything about.
            //
            // **The boot is excluded for the same reason and needs a second
            // rule.** `boot_with_options` returns on this config's ready marker
            // with userland still spawning, and eight vCPUs on four cores do not
            // all run while the guest is busy — run `31280428519` shard 5 had
            // `alive=7/8` at 1.373 s with `cpu1 has never reached a scheduler
            // pass` and `5/8` at 1.624 s, and run `31283095698` rep 2 had cpu0
            // missing from two consecutive lines. So the window opens at the
            // first full mask.
            //
            // Neither rule lets the tick-less control through, and that was run
            // rather than argued: `heartbeat = []` in `kernel/Cargo.toml` reds
            // this on six of the eight CPUs.
            let Some(settled) = beats.iter().position(|l| l.contains("alive=8/8")) else {
                return Err(format!(
                    "no heartbeat in the whole capture reported every CPU alive, so the machine \
                     never reached the state the mask is a claim about\n{log}"
                ));
            };
            let quiet = &beats[settled..];
            if quiet.len() < 4 {
                return Err(format!(
                    "the mask was full for only the last {} of {} heartbeats — the machine did not \
                     settle inside this capture, and a clear bit before it settles says nothing\n\
                     {log}",
                    quiet.len(),
                    beats.len()
                ));
            }
            const CPUS: usize = 8;
            let masks: Vec<u64> = quiet
                .iter()
                .filter_map(|l| l.split("mask=0x").nth(1))
                .filter_map(|m| u64::from_str_radix(m.split_whitespace().next()?, 16).ok())
                .collect();
            if masks.len() != quiet.len() {
                return Err(format!(
                    "{} of {} heartbeats carry no readable mask=0x — the field that says which CPU \
                     stopped\n{log}",
                    quiet.len() - masks.len(),
                    quiet.len()
                ));
            }
            let absent = |c: usize, m: &u64| m & (1 << c) == 0;
            let stopped: Vec<usize> = (0..CPUS)
                .filter(|&c| masks.windows(2).any(|w| absent(c, &w[0]) && absent(c, &w[1])))
                .collect();
            let named: Vec<&str> = captured[at[settled]..]
                .iter()
                .filter(|l| l.contains("heartbeat: cpu"))
                .copied()
                .collect();
            if !stopped.is_empty() {
                return Err(format!(
                    "cpu{stopped:?} missing from two consecutive heartbeats of {}, on a settled \
                     guest where every CPU is healthy — a CPU that misses two lines has missed \
                     five `diag-tick` wakes, so a clear bit does not mean that CPU stopped, which \
                     is the whole of the field\n{}\n{}\n{log}",
                    quiet.len(),
                    quiet.join("\n"),
                    named.iter().take(8).cloned().collect::<Vec<_>>().join("\n"),
                ));
            }
            let blips = masks.iter().filter(|m| **m != (1 << CPUS) - 1).count();
            // And no window between two lines may be wide enough to hide a
            // death. The metal boots this exists for went quiet for between 14 s
            // and 102 s; four times the period is far below any of them and far
            // above anything a loaded host does to a 250 ms cadence.
            const MAX_GAP_S: f64 = 1.0;
            let gaps: Vec<f64> = beats
                .iter()
                .filter_map(|l| l.split("gap=").nth(1))
                .filter_map(|g| g.trim_end_matches('s').split_whitespace().next()?.parse().ok())
                .collect();
            if gaps.len() != beats.len() {
                return Err(format!(
                    "{} of {} heartbeats carry no readable gap= — the field a reader uses to tell \
                     a machine that went quiet from one that died\n{log}",
                    beats.len() - gaps.len(),
                    beats.len()
                ));
            }
            let worst = gaps.iter().copied().fold(0.0f64, f64::max);
            if worst > MAX_GAP_S {
                return Err(format!(
                    "the widest window between two heartbeats was {worst:.3}s against a 250 ms \
                     period — the machine stopped reporting for long enough to have died in\n{log}"
                ));
            }
            // `ran=` has to be a reading too, and its failure mode is the
            // opposite of the mask's: a counter that never moves reports a
            // machine that schedules and runs nothing, which is the second
            // freeze signature and the one `alive=` cannot carry. This guest
            // composites, so some window must be nonzero.
            let rans: Vec<u64> = beats
                .iter()
                .filter_map(|l| l.split("ran=").nth(1))
                .filter_map(|r| r.split_whitespace().next()?.parse().ok())
                .collect();
            let moved = rans.iter().filter(|&&r| r > 0).count();
            if rans.len() != beats.len() || moved == 0 {
                return Err(format!(
                    "{} of {} heartbeats carry a readable ran= and {moved} of them are nonzero — \
                     a counter that never moves cannot tell a machine that stopped scheduling \
                     from one that schedules and runs nothing\n{log}",
                    rans.len(),
                    beats.len(),
                ));
            }
            // And the clock in the line advances, or the timestamp cannot
            // localise a death.
            let stamps: Vec<&str> = beats
                .iter()
                .filter_map(|l| l.split("heartbeat: t=").nth(1))
                .map(|s| s.split_whitespace().next().unwrap_or(""))
                .collect();
            if stamps.first() == stamps.last() {
                return Err(format!(
                    "every heartbeat carries the same timestamp {:?}\n{log}",
                    stamps.first()
                ));
            }
            // The line beside every heartbeat, and the reason it is there: the
            // T14's freeze reads `alive=8/8 ran=0` under both live hypotheses,
            // and only the pin's own state separates them. What has teeth here
            // is not that the line exists but that its vector is *read back off
            // the chip* and matches the one `init` said it programmed — a probe
            // printing a literal, or reading the wrong register, disagrees.
            let armed = log
                .lines()
                .find_map(|l| l.split("scanning on, GSI ").nth(1))
                .map(|s| s.to_string());
            let Some(armed) = armed else {
                return Err(format!(
                    "no i8042 arming line on a Profile::Metal guest, so nothing says which GSI or \
                     vector the probe below should have read\n{log}"
                ));
            };
            // `1 -> vec 0x24 apic 0 on`
            let mut fields = armed.split_whitespace();
            let kbd_gsi = fields.next().unwrap_or("").to_string();
            let vector = fields.nth(2).unwrap_or("").trim_start_matches("0x").to_string();
            let lines: Vec<&str> = log.lines().filter(|l| l.contains("i8042: line ")).collect();
            // `rte=0x0000000000000024`: the vector is the low byte, bit 16 is
            // the mask. Both are the chip's answer, not ours.
            let healthy = |l: &str| {
                l.split("rte=0x")
                    .skip(1)
                    .map(|e| e.split_whitespace().next().unwrap_or(""))
                    .all(|e| {
                        u64::from_str_radix(e, 16).is_ok_and(|entry| {
                            entry & 0xFF == u64::from_str_radix(&vector, 16).unwrap_or(0)
                                && entry & (1 << 16) == 0
                        })
                    })
            };
            let wrong: Vec<&&str> = lines.iter().filter(|l| !healthy(l)).collect();
            if !wrong.is_empty() || !lines.iter().all(|l| l.contains(&format!("kbd gsi={kbd_gsi} "))) {
                return Err(format!(
                    "{} of {} `i8042: line` readings carry an entry that is masked, or whose vector \
                     is not the {vector} `init` programmed on GSI {kbd_gsi} — the probe is not \
                     reading the chip\n{}\n{log}",
                    wrong.len(),
                    lines.len(),
                    wrong.iter().take(4).map(|l| l.to_string()).collect::<Vec<_>>().join("\n"),
                ));
            }
            // OBF stuck is the state the probe exists to name, so a healthy
            // guest must never show it — otherwise the reading is noise and a
            // metal log carrying it proves nothing.
            let obf: Vec<&&str> = lines
                .iter()
                .filter(|l| {
                    l.split("status=0x")
                        .nth(1)
                        .and_then(|s| u8::from_str_radix(s.split_whitespace().next()?, 16).ok())
                        .is_some_and(|s| s & 1 != 0)
                })
                .collect();
            if !obf.is_empty() {
                return Err(format!(
                    "{} of {} `i8042: line` readings found the output buffer full on a guest whose \
                     input is healthy — a set bit there is meant to mean the controller is holding \
                     a byte no ISR will ever read\n{}\n{log}",
                    obf.len(),
                    lines.len(),
                    obf.iter().take(4).map(|l| l.to_string()).collect::<Vec<_>>().join("\n"),
                ));
            }
            eprintln!(
                "  [heartbeat] {} whole lines in ~3 s, each with its own pin reading, {settled} \
                 before the machine settled and {} after, {blips} of those missing a CPU for one \
                 line and none for two, {moved} with ran>0, widest gap {worst:.3}s, t={} → t={}; \
                 {} i8042 line reading(s), vec 0x{vector} on gsi {kbd_gsi}, none masked, none with \
                 OBF set",
                beats.len(),
                quiet.len(),
                stamps.first().unwrap_or(&"?"),
                stamps.last().unwrap_or(&"?"),
                lines.len(),
            );
            Ok(())
        }
        // Body in `tests/common/wallclock.rs`, same reason.
        "wall_clock_file" => common::wallclock::wall_clock_file(test_config, c_bins, rust_bins),
        "wall_clock_refusals" => {
            common::wallclock::wall_clock_refusals(test_config, c_bins, rust_bins)
        }
        "late_storage_connect" => common::volumes::late_storage_connect(test_config, c_bins, rust_bins),
        "log_partition_layout" => {
            common::volumes::log_partition_layout(test_config, c_bins, rust_bins)
        }
        "log_partition_identity" => {
            common::volumes::log_partition_identity(test_config, c_bins, rust_bins)
        }
        "log_backing_read_error" => {
            common::volumes::log_backing_read_error(test_config, c_bins, rust_bins)
        }
        "boot_volume_metadata_error" => {
            common::volumes::boot_volume_metadata_error(test_config, c_bins, rust_bins)
        }
        "usb_storage_write_error" => usb::usb_storage_write_error(test_config, c_bins, rust_bins),
        "usb_flush_optional" => usb::usb_flush_optional(test_config, c_bins, rust_bins),
        "xhci_deaf_registers" => usb::xhci_deaf_registers(test_config, c_bins, rust_bins),
        "xhci_slow_connect" => usb::xhci_slow_connect(test_config, c_bins, rust_bins),
        "xhci_portsc_rw1c" => usb::xhci_portsc_rw1c(test_config, c_bins, rust_bins),
        "usb_transport_break" => usb::usb_transport_break(test_config, c_bins, rust_bins),
        "xhci_full_speed_device" => {
            usb::xhci_full_speed_device(test_config, c_bins, rust_bins)
        }
        "xhci_superspeed_ports" => usb::xhci_superspeed_ports(test_config, c_bins, rust_bins),
        "xhci_hotplug" => usb::xhci_hotplug(test_config, c_bins, rust_bins),
        "xhci_flap" => usb::xhci_flap(test_config, c_bins, rust_bins),
        "xhci_hid_break" => usb::xhci_hid_break(test_config, c_bins, rust_bins),
        // Body in `tests/common/iommu.rs`, same reason.
        "iommu_discovery" => common::iommu::iommu_discovery(test_config, c_bins, rust_bins),
        // Body in `tests/common/logread.rs`, so the hunk here stays one line.
        "log_conservation_smp1" => {
            common::logread::log_conservation_smp1(test_config, c_bins, rust_bins)
        }
        "log_conservation_smp4" => {
            common::logread::log_conservation_smp4(test_config, c_bins, rust_bins)
        }
        "log_conservation_smp8" => {
            common::logread::log_conservation_smp8(test_config, c_bins, rust_bins)
        }
        "log_nested_emit" => common::logread::log_nested_emit(test_config, c_bins, rust_bins),
        "log_reserve_window" => {
            common::logread::log_reserve_window(test_config, c_bins, rust_bins)
        }
        "log_reserve_window_negative" => {
            common::logread::log_reserve_window_negative(test_config, c_bins, rust_bins)
        }
        "log_poll_outlives_a_close" => {
            common::logread::log_poll_outlives_a_close(test_config, c_bins, rust_bins)
        }
        // Body in `tests/common/console.rs`, same reason.
        "c_capture_ignores_daemon_lines" => {
            common::console::c_capture_ignores_daemon_lines(test_config, c_bins, rust_bins)
        }
        "console_line_atomicity" => {
            common::console::console_line_atomicity(test_config, c_bins, rust_bins)
        }
        "keyboard_claim_close_spares_stdin" => {
            common::console::keyboard_claim_close_spares_stdin(test_config, c_bins, rust_bins)
        }
        "iommu_context_absent" => common::iommu::iommu_context_absent(test_config, c_bins, rust_bins),
        "iommu_empty_domain" => common::iommu::iommu_empty_domain(test_config, c_bins, rust_bins),
        // Body in `tests/common/hda.rs`, same reason.
        "hda_tone" => common::hda::hda_tone(test_config, c_bins, rust_bins),
        "hda_client_stall" => common::hda::hda_client_stall(test_config, c_bins, rust_bins),
        "hda_two_live_refused" => {
            common::hda::hda_two_live_refused(test_config, c_bins, rust_bins)
        }
        "double_fault_stack" => faults::double_fault_stack(test_config, c_bins, rust_bins),
        "syscall_window_nmi" => faults::syscall_window_nmi(test_config, c_bins, rust_bins),
        "idle_stack_guard" => faults::idle_stack_guard(test_config, c_bins, rust_bins),
        "dump_nmi_probe" => faults::dump_nmi_probe(test_config, c_bins, rust_bins),
        "diskless_boot" => faults::diskless_boot(test_config, c_bins, rust_bins),
        "virtio_net_no_msix" => faults::virtio_net_no_msix(),
        // Body in `tests/common/audio.rs`, so the hunk here stays one line.
        "metal_sim_null_audio" => audio::null_sink_real_rate(test_config, c_bins, rust_bins),
        "null_sink_shipped_client" => audio::null_sink_shipped_client(test_config, c_bins, rust_bins),
        "doom_sound_flood" => audio::doom_sound_flood(rust_bins),
        "doom_music" => doom_music(rust_bins),
        "metal_sim_compositor" => {
            metal_sim_compositor(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "metal_sim_scanout_wc" => {
            metal_sim_scanout_wc(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "metal_sim_window_caps" => {
            metal_sim_window_caps(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "metal_sim_ipc_hostile_peer" => {
            metal_sim_ipc_hostile_peer(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "metal_sim_compositor_stall" => {
            metal_sim_compositor_stall(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "metal_sim_client_death" => {
            metal_sim_client_death(group_boot(held, METAL_SIM_DESKTOP, || {
                boot_metal_sim_desktop(rust_bins)
            }))
        }
        "i8042_keyboard" => i8042_keyboard(group_boot(held, I8042_TRACE, || {
            boot_i8042_trace(test_config, c_bins, rust_bins)
        })),
        "i8042_no_spurious_wake" => i8042_no_spurious_wake(group_boot(held, I8042_TRACE, || {
            boot_i8042_trace(test_config, c_bins, rust_bins)
        })),
        "i8042_mouse" => i8042_mouse(group_boot(held, I8042_TRACE, || {
            boot_i8042_trace(test_config, c_bins, rust_bins)
        })),
        "swiss_german_layout" => {
            swiss_german_layout(&mut boot_locale(test_config, c_bins, rust_bins))
        }
        "locale_detect" => locale_detect(&mut boot_locale(test_config, c_bins, rust_bins)),
        "locale_detect_unrecognized" => {
            locale_detect_unrecognized(&mut boot_locale(test_config, c_bins, rust_bins))
        }
        "console_locale_detect" => console_locale_detect(),
        "desktop_locale_detect" => desktop_locale_detect(),
        "desktop_typing_damage" => desktop_typing_damage(),
        "desktop_window_child" => desktop_window_child(rust_bins),
        "desktop_audio_client" => desktop_audio_client(),
        "blocked_dump" => blocked_dump(),
        "xhci_many_devices" => {
            // The T14's internal controller carries a camera, Bluetooth and a
            // fingerprint reader next to the boot stick, and every profile in
            // this tree had at most three devices on the bus — so no test
            // could see a driver that stopped at three, and no test could see
            // two devices of one class landing on one interrupt ring.
            let options = BootOptions {
                profile: qemu::Profile::MetalUsb,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            let usb = usb_argv(&argv);
            // The profile's claim is about the bus, so it is checked against
            // argv: a console line cannot distinguish "the driver bound one
            // keyboard" from "only one keyboard was ever attached".
            if usb.len() < 4 {
                return Err(format!("this profile needs more USB devices than {usb:?}"));
            }
            if usb.iter().filter(|d| d.starts_with("usb-kbd")).count() < 2 {
                return Err(format!("two keyboards are the point; argv has {usb:?}"));
            }
            if !usb.iter().any(|d| d.starts_with("usb-storage")) {
                return Err(format!("no non-HID device on the bus: {usb:?}"));
            }

            let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let log = qemu.boot_log().to_string();

            // Where the block count came from, which is the thing this work
            // exists to protect and the thing no count of devices or rings can
            // see: a fixed cap of any value at or above the size of this bus
            // leaves every other assertion here green.
            let Some(dma) = parse_xhci_layout(&log) else {
                return Err(format!("the driver printed no DMA layout line:\n{log}"));
            };
            let room = dma.pool_kib * 1024 / dma.stride;
            if room <= dma.blocks {
                return Err(format!(
                    "the pool holds {room} blocks of {} B and the driver claimed {}: {dma:?}",
                    dma.stride, dma.blocks
                ));
            }
            // The pool has room for four times what this controller can
            // address, so the slot count is the binding term of the two and
            // the block count has to be it exactly. (A cap that happened to
            // equal 64 would still pass — no QEMU controller can tell those
            // apart. Every other constant cannot.)
            if dma.blocks != dma.cap_slots {
                return Err(format!(
                    "device blocks={} with max_slots={} and room for {room} — the block count \
                     is not the controller's slot count:\n{log}",
                    dma.blocks, dma.cap_slots
                ));
            }
            // And it fit in the single 2 MiB page DmaPool was going to hand
            // out for the head regardless, which is the whole cost argument.
            if dma.pool_kib != 2048 {
                return Err(format!(
                    "the pool is {} KiB, not the one 2 MiB page the head already forces: {dma:?}",
                    dma.pool_kib
                ));
            }

            // One slot per device on the bus, non-HID included: the driver
            // enables a slot before it can know what the device is.
            let slots = parse_xhci_slots(&log);
            if slots.len() != usb.len() {
                return Err(format!(
                    "{} devices on the bus, {} slots enabled ({slots:?}):\n{log}",
                    usb.len(),
                    slots.len()
                ));
            }
            let mut distinct = slots.clone();
            distinct.sort_unstable();
            distinct.dedup();
            if distinct.len() != slots.len() {
                return Err(format!("a slot id came back twice: {slots:?}"));
            }

            // Each HID on its own interrupt ring and its own report buffer.
            // Two keyboards sharing a ring is the defect this asserts against,
            // and it is silent from every other angle.
            let binds = parse_xhci_binds(&log);
            let keyboards = binds.iter().filter(|b| b.kind == "keyboard").count();
            if keyboards != 2 {
                return Err(format!("{keyboards} keyboards bound, want 2: {binds:?}\n{log}"));
            }
            if binds.len() < 4 {
                return Err(format!("only {} HID devices bound: {binds:?}\n{log}", binds.len()));
            }
            let mut rings: Vec<usize> = binds.iter().map(|b| b.int_ring).collect();
            rings.sort_unstable();
            rings.dedup();
            if rings.len() != binds.len() {
                return Err(format!(
                    "{} devices share {} interrupt rings: {binds:?}",
                    binds.len(),
                    rings.len()
                ));
            }
            // And every device on the bus is accounted for exactly once: the
            // HIDs bound above, the boot stick bound as a disk, the hub walked
            // past. An inequality here would let a driver that bound the stick
            // *and* skipped it, or that stopped enumerating early, pass.
            let disks = log.matches("usb-storage: disk ").count();
            let skipped = log.matches("no HID boot interface found").count();
            if binds.len() + disks + skipped != usb.len() {
                return Err(format!(
                    "{} HID + {disks} disk + {skipped} skipped is not the {} devices on the bus:\n{log}",
                    binds.len(),
                    usb.len()
                ));
            }
            if disks != 1 {
                return Err(format!("{disks} disks bound, want the boot stick:\n{log}"));
            }
            serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
            eprintln!(
                "  [xhci] {} devices, {} slots, {keyboards} keyboards on {} distinct rings, \
                 {disks} disk; {} blocks of {} B for max_slots={}, scratchpad={}, pool {} KiB",
                usb.len(),
                slots.len(),
                rings.len(),
                dma.blocks,
                dma.stride,
                dma.cap_slots,
                dma.scratchpad,
                dma.pool_kib
            );
            Ok(())
        }
        "xhci_second_controller" => {
            // The T14's shape, and the defect that shape found. Tiger Lake has
            // two xHCI controllers — the Thunderbolt block's at 00:0d.0 and the
            // PCH's at 00:14.0, identical in class, subclass and prog_if — and
            // the laptop's own ports hang off the second. The kernel took the
            // first PCI match, so a real boot logged one `xHCI: found at PCI
            // 00:0d.0` and then `no HID devices found` on a machine whose
            // keyboard was one bus over. Every profile in this tree had exactly
            // one controller, so nothing could see it.
            let options = BootOptions {
                profile: qemu::Profile::MetalXhciSecond,
                qmp: true,
                // Nothing else on this machine may be able to deliver a
                // keystroke. With the i8042 on, a kernel that never found the
                // second controller could still be handed the key by QEMU's
                // PS/2 keyboard and everything below would pass with the defect
                // intact.
                i8042: false,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            let controllers = xhci_argv(&argv);
            if controllers.len() != 2 {
                return Err(format!(
                    "this profile is two controllers or it is nothing; argv has {controllers:?}"
                ));
            }
            // And every USB device is on the second of them. This is the
            // assertion that stops the test passing for the wrong reason: a
            // keyboard on the first controller is found by the defect too, and
            // no console line can tell that apart from the fix working.
            let usb = usb_argv(&argv);
            if let Some(bad) = usb.iter().find(|d| !d.contains("bus=xhci1.0")) {
                return Err(format!(
                    "{bad} is not on the second controller — a driver that stops at the \
                     first would find it"
                ));
            }
            for want in ["usb-kbd", "usb-mouse"] {
                if !usb.iter().any(|d| d.starts_with(want)) {
                    return Err(format!("no {want} to find: {usb:?}"));
                }
            }
            if !argv.iter().any(|a| a.contains("i8042=off")) {
                return Err("the i8042 is on; a PS/2 keyboard could deliver instead".to_string());
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();

            // Both controllers were brought up. One line here is the defect's
            // exact signature on the laptop.
            let found = boot.matches("xHCI: found at PCI ").count();
            if found != 2 {
                return Err(format!("{found} controller(s) initialised, want 2:\n{boot}"));
            }
            // And the empty one came up rather than being skipped: it has been
            // reset and armed with MSI-X, so dropping it would leave a live
            // interrupter with nothing draining its event ring.
            if !boot.contains("xHCI: no HID devices on the controller") {
                return Err(format!(
                    "the controller with nothing on it never reported itself:\n{boot}"
                ));
            }
            let binds = parse_xhci_binds(&boot);
            for want in ["keyboard", "mouse"] {
                if binds.iter().filter(|b| b.kind == want).count() != 1 {
                    return Err(format!("{want} not bound exactly once: {binds:?}\n{boot}"));
                }
            }
            // The boot stick is on the second controller too, so the disk
            // index the block layer holds names a device the first controller
            // does not have — the flattening `with_disk` does.
            if boot.matches("usb-storage: disk 0 ready").count() != 1 {
                return Err(format!("the stick on the second controller is not disk 0:\n{boot}"));
            }

            // Then the part no log line can show: an injected keystroke and an
            // injected pointer delta reach a userland process. Ground truth is
            // the host's own injection at the device boundary; the assertion is
            // what the guest printed.
            let Some((scale_x, scale_y)) = parse_rel_scale(&boot) else {
                return Err(format!("the kernel never said what pointer scale it used:\n{boot}"));
            };
            const DX: i32 = 40;
            const DY: i32 = -30;
            // Off the origin first: the accumulated position clamps at 0, so a
            // move up or left from there is invisible. A boot mouse reports each
            // axis as an i8, so this arrives clamped and its exact value is not
            // something to assert on.
            let (result, sent) = input_events_run(&mut qemu, (100, 100), (DX, DY));
            if let Some(err) = &result.error {
                return Err(format!("{err} after {sent} of the sequence\n{}", result.stdout));
            }

            let keys = parse_key_events(&result.stdout);
            let typed: String = keys
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?}, want it to contain \"hello\" — the keyboard on the \
                     second controller never reached userland:\n{}",
                    result.stdout
                ));
            }

            let pointer = parse_mouse_events(&result.stdout);
            // The delta the wire carried, not "it moved": a sign error in dy
            // and a dropped high bit both survive "it moved".
            let want = (DX * scale_x, DY * scale_y);
            let deltas: Vec<(i32, i32)> = pointer
                .windows(2)
                .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
                .collect();
            if !deltas.contains(&want) {
                return Err(format!(
                    "no pointer event moved by {want:?}; deltas seen: {deltas:?}\n{}",
                    result.stdout
                ));
            }
            let Some(down) = pointer.iter().position(|e| e.buttons == 0x01) else {
                return Err(format!("no left-button-down event; buttons seen: {:?}",
                    pointer.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()));
            };
            if !pointer[down + 1..].iter().any(|e| e.buttons == 0x00) {
                return Err(format!("the left button went down and never came up: {pointer:?}"));
            }
            eprintln!(
                "  [xhci] 2 controllers, HID only on the second; {} key events (typed {typed:?}), \
                 {} pointer events, delta {want:?} delivered",
                keys.len(),
                pointer.len()
            );
            Ok(())
        }
        "xhci_two_controllers" => {
            // Composition across controllers. `keyboard::handle_key` and
            // `mouse::handle_motion` are one held-set and one button merge for
            // the whole machine, which was argued for two devices on one bus
            // and never asked about two buses. The pointer half of it was
            // false: the merge was keyed by xHCI slot id, and slot ids are per
            // controller, so a pointer on slot 1 of each of two controllers was
            // one entry and each report published the other's buttons.
            let options = BootOptions {
                profile: qemu::Profile::MetalXhciBoth,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            let controllers = xhci_argv(&argv);
            if controllers.len() != 2 {
                return Err(format!("want two controllers, argv has {controllers:?}"));
            }
            let usb = usb_argv(&argv);
            for bus in ["bus=xhci.0", "bus=xhci1.0"] {
                let pointers = usb
                    .iter()
                    .filter(|d| d.contains(bus) && d.starts_with("usb-mouse"))
                    .count();
                if pointers != 1 {
                    return Err(format!(
                        "{pointers} pointer(s) on {bus}; the collision needs one on each: {usb:?}"
                    ));
                }
                if !usb.iter().any(|d| d.contains(bus) && d.starts_with("usb-kbd")) {
                    return Err(format!("no keyboard on {bus}: {usb:?}"));
                }
            }

            let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();

            let found = boot.matches("xHCI: found at PCI ").count();
            if found != 2 {
                return Err(format!("{found} controller(s) initialised, want 2:\n{boot}"));
            }
            if !boot.contains("xHCI: 2 controller(s), 4 HID device(s)") {
                return Err(format!(
                    "the machine-wide totals are not 2 controllers and 4 HID devices:\n{boot}"
                ));
            }
            let binds = parse_xhci_binds(&boot);
            for (want, count) in [("keyboard", 2), ("mouse", 2)] {
                let got = binds.iter().filter(|b| b.kind == want).count();
                if got != count {
                    return Err(format!("{got} {want}(s) bound, want {count}: {binds:?}\n{boot}"));
                }
            }

            // The merge itself. Two pointers, two entries in the button table,
            // and — the reason this profile is shaped the way it is — the same
            // slot id on both, so a source derived from the slot id is provably
            // one entry rather than accidentally two.
            let pointers = parse_pointer_sources(&boot);
            if pointers.len() != 2 {
                return Err(format!("{} pointers numbered, want 2: {pointers:?}\n{boot}",
                    pointers.len()));
            }
            if pointers[0].0 != pointers[1].0 {
                return Err(format!(
                    "the two pointers are on slots {} and {}, so a slot-keyed merge would not \
                     have collided and this test proves nothing:\n{boot}",
                    pointers[0].0, pointers[1].0
                ));
            }
            if pointers[0].1 == pointers[1].1 {
                return Err(format!(
                    "both pointers merge as source {} — one of them publishes the other's \
                     buttons:\n{boot}",
                    pointers[0].1
                ));
            }
            serial::Serial::named("boot console", boot.as_str()).must_be_clean()?;
            eprintln!(
                "  [xhci] 2 controllers, 4 HID; both pointers on slot {}, merging as sources {} \
                 and {}",
                pointers[0].0, pointers[0].1, pointers[1].1
            );
            Ok(())
        }
        "xhci_msi_only" => {
            // The T14's Thunderbolt controller printed `xHCI: no MSI-X
            // capability, using polled mode` on a real boot. There was no
            // polled mode: every read of an event ring in this driver is
            // `poll_if_pending`, gated on an `irq_ring` record that only
            // vector 0x21's ISR publishes, and that ISR is delivered only
            // through the MSI-X table the driver had just declined to program.
            // The controller was reset, started, and never read again — with
            // `USB keyboard ready on slot N` printed above it.
            //
            // Every controller in this suite had MSI-X, so this branch had
            // never executed. `msix=off` is the actuator.
            let options = BootOptions {
                profile: qemu::Profile::MetalXhciMsi,
                qmp: true,
                // As in `xhci_second_controller`: with a PS/2 keyboard on the
                // machine, QEMU could deliver the injected keystroke over it
                // and every assertion below would pass with the USB path dead.
                i8042: false,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            // The actuator is a device property, and argv is the only place a
            // device property is visible: a controller that quietly kept its
            // MSI-X table would make this whole test a re-run of the happy
            // path under a different name.
            let controllers = xhci_argv(&argv);
            let [storage, hid] = controllers[..] else {
                return Err(format!("this profile is two controllers; argv has {controllers:?}"));
            };
            if !hid.contains("msix=off") {
                return Err(format!("{hid} still has its MSI-X table"));
            }
            if hid.contains("msi=off") {
                return Err(format!(
                    "{hid} has no MSI either, so there is nothing to fall through to and the \
                     driver is expected to refuse it — that is xhci_no_interrupt"
                ));
            }
            // And the boot stick's controller has nothing at all, so the guest
            // does no USB storage I/O. Without this the test cannot fail:
            // `wait_transfer` drains the entire event ring and dispatches
            // every HID report in it, so the ESP log's idle-loop writes
            // deliver a keyboard's reports with no interrupt anywhere. That is
            // measured, not feared — the first shape of this profile passed
            // with MSI deliberately left disabled.
            for want in ["msix=off", "msi=off"] {
                if !storage.contains(want) {
                    return Err(format!(
                        "{storage} carries the boot stick and still has {want}'s mechanism, so \
                         storage I/O would drain the HID controller's ring for free"
                    ));
                }
            }
            let usb = usb_argv(&argv);
            for want in ["usb-kbd", "usb-mouse"] {
                if !usb.iter().any(|d| d.starts_with(want) && d.contains("bus=xhci1.0")) {
                    return Err(format!("no {want} on the MSI-only controller: {usb:?}"));
                }
            }
            if !argv.iter().any(|a| a.contains("i8042=off")) {
                return Err("the i8042 is on; a PS/2 keyboard could deliver instead".to_string());
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = serial::Serial::boot(&qemu);

            // What the driver programmed, off its own line. Both halves are
            // needed: MSI-X absent says the actuator did something, MSI
            // present says the driver found the other mechanism rather than
            // refusing the controller.
            boot.must_not_say("xHCI: MSI-X enabled")?;
            boot.must_say("xHCI: MSI enabled (vector 0x21)")?;
            // The line that named a mechanism this driver does not have.
            boot.must_not_say("polled mode")?;
            boot.must_be_clean()?;
            for want in ["keyboard", "mouse"] {
                let binds = parse_xhci_binds(boot.text());
                if binds.iter().filter(|b| b.kind == want).count() != 1 {
                    return Err(format!("{want} not bound exactly once: {binds:?}\n{}",
                        boot.text()));
                }
            }
            // The guest's half of the isolation above: no disk was bound, so
            // nothing in this boot can drain an event ring except an interrupt.
            boot.must_not_say("usb-storage: disk")?;

            // And then the half no log line can show, which is the whole
            // point: a driver that logs `MSI enabled` and programs the
            // capability wrong is indistinguishable from this one until a
            // device actually interrupts. Ground truth is the host's own
            // injection at the device boundary.
            let Some((scale_x, scale_y)) = parse_rel_scale(boot.text()) else {
                return Err(format!("the kernel never said what pointer scale it used:\n{}",
                    boot.text()));
            };
            const DX: i32 = 40;
            const DY: i32 = -30;
            // Off the origin first: the accumulated position clamps at 0, so a
            // move up or left from there is invisible.
            //
            // **`input_events_run`, which is `xhci_second_controller`'s own
            // sequence and was written out again here on fixed sleeps.** Two
            // things came of the copy and both were defects: nothing paced the
            // injection, so a key the host sent while the guest was behind was
            // indistinguishable from one this controller lost — the exact
            // reading `xhci_second_controller` moved off (§5.5.2) — and nothing
            // sent the right-button release `test_rs_input_events` ends on, so
            // every green run waited out the client's whole 30 s fallback
            // deadline. `input_events_end`'s own doc says every caller owes it
            // one; this was the caller that did not.
            let (result, sent) = input_events_run(&mut qemu, (100, 100), (DX, DY));
            if let Some(err) = &result.error {
                return Err(format!("{err} after {sent} of the sequence\n{}", result.stdout));
            }

            let keys = parse_key_events(&result.stdout);
            let typed: String = keys
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?}, want it to contain \"hello\" — the keyboard on an \
                     MSI-only controller never reached userland:\n{}",
                    result.stdout
                ));
            }

            let pointer = parse_mouse_events(&result.stdout);
            let want = (DX * scale_x, DY * scale_y);
            let deltas: Vec<(i32, i32)> = pointer
                .windows(2)
                .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
                .collect();
            if !deltas.contains(&want) {
                return Err(format!(
                    "no pointer event moved by {want:?}; deltas seen: {deltas:?}\n{}",
                    result.stdout
                ));
            }
            let Some(down) = pointer.iter().position(|e| e.buttons == 0x01) else {
                return Err(format!("no left-button-down event; buttons seen: {:?}",
                    pointer.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()));
            };
            if !pointer[down + 1..].iter().any(|e| e.buttons == 0x00) {
                return Err(format!("the left button went down and never came up: {pointer:?}"));
            }
            eprintln!(
                "  [xhci] no MSI-X table; MSI took vector 0x21, {} key events (typed {typed:?}), \
                 {} pointer events, delta {want:?} delivered",
                keys.len(),
                pointer.len()
            );
            Ok(())
        }
        "xhci_no_interrupt" => {
            // The terminal case of the same defect: a controller offering
            // neither mechanism. Nothing on a PCIe bus is really built that
            // way, which is exactly why the branch needs staging — "I cannot
            // drive this controller" is a state the driver has to be able to
            // reach and say, and it used to say "using polled mode" instead
            // and then enumerate a keyboard on it.
            //
            // Two controllers, and the crippled one is the second: the first
            // carries the boot stick, so a refusal that took the machine down
            // with it would show up here as a boot that never reaches userland.
            let options = BootOptions {
                profile: qemu::Profile::MetalXhciNoIrq,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            let controllers = xhci_argv(&argv);
            let [good, crippled] = controllers[..] else {
                return Err(format!("this profile is two controllers; argv has {controllers:?}"));
            };
            for want in ["msix=off", "msi=off"] {
                if !crippled.contains(want) {
                    return Err(format!("{crippled} still has {want}'s mechanism"));
                }
            }
            if good.contains("msi") {
                return Err(format!(
                    "{good} is crippled too; then a refusal could not be shown to be per \
                     controller and the machine would have no boot stick"
                ));
            }
            let usb = usb_argv(&argv);
            // The HID is on the controller that will be refused — otherwise
            // "nothing claimed a device" below is true because there was no
            // device to claim, which is not the same statement at all.
            if let Some(bad) = usb
                .iter()
                .filter(|d| !d.starts_with("usb-storage"))
                .find(|d| !d.contains("bus=xhci1.0"))
            {
                return Err(format!("{bad} is not on the controller under test"));
            }
            if !usb.iter().any(|d| d.starts_with("usb-kbd") && d.contains("bus=xhci1.0")) {
                return Err(format!("no keyboard for the driver to refuse: {usb:?}"));
            }
            if !usb.iter().any(|d| d.starts_with("usb-storage") && d.contains("bus=xhci.0")) {
                return Err(format!("the boot stick is not on the good controller: {usb:?}"));
            }

            let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = serial::Serial::boot(&qemu);

            // Both controllers were looked at, one was refused by name, and
            // the refusal says what it means rather than naming a mode.
            if boot.text().matches("xHCI: found at PCI ").count() != 2 {
                return Err(format!("both controllers should be reached:\n{}", boot.text()));
            }
            boot.must_say("xHCI: NOT INITIALISED at PCI")?;
            boot.must_not_say("polled mode")?;

            // And nothing claimed a device on it. This is the assertion the
            // old code failed: it bound the keyboard, printed
            // `USB keyboard ready on slot 2`, and delivered nothing.
            let binds = parse_xhci_binds(boot.text());
            if !binds.is_empty() {
                return Err(format!(
                    "a device was announced on a controller nothing can read: {binds:?}\n{}",
                    boot.text()
                ));
            }
            boot.must_say("xHCI: 1 controller(s), 0 HID device(s)")?;
            // The good controller is untouched by its neighbour's refusal,
            // and the machine reached userland — `boot_log` ends at the ready
            // marker, so having one at all is that assertion.
            boot.must_say("usb-storage: disk 0 ready")?;
            boot.must_be_clean()?;
            eprintln!(
                "  [xhci] 2 controllers, the second with neither MSI-X nor MSI: refused by \
                 name, 0 HID announced, boot stick on the first still bound"
            );
            Ok(())
        }
        "nvme_large_device" => {
            // Device *size* is a shape dimension, and it is the one nobody had
            // varied: every test image was small enough that an index sized
            // per device block fit under the object allocator's 2 MiB ceiling,
            // so the first boot on the laptop was the first time anything
            // asked for a device-sized allocation — and it died in
            // page_cache::init before it mounted anything.
            let options = BootOptions {
                profile: qemu::Profile::MetalDisk,
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let log = qemu.boot_log().to_string();

            // The mechanism, not the argv: a big file on the host proves
            // nothing until the guest's own driver says it enumerated a big
            // namespace. This is the number the T14 printed.
            let Some(blocks) = parse_nvme_blocks(&log) else {
                return Err(format!("the NVMe driver printed no block count:\n{log}"));
            };
            if blocks != qemu::NVME_T14_BLOCKS {
                return Err(format!(
                    "the guest enumerated {blocks} blocks, not the T14's {}",
                    qemu::NVME_T14_BLOCKS
                ));
            }

            // And the cache did not size its index by that number.
            //
            // The bound has to sit *below* the allocator's 2 MiB ceiling to be
            // able to fire at all, which is a narrower window than it looks:
            // a hashbrown index costs 17 B per bucket and its capacities are
            // 7/8 of a power of two, so the last one that fits under the
            // ceiling is 114,688 and the next is unreachable. 16,384 leaves
            // room for a fixed reserve mirroring `slot_to_block`'s 4096 (which
            // rounds up to 7168) and rejects every device-proportional reserve
            // down to one entry per 4 MiB of disk. Measured red at 57,344,
            // which is what `block_count / 1024` asks for and the allocator
            // lets through.
            let Some(index) = parse_page_cache_index(&log) else {
                return Err(format!("the page cache printed no index size:\n{log}"));
            };
            if index > 16_384 {
                return Err(format!(
                    "the block index is sized for {index} blocks on a {blocks}-block device — \
                     that is proportional to the device again:\n{log}"
                ));
            }

            // The whole storage stack on the real geometry, not just the boot:
            // format, allocate, write, read back.
            let result = qemu.run_test("test_rs_nvme_home_roundtrip", Duration::from_secs(20));
            if !check_rust_result(&result) {
                return Err(format!(
                    "the /home round trip failed on a {blocks}-block device:\n{}",
                    result.stdout
                ));
            }

            // Then shut down, which is the only thing that runs the page
            // cache's write-back over every dirty slot the format left —
            // ~1900 of them on a device this size against 8 on the small one,
            // so the coalescing loop is only ever exercised at scale here.
            //
            // The kernel's own shutdown lines are observable now: the ring
            // is drained in `acpi::shutdown()` before it cuts the power.
            // Asserted below, because "how far did the sync get" is the only
            // diagnostic a shutdown failure has, and on a machine with no
            // serial it is the only channel there is.
            let image = qemu.nvme_image().to_path_buf();
            writeln!(qemu.stdin_mut(), "run shutdown").expect("write to QEMU stdin");
            qemu.flush_stdin();
            let tail = qemu.drain_serial(Duration::from_secs(20));

            for line in ["Syncing filesystems...", "Shutting down."] {
                if !tail.contains(line) {
                    return Err(format!(
                        "{line:?} never reached the host — the ring was still \
                         holding it when the power was cut:\n{tail}"
                    ));
                }
            }

            // The shutdown half is the one this conversion is for: `tail` is a
            // `drain_serial` window, and an empty drain used to pass its panic
            // scan in silence. It carries kernel lines of its own -- measured,
            // five, including both lines asserted just above -- so requiring
            // liveness of it is a real check and not a new flake.
            serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
            serial::Serial::named("shutdown drain", tail.as_str()).must_be_clean()?;

            // Ground truth at the hardware boundary: the backing file is what
            // the *device* received, so this is the one place a storage claim
            // does not rest on the guest's account of itself. The clean flag
            // reaches the platter only through `PageCache::sync`, and the
            // backup superblock only through a write at byte 256,060,510,208 —
            // the far end of a 244 GB device.
            for (name, block) in [("primary", 0), ("backup", qemu::NVME_T14_BLOCKS - 1)] {
                let sb = read_superblock(&image, block)
                    .map_err(|e| format!("{name} superblock at block {block}: {e}"))?;
                if sb.block_count != qemu::NVME_T14_BLOCKS {
                    return Err(format!(
                        "the {name} superblock was formatted for {} blocks, not {}",
                        sb.block_count,
                        qemu::NVME_T14_BLOCKS
                    ));
                }
                if !sb.is_clean() {
                    return Err(format!(
                        "the {name} superblock is not marked clean — the write-back at \
                         shutdown did not reach the device"
                    ));
                }
            }

            // And the image is still sparse. A materialized one is how a test
            // disk ends up small enough to hide this class of bug in the first
            // place, and 244 GB of zeros is not something to leave on a laptop.
            let (apparent, allocated) = image_extent(&image);
            if apparent != qemu::NVME_T14_BYTES {
                return Err(format!("the image is {apparent} bytes, want {}", qemu::NVME_T14_BYTES));
            }
            if allocated > 1024 * 1024 * 1024 {
                return Err(format!(
                    "the image occupies {allocated} bytes of the host's disk — it is not sparse"
                ));
            }
            eprintln!(
                "  [nvme] {blocks} blocks, index sized for {index}; both superblocks clean; \
                 image {} MiB on disk of {} GB apparent",
                allocated / (1024 * 1024),
                apparent / 1_000_000_000
            );
            Ok(())
        }
        // No guest: the instrument itself, in both directions. `screen_decoder`
        // is the same idea for the framebuffer decoder.
        //
        // Three of them under one name, because they are one subject: what a
        // console line says died, what a wait does about it, and the fact that
        // only one place in the harness is allowed to answer either.
        "serial_vocabulary" => {
            serial::self_check()?;
            qemu::ceiling_self_check()?;
            one_vocabulary()
        }
        "suspend_detector" => common::clock::self_check(),
        "suspend_invalidates_a_verdict" => suspend_invalidates_a_verdict(),
        "stall_is_not_a_verdict" => stall_is_not_a_verdict(),
        "alone_line_reports_the_alone_run" => alone_line_reports_the_alone_run(),
        "nvme_image_is_held_by_one_guest" => nvme_image_is_held_by_one_guest(),
        "expected_failure_verdicts" => expected_failure_verdicts(),
        "expected_failure_exit_status" => expected_failure_exit_status(),
        "expected_failure_entries" => expected_failure_entries(),
        "control_regs_verdict" => control_regs_verdict(),
        "i8042_quarantine_verdict" => idle_trip_verdict(),
        "suite_split" => suite_split(),
        "nightly_tier_is_announced" => nightly_tier_is_announced(),
        "nvme_wide_sector" => {
            // The other half of "a device's size is a shape dimension": not how
            // many sectors, but how big one is. `lba_ds` is an 8-bit
            // device-reported shift that reached `1 << lba_ds` and then
            // `4096 / sector_size`, so an 8 KiB-format namespace divided by
            // zero at 0.068 s — before storage, before a console, and on a
            // machine whose only channel out is the one that does not exist
            // yet. Every profile in this tree took QEMU's implicit 512-byte
            // namespace, so nothing could ask.
            //
            // The guest is expected to die here, which is what makes
            // `ready_marker` the driver's own refusal: anything but
            // DEFAULT_READY tells the harness a panic is the outcome under
            // test rather than a boot failure.
            const REFUSAL: &str = "NVMe: namespace reports";
            let options = BootOptions {
                profile: qemu::Profile::NvmeWideSector,
                ready_marker: REFUSAL,
                ..Default::default()
            };
            let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            // It dies before virtio-console exists, so the 16550 file is the
            // only record — which is also the T14's situation exactly.
            let mut log = serial::Serial::boot(&qemu);
            log.push(&qemu.uart_log());

            // Named, not just refused: the value the device reported is the
            // whole diagnostic on a machine that will not boot again without
            // it. A bare "refused" line would pass with the number wrong.
            log.must_say("2^13-byte sectors")?;
            // And it refused rather than dividing: the pre-fix failure was
            // `attempt to divide by zero`, which is also a panic and would
            // satisfy the check above if it only looked for one. Both of these
            // are absence claims, so both go through `must_not_say`, which
            // fails rather than passing if the capture came back empty.
            log.must_not_say("divide by zero")?;
            // Nothing downstream ran. `block device id=` is the line
            // `NvmeBlockDevice::new` logs, and it is the call that divided.
            log.must_not_say("NVMe: block device id=")?;
            eprintln!("  [nvme] 8 KiB-format namespace refused by name, before storage came up");
            Ok(())
        }
        "va_exhaustion" => {
            // `find_gap` returning None was an `.expect` on five paths. It is
            // an error return now, and this is the only way to reach it: the
            // arena is ~1015 GB and every region in it costs at worst twice
            // its size in physical memory, so the PMM refuses hundreds of
            // gigabytes before the address space does. `test-tiny-va` moves
            // the floor and nothing else — the argument for the actuator is on
            // `vma::ALLOC_FLOOR`.
            //
            // Which is also why the feature has to boot a whole system: an
            // arena too small for a process to map its TLS and its heap would
            // prove the actuator works and nothing about the kernel.
            let options = BootOptions {
                kernel_params: &["test-tiny-va"],
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();

            let result = qemu.run_test("test_rs_va_exhaustion", Duration::from_secs(30));
            if !check_rust_result(&result) {
                return Err(format!("the guest did not survive exhaustion:\n{}", result.stdout));
            }
            // The guest asserts the mapping count itself — the band that
            // separates "address space ran out" from "memory ran out". Here,
            // that nothing in the kernel panicked on the way: the process
            // exiting 0 says its own syscalls returned, not that some other
            // CPU stayed up.
            //
            // Two captures, two `Serial`s rather than one with the second
            // pushed into it: concatenating them would let the boot half's
            // kernel lines vouch for the run half's liveness, which is the
            // vacuum this is being converted out of. Measured: the run window
            // carries 14 kernel lines of its own.
            serial::Serial::named("boot console", boot).must_be_clean()?;
            serial::Serial::named("test serial", result.serial.as_str()).must_be_clean()?;
            eprintln!("  [va] {}", result.stdout.trim());
            Ok(())
        }
        "readdir_bound" => {
            // Two defects, one workload, no kernel feature: `Vfs::list` had no
            // cap and `SYS_READDIR` reported the bytes it managed to write, so
            // a directory of 32,769 files panicked the kernel and one of 34,816
            // came back as 4125 entries and a success.
            //
            // Its own boot because it fills `/tmp` to the listing limit and
            // leaves it there — in the shared boot every later
            // `read_dir("/tmp")` would be refused, which is a cascade rather
            // than a failure.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions::default(),
            );
            serial::Serial::boot(&qemu).must_be_clean()?;

            let result = qemu.run_test("test_rs_readdir_bound", Duration::from_secs(60));
            if let Some(err) = &result.error {
                return Err(format!("the guest stopped answering: {err}\nserial:\n{}", result.serial));
            }
            if !check_rust_result(&result) {
                return Err(format!("readdir_bound failed:\n{}", result.stdout));
            }
            // The refusal must be an error return and nothing else. A panic
            // inside `Vfs::list` is the defect this replaced, and the guest
            // process exiting 0 does not rule one out on another CPU.
            serial::Serial::named("test serial", result.serial.as_str()).must_be_clean()?;
            for line in result.stdout.lines().filter(|l| l.contains("PASS")) {
                eprintln!("  [readdir]{}", line.trim_start_matches("  PASS"));
            }
            Ok(())
        }
        "fpu_isolation" => {
            // Two boots that must answer
            // differently: the shipped kernel preserves the whole user machine
            // state across every transition out of Ring 3, and the kernel built
            // with `fpu-save-nothing` — the same bracket with the two FP
            // instructions taken out — must fail the same three arms.
            //
            // Without the second arm the first proves only that the machine
            // works, which it did before this gate existed too.
            //
            // smp=1 in both, and that is the stronger machine rather than the
            // weaker one: two of the arms are about a register file surviving
            // from one process to the next, which needs the two to share a CPU.
            // On the shared boot's two CPUs that is a coin flip, which is why
            // CI's own observation of the defect was intermittent.
            let one_cpu = || BootOptions { smp: 1, ..BootOptions::default() };

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, one_cpu());
            serial::Serial::boot(&qemu).must_be_clean()?;
            let result = qemu.run_test("test_rs_fpu_isolation", Duration::from_secs(120));
            if let Some(err) = &result.error {
                return Err(format!(
                    "the guest stopped answering: {err}\nserial:\n{}",
                    result.serial
                ));
            }
            if !check_rust_result(&result) {
                return Err(format!("fpu_isolation failed:\n{}", result.stdout));
            }
            // No `must_be_clean` on the run: arm 2 kills `fault_gate_child`
            // on purpose, and the kernel names every Ring 3 fault it takes.
            for line in result.stdout.lines() {
                eprintln!("  [fpu] {}", line.trim());
            }
            // The lane's disk images are one set, and QEMU takes a write lock
            // on them for as long as a guest lives.
            drop(qemu);

            let mut blind = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { kernel_features: &["fpu-save-nothing"], ..one_cpu() },
            );
            serial::Serial::boot(&blind).must_be_clean()?;
            let negative = blind.run_test("test_rs_fpu_isolation", Duration::from_secs(120));
            if let Some(err) = &negative.error {
                return Err(format!(
                    "the negative-control guest stopped answering: {err}\nserial:\n{}",
                    negative.serial
                ));
            }
            if negative.exit_code == Some(0) {
                return Err(format!(
                    "the kernel built with `fpu-save-nothing` passed `fpu_isolation`, so the \
                     gate asserts nothing:\n{}",
                    negative.stdout
                ));
            }
            eprintln!(
                "  [fpu] fpu-save-nothing: exit {:?}, which is the gate having teeth",
                negative.exit_code
            );
            Ok(())
        }
        "sched_check_build" => {
            // The scheduler core's own instruments, run on a real machine —
            // spec §10.2's on-target counterpart to everything the simulator
            // does.
            //
            // `kernel/Cargo.toml` has forwarded `sched-check =
            // ["toyos-sched/check"]` since the check build was written, and
            // until this test nothing in `src/` or `tests/` ever asked for it.
            // So `cpu::MAX_PASS_NS`, the pass-cost measurement and
            // `invariants::check_cpu` were compiled by no CI run at all: a
            // quantum never armed, a task whose container disagreed with its
            // state word, and a distribution of passes with mass over the
            // budget were each caught by nothing on hardware, however green the
            // simulator was.
            //
            // **What the simulator cannot say.** Two of the three instruments
            // are asserts about state, and the sim checks those globally and
            // better. The third is a measurement of *cost*, and the sim's clock
            // does not advance inside a step — `scenarios::overlong_pass` feeds
            // the recorder a modelled pass cost, which proves the recorder
            // compiles and counts, not what a real pass on real silicon costs.
            // Only a booted kernel reads a TSC.
            //
            // **And the cost half is gated here rather than in the kernel, and
            // against a recorded sample rather than against the budget.** What
            // a pass measures is wall clock across the pass, and a guest's wall
            // clock runs while the host has taken its vCPU away — so the
            // quantity includes a term the host's scheduler sets. Measured
            // 2026-08-18, that term moves *every* order statistic and not only
            // the tail, so `common::passcost` judges each accelerator against
            // what that accelerator has been recorded producing, and takes no
            // verdict at all where the recorded sample supports none. Its own
            // two-directions self-check runs first, because a gate that must
            // stay green under host descheduling has to be shown doing so on a
            // case no booted machine can stage.
            //
            // **The workload is `sched_stress`** because the asserts are dense
            // on exactly what it does: it spawns burners that drive vruntime,
            // blocks and wakes across io_uring and ports, and forces a
            // Runnable→NonRunnable→Runnable cycle. Every one of those is a pass,
            // and every pass on every CPU runs both checks. `smp: 2` is the
            // default and it is deliberate — one CPU cannot migrate, and
            // invariant T's arming is per CPU.
            //
            // **That the asserts are compiled in at all is not asked here.** A
            // guest proves they did not fire, and a kernel with the feature
            // quietly dropped proves that more easily; the artifact is asked
            // instead, at build time, by `assert_sched_check_matches_features` —
            // 0 of 3 assert texts in the shipping kernel, 3 of 3 in this one.
            // This half is the other one: on a machine that really carries them,
            // honest work does not trip them.
            common::passcost::self_check()?;
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_features: toyos_build::build::SCHED_CHECK_KERNEL,
                    ..BootOptions::default()
                },
            );
            // The boot is already thousands of passes on both CPUs, and an
            // assert that fires there takes the machine down before userland.
            serial::Serial::boot(&qemu).must_be_clean()?;

            let result = qemu.run_test("test_rs_sched_stress", Duration::from_secs(120));
            if let Some(err) = &result.error {
                return Err(format!(
                    "the check-build guest stopped answering, which is what a scheduler \
                     assert firing looks like from here: {err}\nserial:\n{}",
                    result.serial,
                ));
            }
            if !check_rust_result(&result) {
                return Err(format!(
                    "sched_stress failed on the check build:\n{}",
                    result.stdout
                ));
            }
            // An assert fires as a kernel panic on whichever CPU took the pass,
            // and the guest process can still exit 0 while another CPU is dying
            // — so the serial is read as well as the exit code.
            serial::Serial::named("test serial", result.serial.as_str()).must_be_clean()?;
            for line in result.stdout.lines() {
                eprintln!("  [sched-check] {}", line.trim());
            }
            // The whole boot, in the three pieces a capture comes in: the ready
            // marker, the hole after it, and the test window. The counters are
            // cumulative since boot, so the last line each CPU published is the
            // whole of that CPU's run.
            let mut capture = serial::Serial::boot(&qemu);
            capture.push(&result.before);
            capture.push(&result.serial);
            let reports = common::passcost::reports(capture.text());
            if reports.is_empty() {
                return Err(format!(
                    "the check build published no pass-cost report at all, so nothing above \
                     gated what a pass costs — every pass on this boot went unmeasured or \
                     unspoken. `{}` is the prefix that never appeared:\n{}",
                    toyos_sched::cpu::PassCostReport::PREFIX,
                    capture.text(),
                ));
            }
            // Which recorded sample this run is judged against, before the
            // numbers it judges: a verdict taken against a sample is
            // unreadable without naming the sample, and a run that judged
            // nothing has to say so where a reader cannot miss it.
            let baseline = common::passcost::baseline();
            eprintln!("  [sched-check] {}", common::passcost::judgement_line(baseline));
            for report in &reports {
                eprintln!("  [sched-check] {}", common::passcost::describe(report));
            }
            for report in &reports {
                common::passcost::verdict(report, baseline)?;
            }
            Ok(())
        }
        "klogd_hosted" => {
            // The machine's first kernel thread, and the two things about it
            // no other test in the suite can see.
            //
            // **That it is hosted at all.** `klogd` runs on the ordinary
            // scheduler with no address space of its own — `driver::spawn`
            // names the kernel's `cr3` — through a trampoline that never
            // issues an `iretq`. It gets a process-table entry rather than a
            // bare task, and that is what makes it nameable: without one a
            // crash report would print a pid nothing in the machine resolves.
            //
            // **That its panic is not recoverable, deterministically.** The
            // ordinary predicate is `syscall_rip() != 0 &&
            // current_tid().is_some()`, and `syscall_rip` is never cleared —
            // so a kernel task reads whatever user thread last ran on *that*
            // CPU left behind, and recovers or halts by accident of work
            // stealing. The row in `sched::kthread` is what replaces the
            // accident with an answer.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions::default(),
            );
            let boot = serial::Serial::boot(&qemu);
            boot.must_be_clean()?;
            let line = boot.must_say("kthread: klogd")?;
            if !line.contains("halts the machine") {
                return Err(format!("klogd is hosted but claims the wrong panic row: {line:?}"));
            }
            eprintln!("  [klogd] {}", line.trim());

            // **The other two threads, and the opposite row.** `usbd` owns
            // the xHCI port machine and `iod` the write-back queue, so a
            // stuck USB enumeration cannot stop the log. Their panics are
            // *recoverable* and `klogd`'s deliberately is not — a killed
            // drainer is the one loss nothing left alive can report — and
            // this is the one boot in the suite where all three rows are on
            // the wire together.
            for name in ["usbd", "iod"] {
                let line = boot.must_say(&format!("kthread: {name}"))?;
                if !line.contains("kills the thread") {
                    return Err(format!(
                        "{name} is hosted but claims the wrong panic row: {line:?}"
                    ));
                }
                eprintln!("  [kthread] {}", line.trim());
            }

            drop(qemu);

            // The marker is a line of the crash *report* rather than `PANIC:`
            // itself, because `boot_log` stops at the marker and the name is
            // printed after the header — a boot stopped at the header would
            // have nothing left to assert the process table against.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["klogd-panic"],
                    ready_marker: "Process: klogd",
                    ..Default::default()
                },
            );
            let mut dead = serial::Serial::boot(&qemu);
            dead.must_say("PANIC:")?;
            dead.must_say("klogd-panic: the console drainer died")?;
            // The process table answered for a task with no *user* address
            // space — since C6 it names the kernel's, which is what let
            // `KernelPayload.address_space` stop being an `Option`.
            dead.must_say("Process: klogd")?;

            // The verdict. A *recovered* panic kills the thread and lets the
            // machine carry on into userland, which announces itself; the
            // fatal branch halts every CPU. The window is a liveness margin
            // and not a threshold: `klogd` panics as the scheduler starts, and
            // the arm this must never become reaches the marker a few hundred
            // milliseconds later — so three seconds is a tenfold margin over
            // the state it refuses, and it is the whole of this test's fixed
            // cost against the Fast ceiling.
            const CARRIED_ON: Duration = Duration::from_secs(3);
            dead.push(&qemu.drain_serial(CARRIED_ON));
            dead.must_not_say(qemu::DEFAULT_READY)?;
            eprintln!("  [klogd] a kernel thread's panic halted the machine rather than recovering");

            drop(qemu);

            // **The same panic on the other row, and it is the direction
            // nothing had ever taken.** Two rows in one table are one row
            // until both branches have been walked: before this arm, every
            // kernel-thread panic this tree had ever run took `OnPanic::Halt`,
            // so `Recover` was a value rather than a path — and the path it
            // names goes through `poison_tid`, the idle loop's `reap_poisoned`
            // and `zombify_poisoned`, none of which had ever seen a task with
            // no user address space. A row that quietly halted the machine
            // would make `usbd` and `iod` worse than the thread they were
            // split off from.
            //
            // The verdict is content in the same window and never a timeout:
            // the boot returns at the crash report's own line, and what the
            // three seconds after it must contain is the ready marker the
            // arm above must *not*.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["usbd-panic"],
                    ready_marker: "Process: usbd",
                    ..Default::default()
                },
            );
            let mut survived = serial::Serial::boot(&qemu);
            survived.must_say("PANIC:")?;
            survived.must_say("usbd-panic: the device thread died")?;
            survived.must_say("Process: usbd")?;
            survived.push(&qemu.drain_serial(CARRIED_ON));
            survived.must_say(qemu::DEFAULT_READY)?;
            eprintln!("  [usbd] a kernel thread's panic killed the thread and the machine booted");
            Ok(())
        }
        "reentry_names_the_first_panic" => {
            // **The one class of crash that is by definition two bugs deep, and
            // the one class that used to leave no evidence.** A machine two
            // crashes deep said `DOUBLE PANIC` and nothing else — not what the
            // first crash was, not where, and not what the second one was
            // (`issues/panic-path/a-double-panic-at-boots-edge-says-nothing-but-its-name.md`).
            // What closed it is a bounded byte copy taken *before* either
            // report runs, into a static reserved at link time
            // (`kernel/src/panic.rs`), so what the second crash reads is the
            // first crash's own words rather than whatever the log path
            // survived.
            //
            // **Two names because the two dead ends are reached by different
            // accidents**, and `double_panic_names_the_fault` is the other. The
            // reentry guard fires when the panic *report* panics, on a CPU whose
            // panic depth is already one; `DOUBLE PANIC` fires when a panic
            // lands on a CPU that a *fault* had, whose depth is zero.
            //
            // **This one: the panic path panics.** `test-late-panic` is a real
            // panic with a literal message at a fixed site, and
            // `panic-in-report` kills the report of it before it says a word —
            // so everything on the wire about the first panic came out of the
            // capture. The reentry guard writes straight to the 16550 with no
            // lock, deliberately, because the record path is exactly what has
            // just failed: the marker and the verdict are both in the UART file
            // rather than on the console.
            const REENTRY: &str = "PANIC REENTRY";
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["test-late-panic", "panic-in-report"],
                    ready_marker: REENTRY,
                    ..Default::default()
                },
            );
            let mut reentry = serial::Serial::boot(&qemu);
            reentry.push(&qemu.uart_log());
            // Nothing of the first panic reached the record ring: the report
            // that writes `PANIC:` is the one that died. So this is not a
            // weaker way of reading the same line — without the capture there
            // is no other copy of the site anywhere in the capture.
            reentry.must_not_say("PANIC:")?;
            let header = reentry.must_say(REENTRY)?;
            eprintln!("  [reentry] {}", header.trim());
            let first = reentry.must_say("first (apic")?;
            // `src/main.rs` and not `kernel/src/main.rs`: `build.rs` runs cargo
            // in `kernel/`, so the kernel's own `file!()` is crate-relative.
            for want in ["panic at ", "src/main.rs:", "test-late-panic: on-screen console check"] {
                if !first.contains(want) {
                    return Err(format!(
                        "the reentry report does not carry {want:?} — the first panic's own \
                         words are what the capture exists to keep: {first:?}"
                    ));
                }
            }
            eprintln!("  [reentry] {}", first.trim());
            let second = reentry.must_say("second: panic at")?;
            if !second.contains("panic-in-report: the crash report panicked") {
                return Err(format!(
                    "the reentry report does not name the second panic: {second:?}"
                ));
            }
            eprintln!("  [reentry] {}", second.trim());
            Ok(())
        }
        "double_panic_names_the_fault" => {
            // **A panic on top of a fault, which is what the sighting was and
            // what no test in this tree had ever executed**: a Ring 0 exception
            // is not something a guest program or a QEMU property can produce,
            // so `fatal_exception`'s kernel arm — and the `DOUBLE PANIC` branch
            // only reachable through it — had never run under a test at all.
            // `reentry_names_the_first_panic` is the other dead end and carries
            // the shared argument.
            //
            // `test-kernel-fault` takes the `#UD` with nothing current, so
            // `fatal_exception` runs its kernel arm; `panic-in-report` panics it
            // before the `FAULT rip=…` line, which is the shape the sighting had
            // — a fault whose report died before saying anything at all. This
            // dead end says it as a record too, because a machine with no serial
            // port has no other channel, so the verdict is on the console.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["test-kernel-fault", "panic-in-report"],
                    ready_marker: "DOUBLE PANIC",
                    ..Default::default()
                },
            );
            let mut double = serial::Serial::boot(&qemu);
            double.push(&qemu.uart_log());
            // The fault said nothing about itself, which is the state under
            // test: `FAULT rip=…` is `fatal_exception`'s own first line and it
            // never ran.
            double.must_not_say("FAULT rip=")?;
            let line = double.must_say("DOUBLE PANIC")?;
            for want in [
                // Which of the four states the arriving panic found. A panic on
                // top of a fault and a panic on top of a panic are different
                // machines and the old line named neither.
                "already in Fatal",
                // The fault, by the same name the report it never reached would
                // have given it, and where it was.
                "invalid opcode",
                "rip=0x",
                // And the panic that ended it.
                "second: panic at ",
                "panic-in-report: the crash report panicked",
            ] {
                if !line.contains(want) {
                    return Err(format!(
                        "the DOUBLE PANIC line does not carry {want:?}, so the machine still \
                         dies without saying what it was already doing: {line:?}"
                    ));
                }
            }
            eprintln!("  [double] {}", line.trim());
            // And the same report on the channel that cannot be held by
            // whatever broke — the raw port write goes out before the record
            // does, so a wedge in the log path costs the second copy and never
            // the first.
            let raw = serial::Serial::named("16550 file", qemu.uart_log());
            let raw_line = raw.must_say("first (apic")?;
            if !raw_line.contains("invalid opcode") {
                return Err(format!(
                    "the lock-free copy of the report does not name the fault: {raw_line:?}"
                ));
            }
            eprintln!("  [double] {}", raw_line.trim());
            Ok(())
        }
        "pre_idle_wedge_speaks" => {
            // **The worst diagnostic hole in the tree, closed and gated.**
            // Before this branch a boot that wedged before `enter_idle_loop`
            // produced nothing at all on the console — not "less", *nothing*,
            // including every line it had logged — because the only two things
            // that drained the byte ring were the timer tick and the idle loop,
            // and the machine reaches neither
            // (`issues/diagnostics/pre-idle-wedge-says-nothing.md`).
            // `Drain::Inline` puts every record on the wire as it is committed,
            // for the whole boot, so the end of the log is now where the
            // machine stopped rather than where it last drained.
            //
            // The verdict is content and not a duration: what is asserted is
            // which lines arrived, from the first phase to the wedge, and that
            // the phase after it never did.
            const WEDGE: &str = "pre-idle-wedge: the boot stops here";
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    // **`Metal`, because the console has to exist in phase 1
                    // for the claim to mean anything.** The headless profile's
                    // console is a virtio device the kernel does not bring up
                    // until phase 6, so a machine wedged in phase 3 has nowhere
                    // to put a byte on that shape and the records wait in their
                    // shards for a backend that never arrives. metal-sim keeps
                    // a 16550, which is up from the second statement of
                    // `kernel_main` — and it is also the profile this whole
                    // feature exists for, being the shape that gets flashed.
                    profile: qemu::Profile::Metal,
                    kernel_params: &["pre-idle-wedge"],
                    ready_marker: WEDGE,
                    ..Default::default()
                },
            );
            let boot = serial::Serial::boot(&qemu);
            // Every phase up to the wedge, oldest first — the first line the
            // machine ever logs, both boot checkpoints before phase 3, and a
            // phase-3 line from between them and the wedge.
            for needle in [
                "serial: 16550 loopback read",
                "Boot: CPU ready",
                "gpt: firmware booted us from partition",
                "Boot: storage ready",
                WEDGE,
            ] {
                boot.must_say(needle)?;
            }
            // And nothing from after it, which is what says the machine really
            // is wedged rather than slow.
            for needle in ["Boot: peripherals ready", "Boot: complete"] {
                boot.must_not_say(needle)?;
            }
            eprintln!(
                "  [wedge] {} kernel line(s) reached the console from a machine that never \
                 reached a scheduler pass",
                boot.kernel_lines(),
            );
            Ok(())
        }
        "short_sleep_livelock" => {
            // Task #156. A `nanosleep` whose deadline is already past when the
            // pass arms the one-shot armed the register's one-tick minimum, and
            // the Ring 0 timer stub reloads whatever was last armed — so the
            // CPU took that interrupt again before it could execute the
            // instruction after the `wrmsr` that armed it, forever. Eight boots
            // of the owner's T14 caught it twice by NMI, at
            // `arm_one_shot+0x8d` and at `timer_entry+0x0`, which are the two
            // instruction boundaries of exactly that loop.
            //
            // Its own boot because the failure is a CPU that never runs
            // anything again: on the shared boot it would be reported against
            // whichever test followed it.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions::default(),
            );
            serial::Serial::boot(&qemu).must_be_clean()?;

            let result = qemu.run_test("test_rs_abuse_short_sleep", Duration::from_secs(60));
            if let Some(err) = &result.error {
                return Err(format!(
                    "a sleep shorter than one LAPIC tick took the CPU with it: {err}\nserial:\n{}",
                    result.serial,
                ));
            }
            if !check_rust_result(&result) {
                return Err(format!("abuse_short_sleep failed:\n{}", result.stdout));
            }
            serial::Serial::named("test serial", result.serial.as_str()).must_be_clean()?;
            eprintln!("  [sleep] {}", result.stdout.lines().last().unwrap_or("").trim());
            Ok(())
        }
        "heap_ceiling_recovery" => {
            // A panic inside the kernel allocator's own lock left the heap
            // locked for the rest of the boot: the panicking thread never
            // unwinds, so `now` never advances, and the CPU that recovered
            // spun `Lock::lock` to its 500M-spin deadline on its next `alloc`
            // or `free` — then panicked again, forever. The fix moved the
            // ceiling check to `KernelAllocator::alloc`, before the lock.
            //
            // `smp: 1` is what makes the claim precise. The property is that
            // *the recovered CPU* survives its next allocation; on a wider
            // machine `/bin/echo` could run somewhere else and pass without
            // touching it. With one CPU there is nowhere else.
            //
            // The actuator is SYS_DEBUG 5, 6 and 7, and the reason it is not
            // an ordinary workload is beside them in `arch/syscall.rs`: routes
            // past the ceiling do still exist,
            // and each of them holds the VFS lock when it dies, so the
            // machine wedges either way and the allocator's recovery cannot
            // be observed on its own.
            let options = BootOptions {
                smp: 1,
                kernel_features: ACTUATOR_KERNEL,
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            serial::Serial::boot(&qemu).must_be_clean()?;

            let result = qemu.run_test("test_rs_heap_ceiling", Duration::from_secs(30));
            if let Some(err) = &result.error {
                // The wedge's signature. Before the fix this is where the test
                // ends: the child's panic strands the allocator, the guest
                // stops answering, and `run_test` runs out of window.
                return Err(format!(
                    "the guest stopped answering after the over-ceiling panic: {err}\n\
                     serial:\n{}",
                    result.serial
                ));
            }
            if !check_rust_result(&result) {
                return Err(format!("heap_ceiling failed:\n{}", result.stdout));
            }

            // The panic must be the one this test asked for, and it must have
            // fired where the fix put it. `mm/alloc.rs` appears in the report
            // either way — the old assert was in the same file — so the needle
            // is the message, which names the ceiling rather than the page
            // source's own request.
            let serial = serial::Serial::named("test serial", result.serial.as_str());
            serial.must_say("PANIC:")?;
            let line = serial.must_say("exceeds MAX_HEAP_ALLOC")?;
            eprintln!("  [heap] {}", line.trim());
            Ok(())
        }
        "cache_eviction" => {
            // Both disk caches grew for the life of the boot: nothing ever
            // removed a block-cache slot, and the file cache's budget was
            // `usize::MAX` because the one function that would have set it had
            // no callers. This drives the bounds that replaced that.
            //
            // `test-small-caches` is the actuator for the same reason
            // `xhci-one-slot` is: the shipped bounds are 16 MiB and 64 MiB on
            // this guest, and filling them by doing real I/O is minutes of
            // NVMe traffic to observe a policy that 256 KiB observes in a
            // second. The eviction code is the shipped code — only the number
            // moves, and the boot line below is what proves which number is in
            // force.
            // The T14's namespace, because the two caches are filled by
            // different things. File pages come from the guest program below;
            // metadata blocks come from the *device*, whose allocator bitmap
            // is one bit per block — 1900 blocks of it on a 244 GB namespace
            // against 8 on the 128 MiB one, which is the difference between
            // overflowing a 64-slot cache during the format and never
            // reaching it. Measured: 0 block-cache evictions on Headless.
            //
            // And it has to be an *unformatted* namespace, which is not what a
            // full run leaves behind: the image is named by device size and
            // reused within a lane, so a `nvme_large_device` that ran in this
            // one formatted it and this boot would then only mount — a handful
            // of metadata blocks and no eviction at all. Measured exactly that
            // way: green alone, red in the suite. Removing it restores the
            // precondition whichever lane this landed in, and duplicating the
            // harness's naming here is safe in the only direction that matters:
            // if that name ever drifts, the boot mounts instead of formatting
            // and the turnover assertion below goes red rather than vacuously
            // green.
            let stale = common::lane::dir()
                .join(format!("test-nvme-{}.img", qemu::NVME_T14_BYTES));
            let _ = fs::remove_file(&stale);

            // `nvme-spent-budget` rides this boot rather than buying a
            // registered name of its own: it needs the test kernel and a real
            // NVMe namespace, which is what this test already boots, and what
            // it costs is one refused read before anything mounts the device —
            // no command issued, no cache slot taken, and so nothing the
            // eviction series below can see.
            let options = BootOptions {
                profile: qemu::Profile::MetalDisk,
                kernel_params: &["test-small-caches", "nvme-spent-budget"],
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();

            // The NVMe half of `block::OPERATION`. `usb-storage-gate` asserts
            // the same refusal on the USB path; this is the one taken with both
            // page-cache locks held, which is what made a missing deadline a
            // wedged CPU rather than a slow read.
            //
            // Both lines, and the second is the one easy to leave out: a driver
            // that refused by abandoning a command in flight would pass the
            // first and fail here, because the queue would still be owed a
            // completion and the DMA window still owed a write.
            let console = serial::Serial::named("boot console", boot.as_str());
            console.must_say("nvme-gate: read with a spent budget refused=true")?;
            console.must_say("nvme-gate: the same block read afterwards ok=true")?;

            let Some(file_budget) = parse_cache_budget(&boot, "file cache: budget ") else {
                return Err(format!("the file cache printed no budget:\n{boot}"));
            };
            let Some(block_budget) = parse_cache_budget(&boot, "cached blocks, cap ") else {
                return Err(format!("the block cache printed no slot cap:\n{boot}"));
            };
            if file_budget != 64 || block_budget != 64 {
                return Err(format!(
                    "budgets are {file_budget} file pages and {block_budget} block slots, \
                     not the 64 each the feature asks for — the bound under test is not the \
                     one the workload was sized against:\n{boot}"
                ));
            }

            let result = qemu.run_test("test_rs_cache_eviction", Duration::from_secs(180));
            if !check_rust_result(&result) {
                return Err(format!(
                    "a page did not survive being evicted and re-read:\n{}\n{}",
                    result.stdout, result.serial
                ));
            }

            // The whole point, and the half a compile cannot fake: residency
            // is flat while the eviction count climbs. Boot and test output
            // both, since the block cache starts evicting during the format.
            let log = format!("{boot}\n{}", result.serial);
            let file_series = parse_cache_series(&log, "file cache: ", "pages resident");
            let block_series = parse_cache_series(&log, "page cache: ", "slots resident");

            for (what, series, budget) in [
                ("file cache", &file_series, file_budget),
                ("block cache", &block_series, block_budget),
            ] {
                // One turnover line means one eviction happened and nothing
                // more; the workload is 8x the budget in each cache, so a
                // series this short means eviction is not keeping up with the
                // pressure — or is not running at all.
                if series.len() < 4 {
                    return Err(format!(
                        "{what}: {} turnover lines, want at least 4 — {series:?}\n{log}",
                        series.len()
                    ));
                }
                for &(evictions, resident) in series {
                    if resident > budget {
                        return Err(format!(
                            "{what}: {resident} entries resident against a {budget} bound \
                             after {evictions} evictions — the bound does not hold:\n{log}"
                        ));
                    }
                }
                let (last, _) = series[series.len() - 1];
                let (first, _) = series[0];
                if last <= first {
                    return Err(format!("{what}: eviction count never advanced: {series:?}"));
                }
            }

            eprintln!(
                "  [cache] file {} evictions over {} turnovers, block {} evictions over {}; \
                 residency never above {file_budget}/{block_budget}",
                file_series[file_series.len() - 1].0,
                file_series.len(),
                block_series[block_series.len() - 1].0,
                block_series.len()
            );
            Ok(())
        }
        "xhci_slot_exhaustion" => {
            // A device count is untrusted input: more devices than the driver
            // has room for must cost those devices and nothing else. QEMU
            // cannot stage it — see XHCI_WIDE for why `slots=` is not the
            // actuator it looks like — so the kernel clamps itself to one
            // device block and the six-device bus does the rest. QEMU's Enable
            // Slot ignores MaxSlotsEn too, so the slot ids the controller hands
            // back really do run past the pool: this drives the driver's own
            // bound, not the controller's politeness.
            let options = BootOptions {
                profile: qemu::Profile::MetalUsb,
                kernel_params: &["xhci-one-slot"],
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            let usb = usb_argv(&argv);
            if usb.len() < 3 {
                return Err(format!("nothing to overflow with: {usb:?}"));
            }

            let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let log = qemu.boot_log().to_string();

            let Some(dma) = parse_xhci_layout(&log) else {
                return Err(format!("the driver printed no DMA layout line:\n{log}"));
            };
            if dma.blocks != 1 {
                return Err(format!("device blocks={}, want exactly 1: {dma:?}", dma.blocks));
            }
            // And it is the feature that bound it. A build where the ceiling
            // stopped reaching `Layout::new` reports the controller's own 64
            // here and drops nothing, which is a green test with no shortage
            // in it.
            if dma.cap_slots <= dma.blocks {
                return Err(format!(
                    "max_slots={} — there is no shortage to observe: {dma:?}",
                    dma.cap_slots
                ));
            }

            // Every device past the first is dropped, one line each.
            let slots = parse_xhci_slots(&log);
            let over = log.matches("beyond the pool").count();
            if over != usb.len() - 1 {
                return Err(format!(
                    "{over} devices dropped for want of a block, want {} (slots {slots:?}):\n{log}",
                    usb.len() - 1
                ));
            }
            if slots != [1] {
                return Err(format!("slots {slots:?} got a block, want just slot 1:\n{log}"));
            }

            // The one device that did get the block was enumerated to
            // completion, which is what makes "the extra devices and nothing
            // else" more than the absence of a panic. On this bus that device
            // is the boot stick — QEMU puts it on the controller's first
            // SuperSpeed port register, ahead of every USB2 one — so what it
            // proves is block 0's output context, its EP0 ring and its bulk
            // pair, not a HID's interrupt ring. A `dev_base` that overlapped
            // the shared head would put slot 1's device context on the command
            // ring and the next command would fail here.
            for bad in [
                "Enable Slot failed",
                "Address Device failed",
                "GET_DESCRIPTOR",
                "Configure Endpoint failed",
                "not enabled after reset",
            ] {
                if log.contains(bad) {
                    return Err(format!("{bad:?} on the one device that fit:\n{log}"));
                }
            }
            if !log.contains("xHCI: device addressed") {
                return Err(format!("slot 1 got a block and was never addressed:\n{log}"));
            }
            // And it was driven all the way to a disk. The device blocks are
            // what ran short, not the mass-storage blocks, so the one device
            // that fit has to come out the far end with a capacity.
            if log.matches("usb-storage: disk ").count() != 1 {
                return Err(format!("the stick that fit did not bind as a disk:\n{log}"));
            }
            if !log.contains("usb-storage: 1 device(s)") {
                return Err(format!("want exactly one disk, the stick that fit:\n{log}"));
            }
            serial::Serial::named("boot console", log.as_str()).must_be_clean()?;
            eprintln!(
                "  [xhci] 1 block of {} for {} devices, {over} dropped, slot 1 addressed",
                dma.stride,
                usb.len()
            );
            Ok(())
        }
        "ioapic_topology" => {
            // Everything the I/O APIC driver says happens in Phase 2, long
            // before the virtio-console exists, so the 16550 file is where a
            // host reads it. On the T14 the same lines land on the screen at
            // the next boot checkpoint; this is the QEMU-side equivalent.
            let qemu = QemuInstance::boot(test_config, c_bins, rust_bins);
            // The ready marker only proves the guest booted; the lines under
            // test were written before that, so nothing else to wait for.
            let log = qemu.boot_log().to_string();
            let units: Vec<&str> = log
                .lines()
                .filter_map(|l| l.split("ioapic: id=").nth(1))
                .collect();
            if units.is_empty() {
                return Err(format!("no `ioapic: id=` line in the boot log:\n{log}"));
            }
            // A window the machine does not decode answers 0xFFFFFFFF to
            // everything, which is a *valid-looking* unit: 256 entries, all
            // read back masked, `route` succeeds into nothing. The driver
            // drops such a unit, so its absence from the log is the assertion.
            if let Some(ignored) = log.lines().find(|l| l.contains("ioapic: id=") && l.contains("IGNORED")) {
                return Err(format!("an I/O APIC failed its plausibility gate: {ignored}"));
            }
            let mut covered: Vec<(u32, u32)> = Vec::new();
            for unit in &units {
                // `<id> at <addr> ver=<v> gsi <lo>..<hi> masked <n>/<total>`
                let ver = unit
                    .split_once(" ver=0x")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                    .ok_or_else(|| format!("no version in {unit:?}"))?;
                // Both halves of the entry count come from this register, so a
                // version that is not a chip's makes the count meaningless.
                if ver == 0x00 || ver == 0xFF {
                    return Err(format!("I/O APIC version {ver:#04x} is a floating bus: {unit:?}"));
                }
                let (range, masked) = unit
                    .split_once(" gsi ")
                    .and_then(|(_, rest)| rest.split_once(" masked "))
                    .ok_or_else(|| format!("unreadable I/O APIC line: {unit:?}"))?;
                let (lo, hi) = range
                    .split_once("..")
                    .ok_or_else(|| format!("no GSI range in {unit:?}"))?;
                let lo: u32 = lo.trim().parse().map_err(|_| format!("bad GSI base in {unit:?}"))?;
                let hi: u32 = hi.trim().parse().map_err(|_| format!("bad GSI top in {unit:?}"))?;
                let (n, total) = masked
                    .trim()
                    .split_once('/')
                    .ok_or_else(|| format!("no mask count in {unit:?}"))?;
                let n: u32 = n.parse().map_err(|_| format!("bad mask count in {unit:?}"))?;
                let total: u32 = total
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .parse()
                    .map_err(|_| format!("bad entry count in {unit:?}"))?;
                // `hi` is printed as `lo + total - 1`, so comparing them is a
                // tautology. What is checkable is the bound the driver refuses
                // past — a floating bus reports 256 here.
                if hi < lo || !(1..=240).contains(&total) {
                    return Err(format!(
                        "I/O APIC claims gsi {lo}..{hi}, {total} entries — not a redirection table: {unit:?}"
                    ));
                }
                covered.push((lo, hi));
                // The whole reason this driver runs before the first sti: an
                // entry firmware left armed at a vector with no gate is a #GP
                // that kills the boot.
                if n != total {
                    return Err(format!(
                        "{n} of {total} redirection entries masked — {} left armed: {unit:?}",
                        total - n
                    ));
                }
            }
            // Independent of any number the log derived from another: the two
            // pins the i8042 needs have to fall inside some unit's range, or
            // `route` returns `NoUnit` and there is no PS/2 input at all.
            for gsi in [1u32, 12] {
                if !covered.iter().any(|&(lo, hi)| (lo..=hi).contains(&gsi)) {
                    return Err(format!(
                        "no I/O APIC covers GSI {gsi}; units cover {covered:?}"
                    ));
                }
            }
            // IRQ 1 and IRQ 12 must be uncovered by the override table, or
            // the i8042 driver's identity assumption is wrong on this machine.
            let Some(isos) = log
                .lines()
                .find_map(|l| l.split("ioapic: iso bus:irq->gsi [").nth(1))
                .and_then(|r| r.split(']').next())
            else {
                return Err(format!("no `ioapic: iso` line in the boot log:\n{log}"));
            };
            // q35 always overrides at least IRQ 0, so an empty table means the
            // parse found nothing rather than that the machine has nothing.
            if isos.is_empty() {
                return Err(format!("the override table is empty; q35 always has IRQ 0:\n{log}"));
            }
            eprintln!("  [ioapic] {} unit(s), overrides {isos}", units.len());
            Ok(())
        }
        "control_regs" => {
            const CPUS: u32 = 4;
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { smp: CPUS, ..Default::default() },
            );
            control_regs(qemu.boot_log(), CPUS)
        }
        "control_regs_negative" => control_regs_negative(test_config, c_bins, rust_bins),
        "input_merge" => {
            // The check runs in the kernel and panics on mismatch, so a
            // failure arrives as a dead boot; the marker is the only proof it
            // ran at all.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["test-input-merge"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log();
            if !log.contains("input-merge: ok") {
                return Err(format!("the input core check never reported:\n{log}"));
            }
            Ok(())
        }
        "i8042_health_cadence" => {
            // The T14 lost keyboard, TrackPoint and touchpad — all three behind
            // this controller — 6.6 s into a session, and the driver's last
            // word on the subject was printed 15 ms *before* it happened. The
            // verdict was terminal, so for the remaining 54 s the log cannot
            // distinguish "the pin stopped asserting" from "bytes kept arriving
            // and decoded to nothing". Those are opposite defects in opposite
            // subsystems and the counters that separate them were read once.
            //
            // What is under test is not that a line appears. It is that its
            // *absence* means something: the report fires whenever the pin has
            // asserted since the last one, so no line means no interrupt. A
            // report that fired on a timer would satisfy every "is it alive"
            // search and answer nothing.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    kernel_params: &["i8042-fast-health"],
                    ..Default::default()
                },
            );
            if !qemu.boot_log().contains("i8042: kbd set2+xlat") {
                return Err(format!("the PS/2 keyboard never came up:\n{}", qemu.boot_log()));
            }
            // One key, a silence several periods long, then one more key. The
            // guest program holds the keyboard claim for 5 s and the period is
            // 500 ms, so the quiet stretch is nine periods with nothing to say.
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(30),
                I8042_READY,
                |socket| {
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                    thread::sleep(Duration::from_millis(3000));
                    qemu::qmp_send_keys(socket, &[("b", true), ("b", false)]);
                    thread::sleep(Duration::from_millis(1000));
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            let lines: Vec<&str> =
                result.serial.lines().filter(|l| l.contains("last byte at")).collect();
            // Two keystrokes, two lines. Not one — the verdict is not the
            // report and a driver that only ever spoke at boot would give one.
            // Not ten — a line per period through the quiet stretch is the
            // failure that makes silence unreadable, and it is the reason this
            // test injects a gap at all.
            if lines.len() != 2 {
                return Err(format!(
                    "two keystrokes three seconds apart, {} counter lines — the report is on a \
                     timer rather than on the pin:\n{}",
                    lines.len(),
                    lines.join("\n")
                ));
            }
            let last_byte_ms = |line: &str| -> Option<u64> {
                line.rsplit_once("last byte at ")?.1.trim_end_matches("ms").parse().ok()
            };
            let first = last_byte_ms(lines[0])
                .ok_or_else(|| format!("unreadable counter line: {}", lines[0]))?;
            let second = last_byte_ms(lines[1])
                .ok_or_else(|| format!("unreadable counter line: {}", lines[1]))?;
            // The second line is about the second keystroke, not a rerun of the
            // first. This is what dates the freeze on a machine whose log is
            // read hours later.
            if second <= first {
                return Err(format!(
                    "the second report dates the last byte at {second}ms, not after {first}ms — \
                     it is repeating a stale reading:\n{}",
                    lines.join("\n")
                ));
            }
            // A working keyboard owes none of the four fault counters.
            for want in ["0 discarded", "0 overruns", "0 dropped", "0 lost edges"] {
                if !lines[1].contains(want) {
                    return Err(format!("a healthy keyboard reports {want:?} wrong: {}", lines[1]));
                }
            }
            eprintln!("  [i8042] {}", lines[0].trim());
            eprintln!("  [i8042] {}", lines[1].trim());
            Ok(())
        }
        "i8042_health" => {
            // The failure mode that had no line at all: `init` arms the pin,
            // prints its green line, and nothing ever asserts. Two boots,
            // because the transition is the claim and one boot can only be on
            // one side of it — the first is never touched, the second is.
            //
            // Boot one waits on the verdict *as its ready marker*, so a driver
            // that never reaches it fails as a boot timeout naming the line it
            // waited for.
            let quiet_boot = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    ready_marker: "the pin has never asserted",
                    ..Default::default()
                },
            );
            let quiet_log = quiet_boot.boot_log().to_string();
            let Some(quiet) = quiet_log.lines().find(|l| l.contains("the pin has never asserted"))
            else {
                return Err(format!("no quiet verdict:\n{quiet_log}"));
            };
            // The counters, not the sentence.
            if !quiet.contains("0 interrupts") {
                return Err(format!("the quiet verdict does not say it saw none: {quiet}"));
            }
            // And nothing on this machine claimed the pin asserts, on a boot
            // where nothing touched the keyboard. A report that printed both
            // lines unconditionally would satisfy every search below.
            if let Some(wrong) = quiet_log.lines().find(|l| l.contains("the pin asserts")) {
                return Err(format!("the pin asserted with nothing to assert it: {wrong}"));
            }
            // Nor its mute twin, which is reached from the same `irqs > 0` gate
            // and would otherwise be a second line free to print on every boot.
            if let Some(wrong) = quiet_log.lines().find(|l| l.contains("nothing decoded")) {
                return Err(format!("bytes decoded to nothing with no bytes at all: {wrong}"));
            }
            drop(quiet_boot);

            // Boot two: the same kernel, one keystroke.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { profile: qemu::Profile::Metal, qmp: true, ..Default::default() },
            );
            if !qemu.boot_log().contains("i8042: kbd set2+xlat") {
                return Err(format!("the PS/2 keyboard never came up:\n{}", qemu.boot_log()));
            }
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                I8042_READY,
                |socket| {
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                    thread::sleep(Duration::from_millis(100));
                    send_i8042_sentinel(socket);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            let Some(line) = result.serial.lines().find(|l| l.contains("the pin asserts")) else {
                return Err(format!(
                    "a key was injected and the driver never said the pin asserts:\n{}",
                    result.serial
                ));
            };
            let words: Vec<&str> = line.split_whitespace().collect();
            let field = |name: &str| -> Option<u64> {
                let at = words.iter().position(|w| w.trim_end_matches(',') == name)?;
                words.get(at.checked_sub(1)?)?.parse().ok()
            };
            let irqs = field("interrupts")
                .ok_or_else(|| format!("unreadable health line: {line}"))?;
            let bytes = field("bytes").ok_or_else(|| format!("unreadable health line: {line}"))?;
            // The chain the line claims, end to end: the pin asserted, the ISR
            // read the port, and the decoder produced an event. Interrupts
            // alone would go green on a driver whose ring never filled.
            let keys = field("keys").ok_or_else(|| format!("unreadable health line: {line}"))?;
            if irqs == 0 || bytes == 0 || keys == 0 {
                return Err(format!(
                    "the alive line reports {irqs} interrupts, {bytes} bytes, {keys} keys: {line}"
                ));
            }
            // `verdict_due` keeps a CPU awake for one pass. If it ever failed to
            // self-clear, that CPU would spin instead of halting — the exact
            // failure the quarantine path already had once.
            let health = result.serial.matches("sched: cpu=").count();
            if health > 50 {
                return Err(format!(
                    "{health} idle-health lines — the health verdict is holding a CPU awake"
                ));
            }
            eprintln!("  [i8042] {}", quiet.trim());
            eprintln!("  [i8042] {}", line.trim());
            eprintln!("  [i8042] {health} idle-health lines — the CPU still halts");
            Ok(())
        }
        "operation_nesting" => {
            // **An inner `scheduler::Operation` may only narrow, and its drop
            // restores what it displaced.** `Operation::begin` stores
            // `outer.min(until)`, so a caller cannot buy itself more device
            // time by starting a second operation inside the first — which is
            // the failure `block::OPERATION` exists to stop, arriving one layer
            // lower.
            //
            // Nothing host-side can read it: the type reaches `percpu::cpu_id`
            // and `driver::current_handle`, and `kernel/` is excluded from the
            // host workspace, so a `Operation` cannot be constructed off a
            // booted machine. The other gates that drive an establishment —
            // `cache_eviction`'s `nvme-spent-budget`, `usb_storage_gate`'s —
            // prove a narrowing happened by the refusal it produces and read
            // none of the values, and all of them establish from a boot phase,
            // which is the *task-less* slot. `kernel/src/sched_gate.rs` runs
            // three nested establishments with known deadlines in both homes
            // and prints what every level saw.
            //
            // **The bound is derived here and not read off the kernel's
            // verdict**: the kernel prints what each level *asked* for and what
            // it *observed*, and this recomputes the running minimum. A kernel
            // that printed a verdict would be marking its own paper.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["sched-operation-nesting"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log().to_string();

            /// One `key=value` off a gate line, as a number.
            fn number(line: &str, key: &str) -> Result<u64, String> {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
                    .ok_or_else(|| format!("no numeric {key} in {line:?}"))
            }
            /// One `key=value` off a gate line, as a flag.
            fn flag(line: &str, key: &str) -> Result<bool, String> {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
                    .ok_or_else(|| format!("no boolean {key} in {line:?}"))
            }

            // Both homes. A task's word is on its `TaskHandle` and a context
            // with no task uses one slot per CPU, and the two are reached by
            // different arms of `operation_slot` — so a gate that ran in one
            // place would leave the other arm unexecuted by any test at all.
            for site in ["boot", "iod"] {
                let say = |what: &str| -> Result<String, String> {
                    let needle = format!("sched-op: {site} {what}");
                    log.lines()
                        .find(|line| line.contains(&needle))
                        .map(str::to_string)
                        .ok_or_else(|| format!("no {needle:?} line on this boot:\n{log}"))
                };

                let outside = say("outside")?;
                if flag(&outside, "established")? {
                    return Err(format!(
                        "{site}: an operation was already established before the gate began, \
                         so nothing below is about the nesting it made: {outside}"
                    ));
                }

                // Every level: what it asked for, and what the depth below it
                // recovered. The bound is the running minimum — an inner
                // establishment takes the earlier of its own deadline and its
                // parent's, and only that.
                let mut asked = Vec::new();
                let mut narrowest = u64::MAX;
                for level in 1..=3 {
                    let line = say(&format!("begin level={level}"))?;
                    let want = number(&line, "asked")?;
                    let saw = number(&line, "observed")?;
                    narrowest = narrowest.min(want);
                    if saw != narrowest {
                        return Err(format!(
                            "{site}: level {level} asked for {want} ns and the depth inside it \
                             recovered {saw} ns, against the {narrowest} ns that is the \
                             earliest of it and every level above it. An establishment that \
                             observes more than its parent allowed is a caller buying itself \
                             device time by nesting: {line}"
                        ));
                    }
                    asked.push(want);
                }
                // The widening attempt has to have been a real one, or the line
                // above is satisfied by a scenario in which nothing was asked.
                if asked[2] <= asked[1] {
                    return Err(format!(
                        "{site}: level 3 asked for {} ns inside a level 2 of {} ns, so the \
                         gate never attempted to widen and the narrowing it reports is vacuous",
                        asked[2], asked[1],
                    ));
                }

                // And the restore: each drop puts back what that establishment
                // displaced rather than clearing the slot, so the operation
                // above it survives the one below ending.
                for (level, restored) in [(3, asked[1].min(asked[0])), (2, asked[0])] {
                    let line = say(&format!("end level={level}"))?;
                    let saw = number(&line, "observed")?;
                    if saw != restored {
                        return Err(format!(
                            "{site}: with level {level} dropped the depth recovered {saw} ns \
                             and the frame above it established {restored} ns — a guard that \
                             restores something else has ended an operation its caller is \
                             still inside: {line}"
                        ));
                    }
                    if !flag(&line, "established")? {
                        return Err(format!(
                            "{site}: dropping level {level} left no operation established at \
                             all, and its caller is still inside one: {line}"
                        ));
                    }
                }

                let last = say("end level=1")?;
                if flag(&last, "established")? {
                    return Err(format!(
                        "{site}: the outermost guard dropped and an operation is still \
                         established — the slot was restored rather than cleared, so the next \
                         depth to ask would be answered a deadline nobody set: {last}"
                    ));
                }
                eprintln!(
                    "  [operation] {site}: {} ns narrowed to {} ns, a {} ns request changed \
                     nothing, and both drops restored",
                    asked[0], asked[1], asked[2],
                );
            }
            Ok(())
        }
        "virtio_used_ring" => {
            // Both fields of a virtqueue used-ring element are written by the
            // device, and on virtio-sound's control and event queues the ring
            // is inside a page a userland process maps writable. Every virtio
            // device QEMU implements writes correct elements and no device or
            // machine property makes one report a head descriptor it was never
            // given, so a boot certifies the correct case and nothing else.
            // The driver therefore runs the shipped `poll_used` over eleven
            // crafted elements at init under this parameter — a real queue on a
            // real DMA page, with the kernel writing the ring where the device
            // would.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_params: &["virtio-used-selftest"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log().to_string();
            if let Some(bad) = log.lines().find(|l| l.contains("used-ring selftest FAILED")) {
                return Err(format!("{bad}\n{log}"));
            }
            let Some(verdict) = log.lines().find(|l| l.contains("used-ring selftest")) else {
                return Err(format!("the parse's self-test never ran:\n{log}"));
            };
            // `11/11`, not "no failures": a self-test that ran zero cases would
            // satisfy the absence of a FAILED line just as well.
            if !verdict.contains("11/11") {
                return Err(format!("not every used-ring element was parsed as required: {verdict}"));
            }
            // Once for the machine. It touches no device, so a run per virtio
            // driver would be four verdicts about the same eleven elements.
            let ran = log.matches("used-ring selftest").count();
            if ran != 1 {
                return Err(format!("the self-test ran {ran} times, wanted once\n{log}"));
            }
            // And the legal direction, on the same boot and not by assertion:
            // this log arrived over virtio-console, whose TX path is
            // `submit_and_wait` around the same `poll_used`. A parse that
            // refused a correct element would have produced no capture to
            // search — but virtio-net says so in its own words, so that the
            // legal case is *named* rather than inferred from the test running
            // at all.
            if !log.contains("VirtIO net:") {
                return Err(format!("the NIC did not come up on this boot\n{log}"));
            }
            if let Some(bad) = log.lines().find(|l| l.contains("refused") && l.contains("RX used-ring")) {
                return Err(format!("a correct completion was refused on the ordinary path: {bad}"));
            }
            eprintln!("  [virtio] {}", verdict.trim());
            Ok(())
        }
        "xhci_descriptor_walk" => {
            // A configuration descriptor is the device's, and a device is not
            // kernel code. Every device QEMU can attach describes itself
            // correctly, so a boot certifies that the parser handles a correct
            // descriptor and nothing else — while the interesting inputs are
            // the wrong ones, and one of them is an endpoint address naming
            // endpoint 0, whose device context index is the slot context or
            // EP0's. The parser is pure, so the driver runs it over nine
            // crafted descriptors at init under this feature.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    kernel_params: &["xhci-descriptor-selftest"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log().to_string();
            if let Some(bad) = log.lines().find(|l| l.contains("descriptor selftest FAILED")) {
                return Err(format!("{bad}\n{log}"));
            }
            let Some(verdict) = log.lines().find(|l| l.contains("descriptor selftest")) else {
                return Err(format!("the parser's self-test never ran:\n{log}"));
            };
            // `9/9`, not "no failures": a self-test that ran zero cases would
            // satisfy the absence of a FAILED line.
            if !verdict.contains("9/9") {
                return Err(format!("not every descriptor was parsed as required: {verdict}"));
            }
            // Once for the machine. It reads no register, so a per-controller
            // run would be two verdicts about the same nine byte arrays.
            let ran = log.matches("descriptor selftest").count();
            if ran != 1 {
                return Err(format!("the self-test ran {ran} times, wanted once\n{log}"));
            }
            // And the ordinary boot beside it: the same parser bound the boot
            // stick off a descriptor a real controller delivered.
            if !log.contains("usb-storage: 1 device(s)") {
                return Err(format!("the boot stick did not bind on this boot\n{log}"));
            }
            eprintln!("  [xhci] {}", verdict.trim());
            Ok(())
        }
        "xhci_xecp_walk" => {
            // The xHCI extended-capability list is firmware's, and firmware is
            // not kernel code. QEMU's controller publishes a list with no USB
            // Legacy Support capability in it, so a boot certifies exactly one
            // thing: the walk runs on a real controller and terminates. Every
            // way the list can be *wrong* — a pointer out of the register
            // window, a chain that never ends, a window reading all ones — is
            // a shape no controller in reach produces, so the driver walks
            // eight of them at init under this feature and says how many it
            // refused.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    kernel_params: &["xhci-xecp-selftest"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log().to_string();
            if let Some(bad) = log.lines().find(|l| l.contains("xecp selftest FAILED")) {
                return Err(format!("{bad}\n{log}"));
            }
            let Some(verdict) = log.lines().find(|l| l.contains("xecp selftest")) else {
                return Err(format!("the walk's self-test never ran:\n{log}"));
            };
            // `8/8`, not "no failures": a self-test that ran zero cases would
            // satisfy the absence of a FAILED line.
            if !verdict.contains("8/8") {
                return Err(format!("not every malformed list was refused: {verdict}"));
            }
            // And the walk on the controller QEMU does provide.
            let Some(real) = log
                .lines()
                .find(|l| l.contains("USB Legacy Support") || l.contains("ownership"))
            else {
                return Err(format!("no line about the handoff at all:\n{log}"));
            };
            // The handoff must precede the reset — a reset that already
            // happened is what the whole capability exists to avoid.
            let reset = log
                .find("xHCI: controller reset")
                .ok_or_else(|| format!("the controller was never reset:\n{log}"))?;
            let handoff = log.find(real).expect("just found");
            if handoff > reset {
                return Err(format!(
                    "the ownership handoff runs after HCRST, which is no handoff at all:\n{log}"
                ));
            }
            // A controller that still enumerates its bus afterwards.
            if !log.contains("xHCI: controller started") {
                return Err(format!("the controller did not come up:\n{log}"));
            }
            eprintln!("  [xhci] {}", verdict.trim());
            eprintln!("  [xhci] {}", real.trim());
            Ok(())
        }
        "i8042_budget_expiry" => {
            // The arithmetic defect this feature stages: stage budgets summing
            // past the total they clamp to. With the total spent before the
            // probe starts, every wait below returns immediately on a
            // controller that is answering perfectly — which is what a slow EC
            // looks like from inside the driver, and what used to surface as
            // `DISABLED — cfg … did not take`, a controller fault.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    kernel_params: &["i8042-budget-expired"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log().to_string();
            let Some(line) = log.lines().find(|l| l.contains("init budget")) else {
                return Err(format!(
                    "the budget was spent before the probe began and nothing said so:\n{log}"
                ));
            };
            // Naming the stage is the whole point: "it timed out" is not a
            // diagnosis on a machine that cannot be single-stepped.
            const STAGES: &[&str] = &["self-test", "keyboard", "aux reset", "the pin could be armed"];
            if !STAGES.iter().any(|s| line.contains(s)) {
                return Err(format!(
                    "a budget expiry that does not name what ran out: {line}"
                ));
            }
            // And it must not still be wearing a controller fault's clothes.
            if let Some(wrong) = log.lines().find(|l| l.contains("did not take")) {
                return Err(format!(
                    "a timeout still reports as a controller fault: {wrong}"
                ));
            }
            // Losing the keyboard must not cost the boot.
            if boot_millis(&log).is_none() {
                return Err(format!("the boot did not finish:\n{log}"));
            }
            eprintln!("  [i8042] {}", line.trim());
            Ok(())
        }
        "i8042_fadt_denial" => {
            // The T14's verdict, reproduced: firmware says there is no 8042 and
            // there is one. `i8042-fadt-denial` hands the probe the laptop's own
            // FADT answer — revision 6, iapc_boot_arch=0x0011 — on QEMU's
            // working controller, because QEMU cannot stage the disagreement
            // itself: it derives the bit from the presence of the device.
            //
            // Delivery to userland is the assertion, not the log line. "The
            // driver attached" is what a gate removal is supposed to produce;
            // "the keys arrive" is what it is *for*, and only the second one
            // fails if some later step believes the claim instead.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_params: &["i8042-fadt-denial"],
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();
            // Revision 6 is what proves the substitution took: QEMU's own FADT
            // is revision 3, so this line cannot be the machine's.
            let want_claim = "FADT rev 6 iapc_boot_arch=0x0011, bit 1 (8042) clear";
            let Some(claim) = boot.lines().find(|l| l.contains(want_claim)) else {
                return Err(format!("the probe was never handed a denial:\n{boot}"));
            };
            if !boot.contains("i8042: kbd set2+xlat (readback 0x41)") {
                return Err(format!(
                    "firmware denied the controller and the driver believed it:\n{boot}"
                ));
            }
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                I8042_READY,
                |socket| {
                    for key in ["h", "e", "l", "l", "o"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                    send_i8042_sentinel(socket);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            let typed: String = parse_key_events(&result.stdout)
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?} — the keyboard firmware denied does not reach userland"
                ));
            }
            eprintln!("  [i8042] {}", claim.trim());
            eprintln!("  [i8042] typed {typed:?} through a controller firmware denied");
            Ok(())
        }
        "i8042_kbd_echo" => {
            // The T14's second answer, reproduced: a healthy controller whose
            // keyboard will not report its scancode set. `i8042-kbd-echo`
            // answers the `0xF0 0x00` argument byte with `0xEE` — ECHO's own
            // reply, the byte the laptop printed — because QEMU's PS/2 keyboard
            // implements the command and nothing on the host side turns that
            // off.
            //
            // Two assertions, and the second is the one with teeth. The log
            // line proves the driver took the *assumed* branch rather than
            // reading the set: it names the byte, and its parenthetical is not
            // `readback 0x41`, so a driver that quietly kept reading the set
            // would fail here even though the keyboard works. Typing "hello"
            // through to a userland process proves the branch delivers, which
            // no log line can: a driver that logs the assumption and then
            // refuses, or that arms a pin nothing decodes, is green on the
            // first assertion alone.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_params: &["i8042-kbd-echo"],
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();
            let want = "0xF0 0x00 answered 0xee";
            let Some(refusal) = boot.lines().find(|l| l.contains(want)) else {
                return Err(format!("the keyboard never refused the set query:\n{boot}"));
            };
            let Some(attached) =
                boot.lines().find(|l| l.contains("i8042: kbd set2+xlat (assumed,"))
            else {
                return Err(format!("the driver refused the keyboard outright:\n{boot}"));
            };
            if boot.contains("(readback 0x41)") {
                return Err(format!(
                    "the injection did not take: the driver still read the set back:\n{boot}"
                ));
            }
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                I8042_READY,
                |socket| {
                    for key in ["h", "e", "l", "l", "o"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                    send_i8042_sentinel(socket);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            let typed: String = parse_key_events(&result.stdout)
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?} — a keyboard that will not report its set does not reach \
                     userland"
                ));
            }
            // The TrackPoint is on the far side of the keyboard block, so a
            // refusal that returns costs the pointer too. It must not here.
            if !boot.contains("i8042: aux rate=100") {
                return Err(format!("the aux port never came up behind the refusal:\n{boot}"));
            }
            eprintln!("  [i8042] {}", refusal.trim());
            eprintln!("  [i8042] {}", attached.trim());
            eprintln!("  [i8042] typed {typed:?} on a keyboard that will not report its set");
            Ok(())
        }
        "i8042_undecoded_bytes" => {
            // The T14 said `1 interrupts, 1 bytes, 0 keys, 0 motion` and the
            // counters could not name a suspect: 84 of the 256 single byte
            // values decode to nothing under set 1, so the same arithmetic
            // covers an extended key's harmless `0xE0` prefix, a `0xAA` from a
            // keyboard that reset, a late `0xFA`, and a wire carrying raw
            // set 2. Only the byte separates them.
            //
            // Pause is the injection because it is the one key whose whole
            // sequence decodes to nothing by design — `E1 1D 45 E1 9D C5`,
            // swallowed to keep the stream in frame — so bytes-with-zero-events
            // is reproduced without a kernel feature and without depending on
            // how the drain happens to batch. Then one plain letter, which is
            // the other half: the first line must not be the last word on a
            // keyboard that works.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { profile: qemu::Profile::Metal, qmp: true, ..Default::default() },
            );
            if !qemu.boot_log().contains("i8042: kbd set2+xlat") {
                return Err(format!("the PS/2 keyboard never came up:\n{}", qemu.boot_log()));
            }
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                I8042_READY,
                |socket| {
                    qemu::qmp_send_keys(socket, &[("pause", true), ("pause", false)]);
                    thread::sleep(Duration::from_millis(200));
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                    thread::sleep(Duration::from_millis(100));
                    send_i8042_sentinel(socket);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            // **Both lines are read from the injection onwards**, and that is
            // not tidiness: this driver reports on its own bring-up too, and a
            // `nothing decoded` line from before the Pause was pressed is not a
            // report about the Pause. Reading the first one in the whole capture
            // is what made this test red on a line naming no byte, on the dev
            // host and on CI
            // (`issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md`).
            // The marker is the boundary the test knows, because the marker is
            // what the injection was timed off.
            let capture = serial::Serial::named("i8042 capture", result.serial);
            let mute = capture.must_say_after(I8042_READY, "nothing decoded").map_err(|why| {
                format!("bytes arrived and decoded to nothing and the driver never said so: {why}")
            })?;
            // The datum, not the count. `0xE1` is Pause's prefix and the first
            // byte of the sequence whichever way the drain batched it; a line
            // that reports only "N bytes, 0 keys" is the one this test exists
            // to reject.
            if !mute.contains("no event from [0xe1") {
                return Err(format!("the line names no byte: {mute}"));
            }
            // And the picture corrects itself. A one-shot report would freeze
            // the panel on the half-arrived sequence and never say the
            // keyboard works after all — which on the T14 is a reflash.
            let alive = capture.must_say_after(I8042_READY, "the pin asserts").map_err(|why| {
                format!(
                    "a letter was typed after the undecoded bytes and the driver never \
                     revised its verdict: {why}"
                )
            })?;
            let keys = alive
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(2)
                .find(|w| w[1].trim_end_matches(',') == "keys")
                .and_then(|w| w[0].parse::<u64>().ok())
                .ok_or_else(|| format!("unreadable alive line: {alive}"))?;
            if keys == 0 {
                return Err(format!("the revised verdict still decodes nothing: {alive}"));
            }
            eprintln!("  [i8042] {}", mute.trim());
            eprintln!("  [i8042] {}", alive.trim());
            Ok(())
        }
        "i8042_absent" => {
            // A/B in one session: the guest's own `Boot: complete (Nms)` is
            // the instrument, because host-side timing here is dominated by
            // image builds. A wait-loop bug that costs a second on a machine
            // with a controller costs a minute on one without.
            let with = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { profile: qemu::Profile::Metal, ..Default::default() },
            );
            let with_log = with.boot_log().to_string();
            let with_ms = boot_millis(&with_log)
                .ok_or_else(|| format!("no `Boot: complete` line:\n{with_log}"))?;
            drop(with);

            let without = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    i8042: false,
                    ..Default::default()
                },
            );
            let log = without.boot_log().to_string();
            // Measured: `-machine q35,i8042=off` also clears the FADT
            // IAPC_BOOT_ARCH 8042 bit. That used to make this test certify the
            // gate; now it is what makes it certify the opposite — firmware
            // denies the controller, the driver probes anyway, and the
            // *handshake* is what refuses. Both halves are asserted, because a
            // refusal on the right machine for the wrong reason is exactly the
            // false pass available here.
            let Some(claim) = log.lines().find(|l| l.contains("iapc_boot_arch")) else {
                return Err(format!("the driver never said what firmware claimed:\n{log}"));
            };
            if !claim.contains("bit 1 (8042) clear") {
                return Err(format!(
                    "`-machine q35,i8042=off` no longer clears the FADT bit, so this \
                     configuration no longer stages a firmware denial: {claim}"
                ));
            }
            // The floating bus, not any of the sixteen handshake refusals: on a
            // machine with nothing there the probe must cost one `inb`, and
            // that is also what makes the timing assertion below tight.
            let want = "i8042: absent — port 0x64 reads 0xff";
            if !log.contains(want) {
                return Err(format!("no `{want}` line on a machine with no i8042:\n{log}"));
            }
            let without_ms = boot_millis(&log)
                .ok_or_else(|| format!("no `Boot: complete` line:\n{log}"))?;
            // The regression this guards is 2100 ms: with no floating-bus test
            // the very first `wait_writable` sees IBF set in 0xff and waits out
            // the whole init budget. The allowance is for boot-to-boot noise
            // between two QEMU launches in one session, nothing else.
            if without_ms > with_ms + 300 {
                return Err(format!(
                    "boot took {without_ms}ms without an i8042 and {with_ms}ms with one — a wait is not bounded"
                ));
            }
            eprintln!("  [i8042] firmware: {}", claim.trim());
            eprintln!(
                "  [i8042] {}",
                log.lines().find(|l| l.contains(want)).unwrap_or_default().trim()
            );
            eprintln!("  [i8042] boot {without_ms}ms without vs {with_ms}ms with");
            Ok(())
        }
        "i8042_quarantine" => {
            // A controller producing bytes faster than the ISR's bound can
            // drain them is the one case the bound alone still lets livelock
            // a CPU. It must cost a keyboard, not a CPU.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    // `sched-fast-health` shortens the idle-trip print from
                    // 10 s to 200 ms: comparing two samples is how a spinning
                    // CPU is told from a halting one, and this test's whole
                    // capture is a handful of seconds — shorter than one
                    // shipped period, let alone two.
                    kernel_params: &["i8042-fault", "sched-fast-health"],
                    ..Default::default()
                },
            );
            if !qemu.boot_log().contains("i8042: fault injection armed") {
                return Err(format!(
                    "the fault was never armed — did init fail?\n{}",
                    qemu.boot_log()
                ));
            }
            // The in-guest reader keeps a CPU doing work, so a livelocked
            // one is visible as a dead test rather than as a quiet pass.
            //
            // No sentinel here: the log below shows quarantine landing within
            // milliseconds of `===I8042_READY===`, before a host round trip
            // could possibly deliver anything, and quarantine masks the GSI —
            // so nothing sent afterward, sentinel included, ever reaches the
            // guest. This is `test_rs_i8042_keyboard`'s fallback deadline by
            // design, not a lost sentinel.
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(30),
                I8042_READY,
                |socket| {
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("the guest did not survive the wedge: {err}"));
            }
            let Some(line) = result.serial.lines().find(|l| l.contains("i8042: quarantined"))
            else {
                return Err(format!("no quarantine line:\n{}", result.serial));
            };
            // The count the driver actually achieved, not the word "masked"
            // in a format string: a quarantine that does not take the line
            // down leaves the CPU exposed to the next flood.
            let masked: u32 = line
                .split("masked=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("unreadable quarantine line: {line}"))?;
            if masked == 0 {
                return Err(format!("quarantined without masking any line: {line}"));
            }
            // "A keyboard, not a CPU" is the claim, so measure the CPU. The
            // first version of this driver left the `irq_ring` record
            // undrained after quarantine and produced 2685 idle-health lines
            // in 5 s, against 1 on a healthy run — a regression this exact
            // shape would no longer trip a *count of lines* now that
            // `log_health` prints at a fixed rate whether the CPU behind it
            // is halting or spinning (`issues/kernel/
            // i8042-quarantine-health-line-count-is-vacuous.md`). What still
            // moves at two different speeds is the `trips=` counter inside
            // each line, which is not rate-limited.
            if let Some((cpu, delta)) = idle_is_spinning(&result.serial) {
                return Err(format!(
                    "cpu{cpu}'s idle-trip counter moved by {delta} within the capture — spinning, not halting"
                ));
            }
            eprintln!("  [i8042] {}", line.trim());
            eprintln!("  [i8042] idle-trip counters stayed sane — the CPU still halts");
            Ok(())
        }
        "metal_sim_window_drag" => metal_sim_window_drag(rust_bins),
        "metal_sim_pointer_churn" => {
            // The owner froze his desktop twice by plugging a mouse in and
            // pulling it out again, and the second freeze landed on the fourth
            // cycle's enumeration. The compositor holds the merged pointer's
            // handle across all of it, so every cycle is a source binding and
            // releasing underneath a claim it never made and cannot see.
            //
            // The liveness signal is `compositor: frames=`, for the reason it
            // was built: it comes from a composited frame, so its absence is a
            // desktop that stopped drawing rather than an instrument that
            // stopped counting.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;

            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let socket = qemu.qmp_socket().to_path_buf();
            let mut console = qemu.boot_log().to_string();
            let frames = |text: &str| text.matches("compositor: frames=").count();

            // A baseline first: churn against a compositor that was never
            // drawing would be a green run proving nothing.
            let deadline = std::time::Instant::now() + qemu::budget(Duration::from_secs(20));
            while std::time::Instant::now() < deadline && frames(&console) < 1 {
                console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
            if frames(&console) < 1 {
                return Err(format!("the compositor never composited a frame:\n{console}"));
            }
            let before = frames(&console);

            // The owner's cadence: plugged for a second or two, unplugged for
            // about as long, over and over. His freeze came on the fourth.
            const CYCLES: usize = 8;
            const SETTLE: Duration = Duration::from_millis(400);
            for cycle in 0..CYCLES {
                let id = format!("churn{cycle}");
                // One monitor at a time — a `server` socket serves one
                // connection — so each phase opens, acts and closes.
                let mut devices = qemu::QmpDevices::open(&socket);
                devices.add("usb-mouse", "xhci.0", &id, &[]);
                drop(devices);
                console.push_str(&qemu.drain_serial(SETTLE));
                // The pointer has to be *used* between binding and unbinding.
                // A source that binds and goes is a lifecycle event the
                // compositor may never look at; a source delivering motion
                // when it goes is one the compositor is reading from, which
                // is the state the owner's machine was in every time.
                let mut input = qemu::QmpInput::open(&socket);
                for step in 0..16 {
                    let dir = if step % 2 == 0 { 12 } else { -12 };
                    input.mouse(dir, dir, None);
                }
                drop(input);
                console.push_str(&qemu.drain_serial(SETTLE));
                let mut devices = qemu::QmpDevices::open(&socket);
                devices.del(&id);
                drop(devices);
                console.push_str(&qemu.drain_serial(SETTLE));
            }

            // The churn has to have reached the guest, or this gate is a
            // twenty-second sleep with an assertion after it.
            //
            // **Waited for rather than slept for.** The three `SETTLE` drains
            // pace the *host* through one cycle; whether the guest's console has
            // caught up by the last of them is a fact about how fast the machine
            // is. On a KVM runner it had not — the last two cycles' bindings were
            // still on their way out when the count was taken, and the test read
            // six of eight as a driver that missed them (run `31246245541`).
            // The assertion is the same one; what
            // changed is that a console behind the guest costs wall clock instead
            // of a verdict.
            let bindings = |text: &str| text.matches("merges as source").count();
            let deadline = std::time::Instant::now() + qemu::budget(Duration::from_secs(20));
            while std::time::Instant::now() < deadline && bindings(&console) < CYCLES {
                console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
            let bound = bindings(&console);
            if bound < CYCLES {
                return Err(format!(
                    "{CYCLES} plug/unplug cycles bound {bound} pointer sources — the churn did \
                     not reach the kernel, so nothing here was tested:\n{console}"
                ));
            }

            // And the motion reached the compositor, or the churn was against
            // a pointer nobody was reading. An idle desktop composites twice
            // per reporting interval (the taskbar's clock); anything above
            // that is the cursor being moved.
            let moved = console
                .lines()
                .filter_map(|l| l.split("compositor: frames=").nth(1))
                .filter_map(|rest| rest.split_whitespace().next())
                .filter_map(|n| n.parse::<u64>().ok())
                .any(|frames| frames > 2);
            if !moved {
                return Err(format!(
                    "no reporting interval composited more than the taskbar's two frames — the \
                     injected motion never reached the compositor, so the churn was against a \
                     pointer it was not reading:\n{console}"
                ));
            }

            // Still painting, counted from here rather than from the boot: the
            // reporting interval is 2 s, so two of them cannot be satisfied by
            // frames the compositor produced before the first cycle.
            let mut after = String::new();
            let deadline = std::time::Instant::now() + qemu::budget(Duration::from_secs(20));
            while std::time::Instant::now() < deadline && frames(&after) < 2 {
                after.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
            if frames(&after) < 2 {
                return Err(format!(
                    "the compositor composited {before} frame batches before {CYCLES} pointer \
                     plug/unplug cycles and {} in the 20 s after them — the desktop stopped:\
                     \n{console}\n--- after ---\n{after}",
                    frames(&after)
                ));
            }

            let console = format!("{console}\n{after}");
            serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
            eprintln!(
                "  [metal-sim] {CYCLES} pointer plug/unplug cycles, {bound} source bindings, \
                 desktop still compositing"
            );
            Ok(())
        }
        "sshd_fail_closed" => {
            // sshd with a network under it — the only boot that gets past its
            // bind. What that reaches for the first time is the daemon's own
            // state on disk: the identity it mints under `/home`, and the file
            // it authenticates against.
            //
            // The verdict is that it authenticates nobody and says which file
            // left it that way. A daemon that cannot accept any key must not
            // be holding port 22, so "never listened" is asserted too — that
            // is the half a missing-file check would still pass without.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sshdcase");
            let options = BootOptions {
                profile: qemu::Profile::Headless,
                ..Default::default()
            };
            if !qemu::profile_argv(&options).iter().any(|a| a.contains("virtio-net")) {
                return Err("this test needs a NIC and the profile has none".to_string());
            }

            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            let mut console = qemu.boot_log().to_string();

            // Minting proves `/home/root/.ssh` is creatable and writable from
            // userland; the fingerprint proves the key it wrote reads back.
            const WANT: [&str; 3] = [
                "sshd: minted a new host identity at /home/root/.ssh/host_ed25519",
                "sshd: host identity SHA256:",
                "sshd: cannot read /home/root/.ssh/authorized_keys",
            ];
            let stalled =
                await_guest(&mut qemu, &mut console, "every line sshd owes", |c| {
                    WANT.iter().all(|w| c.contains(w))
                })
                .err();
            if let Some(why) = stalled {
                eprintln!("  [sshd] {why}");
            }
            for want in WANT {
                if !console.contains(want) {
                    return Err(format!("{want:?} never reached the console:\n{console}"));
                }
            }
            if console.contains("sshd: listening on port 22") {
                return Err(format!(
                    "sshd listened on port 22 with no key it could ever accept:\n{console}"
                ));
            }
            eprintln!(
                "  [sshd] host identity minted under /home, and no authorized_keys file \
                 left it refusing to listen at all"
            );
            Ok(())
        }
        "netd_connection_caps" => {
            // The only boot that runs netd at all. Its `main` opens the NIC
            // first and returns on `NotFound`, so metal-sim never reaches a
            // line of the daemon, and `tests/testcases` does not build netd —
            // between them a full suite run contained zero `netd:` lines and
            // the daemon's bound had no evidence behind it whatsoever.
            //
            // Same assertion design as `metal_sim_window_caps`: netd announces
            // the cap it derived, the guest measures where the refusals start,
            // and these must be the same number.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/netcase");
            let bins: Vec<(String, Vec<u8>)> = rust_bins
                .iter()
                .filter(|(name, _)| name == "netd_caps")
                .cloned()
                .collect();
            if bins.is_empty() {
                return Err("netd_caps was not built".to_string());
            }
            // Headless is the profile with virtio-net; without a NIC netd
            // exits before reaching anything this test is about.
            let options = BootOptions {
                profile: qemu::Profile::Headless,
                ..Default::default()
            };
            if !qemu::profile_argv(&options).iter().any(|a| a.contains("virtio-net")) {
                return Err("this test needs a NIC and the profile has none".to_string());
            }

            let mut qemu = QemuInstance::boot_with_options(&config, &[], &bins, options);

            let mut console = qemu.boot_log().to_string();
            let _ = await_marker(&mut qemu, &mut console, "netd: ready, at most ", "netd to come up");
            let Some(declared) = console
                .lines()
                .find_map(|l| l.split("netd: ready, at most ").nth(1))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                return Err(format!(
                    "netd never said how many piped connections it would hold:\n{console}"
                ));
            };
            if declared == 0 {
                return Err("netd derived a cap of zero connections".to_string());
            }

            // The cap is passed as the burst size, not as the answer: the
            // guest still measures the boundary itself.
            let result = qemu.run_test(
                &format!("test_rs_netd_caps {declared}"),
                Duration::from_secs(120),
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            if result.exit_code != Some(0) {
                return Err(format!(
                    "netd_caps exited {:?}:\n{}",
                    result.exit_code, result.stdout
                ));
            }

            let Some(granted) = result
                .stdout
                .split("netd caps: ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                return Err(format!("netd_caps printed no count:\n{}", result.stdout));
            };
            if granted != declared {
                return Err(format!(
                    "netd declared a cap of {declared} piped connections and accepted \
                     {granted} — the derivation and the enforcement disagree:\n{}",
                    result.stdout
                ));
            }
            eprintln!("  [netcase] netd cap {declared} piped connections, {granted} accepted then refused");
            Ok(())
        }
        "netd_hostile_peer" => {
            // The netcase boot again, and for the same reason: netd's `main`
            // returns on a machine with no NIC, so this is the only config
            // where there is a daemon to be hostile to.
            //
            // The guest carries every verdict that needs a deadline on it —
            // only it can tell a netd that answered from a netd that never
            // did. The host carries the half the guest cannot see: whether
            // netd *named* what it got rid of. A daemon that drops clients
            // silently is one this machine cannot be asked about afterwards,
            // which is the whole argument for the log lines.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/netcase");
            let bins: Vec<(String, Vec<u8>)> = rust_bins
                .iter()
                .filter(|(name, _)| name == "netd_hostile_peer")
                .cloned()
                .collect();
            if bins.is_empty() {
                return Err("netd_hostile_peer was not built".to_string());
            }
            let options = BootOptions {
                profile: qemu::Profile::Headless,
                ..Default::default()
            };
            if !qemu::profile_argv(&options).iter().any(|a| a.contains("virtio-net")) {
                return Err("this test needs a NIC and the profile has none".to_string());
            }

            let mut qemu = QemuInstance::boot_with_options(&config, &[], &bins, options);
            let mut console = qemu.boot_log().to_string();
            let _ = await_marker(&mut qemu, &mut console, "netd: ready, at most ", "netd to come up");
            if !console.contains("netd: ready, at most ") {
                return Err(format!("netd never came up on a machine with a NIC:\n{console}"));
            }

            let result = qemu.run_test("test_rs_netd_hostile_peer", Duration::from_secs(120));
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            if result.exit_code != Some(0) {
                return Err(format!(
                    "netd_hostile_peer exited {:?}:\n{}",
                    result.exit_code, result.stdout
                ));
            }

            // The guest's own case list, restated here so a case deleted on
            // one side is a red run rather than a quieter test.
            const CASES: usize = 6;
            let Some(refused) = result
                .stdout
                .split("hostile peer: ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                return Err(format!(
                    "netd_hostile_peer printed no count:\n{}",
                    result.stdout
                ));
            };
            if refused != CASES {
                return Err(format!(
                    "netd refused {refused} malformed frames, not {CASES}:\n{}",
                    result.stdout
                ));
            }

            // `TestResult::serial` is everything the console carried while the
            // guest ran, netd's own lines included — the daemon and the test
            // share one window (`issues/build/`), which here is what makes the
            // daemon's side of the story readable at all.
            console.push_str(&result.serial);
            for named in ["netd: dropping client", "netd: refusing client"] {
                if !console.contains(named) {
                    return Err(format!(
                        "netd got rid of clients without a `{named}` line — a daemon that \
                         drops peers silently cannot be asked what happened:\n{console}"
                    ));
                }
            }
            serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
            eprintln!("  [netcase] {refused} hostile frames refused, netd named every peer it dropped");
            Ok(())
        }
        "launcher_refusals" => {
            // **`/bin/init` is the one process the machine cannot lose**, and
            // every launcher client — the compositor, every terminal, every
            // shell, sshd — can send it whatever it likes. The guest carries
            // the verdicts: init answered, init is still launching, and the
            // kernel's live-object count did not grow across sixteen refused
            // launches. The host carries the one the guest cannot see —
            // whether init said anything about what it refused.
            //
            // `tests/netcase` because its test-runner is the only one that
            // receives a `launcher` connector, and because two boot programs
            // is the smallest blast radius for a test whose whole subject is
            // making init misbehave.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/netcase");
            let bins: Vec<(String, Vec<u8>)> = rust_bins
                .iter()
                .filter(|(name, _)| name == "launcher_refusals")
                .cloned()
                .collect();
            if bins.is_empty() {
                return Err("launcher_refusals was not built".to_string());
            }
            let mut qemu = QemuInstance::boot_with_options(
                &config,
                &[],
                &bins,
                BootOptions {
                    profile: qemu::Profile::Headless,
                    // The live-object count is a `SYS_DEBUG` action, and a
                    // shipping kernel has none: both readings would be the same
                    // `InvalidArgument` and the leak arm would pass having
                    // counted nothing.
                    kernel_features: ACTUATOR_KERNEL,
                    ..Default::default()
                },
            );
            let mut console = qemu.boot_log().to_string();
            let _ = await_marker(&mut qemu, &mut console, "===READY===", "test-runner to come up");

            let result = qemu.run_test("test_rs_launcher_refusals", Duration::from_secs(120));
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            if result.exit_code != Some(0) {
                return Err(format!(
                    "launcher_refusals exited {:?}:\n{}",
                    result.exit_code, result.stdout
                ));
            }
            console.push_str(&result.serial);
            if !console.contains("init: launcher: cannot start") {
                return Err(format!(
                    "init refused a launch without a line saying so — a launcher that \
                     drops requests silently cannot be asked what happened:\n{console}"
                ));
            }
            serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
            eprintln!("  [netcase] init refused three bad launches, named them, and kept launching");
            Ok(())
        }
        "metal_sim_input" => {
            // M2's exit criterion, on the machine shape and the kernel that
            // get flashed: no virtio device, no USB HID — so the i8042 is the
            // guest's only input device — and no kernel feature turned on for
            // the occasion, unlike the four tests above it.
            //
            // What it asserts is the events, read by an in-guest process and
            // printed. The first version asserted screen pixels after a click
            // at a fixed taskbar coordinate, which made the compositor's
            // layout part of a kernel-delivery criterion and needed thresholds
            // to survive the taskbar's own once-a-second repaint. M2 owns
            // delivery — pin to userland process — so that is what this
            // measures, and nothing here says the compositor reacted.
            // `metal_sim_compositor` is what covers the compositor.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            metal_sim_argv_check(&argv)?;
            if argv.iter().any(|a| a.contains("i8042=off")) {
                return Err("metal-sim turned the i8042 off".to_string());
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);

            // `kernel/src/mouse.rs` scales each relative count into the
            // 0..32767 space the compositor consumes, per axis and derived
            // from the screen — so the kernel is asked what it used rather
            // than the constant being copied here, which would stop being a
            // check the moment either side changed.
            let boot = qemu.boot_log().to_string();
            let Some((scale_x, scale_y)) = parse_rel_scale(&boot) else {
                return Err(format!("the kernel never said what pointer scale it used:\n{boot}"));
            };
            const DX: i32 = 40;
            const DY: i32 = -30;
            // Off the origin first — the accumulated position clamps at 0, so a
            // move up or left from there is invisible. Under 256 counts, or the
            // packet's overflow bit is set and the motion is dropped by design.
            let (result, sent) = input_events_run(&mut qemu, (200, 200), (DX, DY));
            if let Some(err) = &result.error {
                return Err(format!("{err} after {sent} of the sequence\n{}", result.stdout));
            }

            let keys = parse_key_events(&result.stdout);
            let typed: String = keys
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?}, want it to contain \"hello\" — the keyboard never reached userland:\n{}",
                    result.stdout
                ));
            }

            let pointer = parse_mouse_events(&result.stdout);
            // The delta the wire carried, not "it moved": a sign error in dy
            // and a dropped high bit both survive "it moved", and the PS/2
            // wire points the opposite way to the screen. Relative, so it
            // says nothing about where any compositor would draw a cursor.
            let want = (DX * scale_x, DY * scale_y);
            let deltas: Vec<(i32, i32)> = pointer
                .windows(2)
                .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
                .collect();
            if !deltas.contains(&want) {
                return Err(format!(
                    "no pointer event moved by {want:?}; deltas seen: {deltas:?}\n{}",
                    result.stdout
                ));
            }
            let Some(down) = pointer.iter().position(|e| e.buttons == 0x01) else {
                return Err(format!(
                    "no left-button-down event; buttons seen: {:?}",
                    pointer.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()
                ));
            };
            if !pointer[down + 1..].iter().any(|e| e.buttons == 0x00) {
                return Err(format!(
                    "the left button went down and never came up: {pointer:?}"
                ));
            }
            eprintln!(
                "  [metal-sim] {} key events (typed {typed:?}), {} pointer events, delta {want:?} delivered",
                keys.len(),
                pointer.len()
            );
            Ok(())
        }
        other => Err(format!("unknown input test {other}")),
    }
}

#[derive(Debug)]
struct KeyLine {
    usage: u8,
    modifiers: u8,
    translated: String,
}

/// `kev usage=0x04 mods=0x00 tr="a"` — what the in-guest reader prints.
fn parse_key_events(stdout: &str) -> Vec<KeyLine> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.split("kev usage=0x").nth(1)?;
            let (usage, rest) = rest.split_once(" mods=0x")?;
            let (modifiers, rest) = rest.split_once(" tr=")?;
            let translated = rest.trim().trim_matches('"');
            Some(KeyLine {
                usage: u8::from_str_radix(usage, 16).ok()?,
                modifiers: u8::from_str_radix(modifiers, 16).ok()?,
                translated: unescape(translated),
            })
        })
        .collect()
}

/// The guest prints through `{:?}`, so an escape sequence arrives as the
/// four characters `\u{1b}` rather than the byte.
fn unescape(s: &str) -> String {
    s.replace("\\u{1b}", "\u{1b}").replace("\\\"", "\"").replace("\\\\", "\\")
}

#[derive(Debug)]
struct MouseLine {
    buttons: u8,
    x: u16,
    y: u16,
}

/// `mev buttons=0x01 x=6400 y=6400` — what the in-guest reader prints.
fn parse_mouse_events(stdout: &str) -> Vec<MouseLine> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.split("mev buttons=0x").nth(1)?;
            let (buttons, rest) = rest.split_once(" x=")?;
            let (x, y) = rest.split_once(" y=")?;
            Some(MouseLine {
                buttons: u8::from_str_radix(buttons, 16).ok()?,
                x: x.parse().ok()?,
                y: y.trim().parse().ok()?,
            })
        })
        .collect()
}

/// The block count the NVMe driver derived, out of
/// `NVMe: block device id=1 blocks=62514774 (244198MB)`.
fn parse_nvme_blocks(log: &str) -> Option<u64> {
    log.lines()
        .find_map(|l| l.split("NVMe: block device id=").nth(1))?
        .split("blocks=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The first number after `marker`, which both caches print their ceiling as
/// exactly once at boot.
fn parse_cache_budget(log: &str, marker: &str) -> Option<u64> {
    log.lines()
        .find_map(|l| l.split(marker).nth(1))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Every `<prefix>N evictions, R/M <unit>` line, as (evictions, resident).
///
/// The kernel emits one per full turnover of the cache, so the series is the
/// shape of the answer: a cache that evicts has a climbing first column and a
/// flat second, and a cache that only grows has no lines at all.
fn parse_cache_series(log: &str, prefix: &str, unit: &str) -> Vec<(u64, u64)> {
    log.lines()
        .filter_map(|l| {
            let tail = l.split(prefix).nth(1)?;
            if !tail.contains(unit) {
                return None;
            }
            let evictions = tail.split(" evictions,").next()?.trim().parse().ok()?;
            let resident = tail.split("evictions, ").nth(1)?.split('/').next()?.parse().ok()?;
            Some((evictions, resident))
        })
        .collect()
}

/// How many blocks the page cache's index has room for, out of
/// `page cache: N device blocks, index sized for C cached blocks`.
fn parse_page_cache_index(log: &str) -> Option<u64> {
    log.lines()
        .find_map(|l| l.split("index sized for ").nth(1))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Decode one bcachefs superblock straight out of a disk image, with the
/// same parser the kernel uses — magic, version and CRC all checked.
fn read_superblock(image: &Path, block: u64) -> Result<bcachefs::Superblock, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = fs::File::open(image).map_err(|e| format!("open {}: {e}", image.display()))?;
    f.seek(SeekFrom::Start(block * 4096)).map_err(|e| format!("seek: {e}"))?;
    let mut buf = bcachefs::BlockBuf::zeroed();
    f.read_exact(buf.as_bytes_mut()).map_err(|e| format!("read: {e}"))?;
    bcachefs::Superblock::parse(&buf).map_err(|e| format!("{e:?}"))
}

/// A disk image's apparent size and the bytes it actually occupies. The gap
/// between the two is the whole reason a 244 GB test device is affordable.
fn image_extent(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
    (meta.len(), meta.blocks() * 512)
}

/// The guest's own boot duration, out of `Boot: complete (123ms)`.
fn boot_millis(log: &str) -> Option<u64> {
    log.lines()
        .find_map(|l| l.split("Boot: complete (").nth(1))?
        .split("ms)")
        .next()?
        .parse()
        .ok()
}

#[derive(Debug)]
struct XhciBind {
    kind: String,
    int_ring: usize,
}

/// `xHCI: USB keyboard ready on slot 2, int_ring +0xa000` — one line per HID
/// the driver bound, carrying the DMA offset of the ring that device's reports
/// arrive on. The offset is in the line because two devices sharing one ring
/// is invisible from outside: both keyboards still enumerate, still bind, and
/// still deliver — until the second one's TRBs land on top of the first's.
fn parse_xhci_binds(log: &str) -> Vec<XhciBind> {
    log.lines()
        .filter_map(|line| {
            let rest = line.split("xHCI: USB ").nth(1)?;
            let (kind, rest) = rest.split_once(" ready on slot ")?;
            let (_slot, rest) = rest.split_once(", int_ring +0x")?;
            Some(XhciBind {
                kind: kind.to_string(),
                int_ring: usize::from_str_radix(rest.split_whitespace().next()?, 16).ok()?,
            })
        })
        .collect()
}

/// Every CPU's `CR0` and `CR4`, against what a CPU running this kernel must
/// hold.
///
/// **Not the same question the kernel's own self-check asks.** That one compares
/// each CPU against the declaration, so it catches a CPU that missed it and
/// nothing else; a declaration that is wrong satisfies it on every core. The
/// bits below are spelled out here, away from the constants that produce them,
/// so the two have to agree independently — and the ones that matter are the
/// ones an AP used to arrive with: `CD`/`NW` set is caching off, `WP` clear is
/// the kernel's own read-only mappings not binding supervisor writes, `NE`
/// clear routes an unmasked x87 exception to a pin nothing listens on.
///
/// `OSXSAVE` is asserted *clear*: with it set the CPU would permit `XCR0` to
/// name components `FXSAVE64` does not save, and this kernel saves user FP
/// state with `FXSAVE64`.
///
/// Both halves, because the kernel writes both registers whole: every bit named
/// below must hold its named value, **and a bit named nowhere below may not be
/// set at all**. Silence about a bit is a hole rather than a permission.
fn control_regs(log: &str, cpus: u32) -> Result<(), String> {
    /// `(bit, name, must_be_set)`. Every bit `CR0` defines, so a value with any
    /// other bit set is reserved state the kernel put there.
    const CR0_BITS: &[(u32, &str, bool)] = &[
        (0, "PE", true),
        (1, "MP", true),
        (2, "EM", false),
        (3, "TS", false),
        (4, "ET", true),
        (5, "NE", true),
        (16, "WP", true),
        (18, "AM", false),
        (29, "NW", false),
        (30, "CD", false),
        (31, "PG", true),
    ];
    const CR4_BITS: &[(u32, &str, bool)] = &[
        (3, "DE", true),
        (5, "PAE", true),
        (6, "MCE", true),
        (9, "OSFXSR", true),
        (10, "OSXMMEXCPT", true),
        (12, "LA57", false),
        (16, "FSGSBASE", true),
        (18, "OSXSAVE", false),
    ];
    /// The four `CR4` bits the CPU may withhold, so neither answer is wrong.
    const CR4_MAY: &[(u32, &str)] = &[(11, "UMIP"), (17, "PCIDE"), (20, "SMEP"), (21, "SMAP")];

    let mut seen: Vec<(u32, u64, u64)> = Vec::new();
    for line in log.lines() {
        let Some(rest) = line.split("control_regs: cpu").nth(1) else { continue };
        let Some((id, rest)) = rest.split_once(" cr0=0x") else { continue };
        let Some((cr0, cr4)) = rest.split_once(" cr4=0x") else { continue };
        let (Ok(id), Ok(cr0), Ok(cr4)) = (
            id.parse::<u32>(),
            u64::from_str_radix(cr0, 16),
            u64::from_str_radix(cr4.split_whitespace().next().unwrap_or(""), 16),
        ) else {
            return Err(format!("unreadable control-register line: {line:?}"));
        };
        seen.push((id, cr0, cr4));
    }

    // Which CPUs answered, not how many lines were printed: a boot where one AP
    // never came up at all must not pass by having the BSP print twice.
    let ids: BTreeSet<u32> = seen.iter().map(|&(id, _, _)| id).collect();
    let want: BTreeSet<u32> = (0..cpus).collect();
    if ids != want {
        return Err(format!(
            "expected one line from each of {want:?}, got {ids:?}:\n{log}"
        ));
    }

    // A bit named nowhere above is as much of the declaration as a named one,
    // and the kernel writes both registers whole — so `UMIP`, `PGE`, `TSD` or
    // `PKE` would reach every CPU with nothing here to say so. Which is why
    // what follows is the set this gate has an opinion about, rather than a
    // second list of bits to forbid: a forbid-list fails open on the next bit.
    let named = |bits: &[(u32, &str, bool)], may: &[(u32, &str)]| -> u64 {
        bits.iter().fold(0, |m, b| m | 1u64 << b.0)
            | may.iter().fold(0, |m, b| m | 1u64 << b.0)
    };

    // Every wrong bit on a CPU rather than the first: an AP holding INIT's CR0
    // is wrong in five at once and each is a different consequence, so a
    // message naming one sends the next reader after a fifth of it.
    for &(id, cr0, cr4) in &seen {
        let mut wrong = String::new();
        for (reg, value, bits, known) in [
            ("cr0", cr0, CR0_BITS, named(CR0_BITS, &[])),
            ("cr4", cr4, CR4_BITS, named(CR4_BITS, CR4_MAY)),
        ] {
            for &(bit, name, set) in bits {
                if (value & (1 << bit) != 0) != set {
                    wrong += &format!(" {reg}.{name} must be {}", if set { "set" } else { "clear" });
                }
            }
            let extra = value & !known;
            if extra != 0 {
                wrong += &format!(" {reg} holds {extra:#x}, which this gate never named");
            }
        }
        if !wrong.is_empty() {
            return Err(format!("cpu{id} cr0={cr0:#010x} cr4={cr4:#010x}:{wrong}"));
        }
    }

    // A CPU that agrees about every bit named above can still differ in one
    // that is not, and a thread migrating onto it would execute differently
    // from one moment to the next.
    let (_, cr0, cr4) = seen[0];
    if let Some(&(id, other0, other4)) = seen.iter().find(|&&(_, a, b)| (a, b) != (cr0, cr4)) {
        return Err(format!(
            "cpu0 has cr0={cr0:#010x} cr4={cr4:#010x} and cpu{id} has \
             cr0={other0:#010x} cr4={other4:#010x}"
        ));
    }

    eprintln!("  [control_regs] {cpus} CPUs, cr0={cr0:#010x} cr4={cr4:#010x}");
    Ok(())
}

/// [`control_regs`] against machines this host cannot boot, with no guest.
///
/// [`control_regs_negative`] runs the real defective machine and is the link
/// between this verdict and a kernel; what is here is the states no actuator
/// reaches — a CPU that differs from three others, a bit set uniformly on all
/// four, an AP that never printed. Every value is one this tree has printed or
/// one bit away from it.
fn control_regs_verdict() -> Result<(), String> {
    /// The pre-fix machine, `smp=4`, TCG, read off this tree on 2026-08-08:
    /// firmware's registers on the BSP and INIT's on every AP.
    const AP_BEFORE: (u64, u64) = (0xe000_0011, 0x0031_0620);
    const DECLARED: (u64, u64) = (0x8001_0033, 0x0031_0668);

    fn log(cpus: &[(u64, u64)]) -> String {
        cpus.iter()
            .enumerate()
            .map(|(i, (cr0, cr4))| {
                format!("[kernel 0.1 cpu{i}] control_regs: cpu{i} cr0={cr0:#010x} cr4={cr4:#010x}\n")
            })
            .collect()
    }

    let refused = |what: &str, cpus: &[(u64, u64)], says: &str| match control_regs(&log(cpus), 4) {
        Ok(()) => Err(format!("{what} was accepted")),
        Err(e) if e.contains(says) => Ok(()),
        Err(e) => Err(format!("{what} was refused for the wrong reason: {e}")),
    };

    // Positive control first: a verdict that refuses everything refuses the
    // defect too, and would prove nothing below.
    control_regs(&log(&[DECLARED; 4]), 4)
        .map_err(|e| format!("the declared machine was refused: {e}"))?;

    refused("the machine this tree booted", &[DECLARED, AP_BEFORE, AP_BEFORE, AP_BEFORE], "CD")?;
    // The case a "do all the CPUs agree?" test passes: they agree, on INIT's
    // value. Nothing about uniformity says caching is on.
    refused("four CPUs agreeing on INIT's CR0", &[AP_BEFORE; 4], "CD")?;
    refused(
        "one CPU without WP",
        &[DECLARED, (DECLARED.0 & !(1 << 16), DECLARED.1), DECLARED, DECLARED],
        "WP",
    )?;
    refused(
        "one CPU without NE",
        &[DECLARED, DECLARED, (DECLARED.0 & !(1 << 5), DECLARED.1), DECLARED],
        "NE",
    )?;
    // The bit that must be *absent*: with it set, XCR0 can name components
    // FXSAVE64 does not save.
    refused("OSXSAVE set", &[(DECLARED.0, DECLARED.1 | (1 << 18)); 4], "OSXSAVE")?;
    // Two bits a machine could hold uniformly, each one line of kernel diff
    // away, and neither reachable by an actuator. `AM` is named clear above and
    // answers by name; `PGE` is named nowhere, which is the case the whole
    // never-named rule exists for — `TSD` and `PKE` are the same case. `UMIP`
    // used to be this file's example of the same thing, until it joined
    // `CR4_MAY` (`issues/isolation/cr4-umip-undeclared.md`) — a bit that
    // moves from unnamed to optional is exactly the migration this gate exists
    // to force a diff for.
    refused("every CPU with AM set", &[(DECLARED.0 | (1 << 18), DECLARED.1); 4], "AM")?;
    refused(
        "every CPU with PGE set",
        &[(DECLARED.0, DECLARED.1 | (1 << 7)); 4],
        "never named",
    )?;
    // A CPU that agrees about every named bit and differs in one the CPU is
    // allowed to withhold, so nothing above it can object.
    refused("one CPU with PCID and three without", &[DECLARED, DECLARED, DECLARED, (DECLARED.0, DECLARED.1 | (1 << 17))], "cpu3")?;
    // And an AP that never printed at all, which is what a machine whose AP
    // died before the check looks like.
    refused("three lines for four CPUs", &[DECLARED; 3], "{0, 1, 2, 3}")?;

    eprintln!("  [control_regs] the verdict refuses 9 machines and accepts the declared one");
    Ok(())
}

/// The negative control, executed: an AP left holding what `INIT` gave it, and
/// [`control_regs`] refusing the machine that produces.
///
/// The one link the two tests above do not cover. [`control_regs`] reads a
/// healthy boot and [`control_regs_verdict`] reads values typed into this file;
/// between them sits the question of whether the verdict would recognise a real
/// divergent CPU, and a `no-ap-control-regs` kernel nothing runs answers it in
/// prose. The boot dies here — the kernel's own assertion kills it — but
/// `self_check` logs *before* it asserts, exactly so that the values a CPU
/// failed with survive the failure, and that is what this reads.
///
/// `smp=2` because the first AP to check itself panics and `halt_all_cpus`
/// follows it: any CPU after that one is a line that never arrives, and the
/// refusal would then be about the count rather than about the registers.
/// [`qemu::Profile::Metal`] because there the 16550 is the console: a guest that
/// dies during `boot_aps` has no virtio-console yet, and this way one channel
/// carries the per-CPU line and the panic that follows it.
fn control_regs_negative(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    const CPUS: u32 = 2;
    // The AP's own line, which arrives whether or not the actuator did anything
    // — so a feature that silently stopped working is a named failure below
    // rather than a boot timeout with nothing to read.
    const MARKER: &str = "control_regs: cpu1 cr0=";

    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            smp: CPUS,
            profile: qemu::Profile::Metal,
            kernel_params: &["no-ap-control-regs"],
            ready_marker: MARKER,
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    log += &qemu.drain_until(Duration::from_secs(20), |l| l.contains("the declaration is"));

    // The premise: a divergent CPU, not merely a dead boot. Anything can kill a
    // boot, and a test that only asserted the panic would pass on a kernel
    // whose registers were right and whose assertion was wrong.
    let Err(refusal) = control_regs(&log, CPUS) else {
        return Err(format!(
            "the verdict accepted a `no-ap-control-regs` boot — either the actuator did \
             nothing or the verdict cannot see the machine it was written for\n{log}"
        ));
    };
    // Named, and named for a bit rather than for the count: a refusal about a
    // missing line or an unreadable one satisfies `is_err` and means nothing.
    //
    // **`WP`, not `CD`, and that is a finding rather than a preference.** `CD`
    // is the consequence this whole file exists for, and it is the obvious bit
    // to demand — but a guest cannot hold it under KVM. Measured 2026-08-08:
    // an AP that has executed nothing but the trampoline reads `cr0=0xe0000011`
    // under this host's TCG and `cr0=0x80000011` on an Intel Xeon 6973P-C KVM
    // runner (CI run 31278396401, shard 3), `CD` and `NW` clear, everything
    // else identical. So `CD` here would be a gate that only one of the two
    // machines this suite runs on can fail. `WP` is absent on the AP either
    // way, and its consequence — the kernel's own read-only mappings not
    // binding supervisor writes — does not depend on the hypervisor.
    if !refusal.contains("cpu1") || !refusal.contains("WP") {
        return Err(format!(
            "the verdict refused for something other than cpu1's write protection: {refusal}"
        ));
    }
    // Where the host does leave `CD` set, it is demanded, so the arm that *can*
    // see the caching defect does not quietly become the weaker of the two.
    if !toyos_build::kvm_usable() && !refusal.contains("CD") {
        return Err(format!(
            "TCG leaves an AP's `CD` set and the refusal does not name it: {refusal}"
        ));
    }
    // And the kernel refused too, on its own assertion rather than on a fault
    // somewhere downstream of one — the shipped check, on the shipped line. The
    // declaration is a constant, so it is the same number on either host.
    for want in ["control_regs: cpu1 holds cr0=", "the declaration is 0x80010033"] {
        if !log.contains(want) {
            return Err(format!("the kernel never said {want:?}:\n{log}"));
        }
    }
    eprintln!("  [control_regs] a real divergent AP, refused: {refusal}");
    Ok(())
}

/// The largest an idle-trip counter (`kernel/src/scheduler.rs`'s
/// `IDLE_TRIPS`, printed as `trips=` on `sched: cpu=`'s now rate-limited
/// line) may move for one CPU across a captured serial before
/// [`idle_is_spinning`] calls it spinning rather than halting.
///
/// Two orders of magnitude above the worst real trip delta this suite has
/// measured on a healthy `i8042_quarantine` run on this host (`cargo test --
/// i8042_quarantine`, 2026-08-17: cpu1 moved by 2 within the capture — one
/// print at readiness, one roughly ten seconds later, the rate limit's own
/// cadence) and well under the shape of the regression this gate exists for:
/// the first quarantine driver's undrained `irq_ring` produced 2685 printed
/// lines in 5 s under the *old*, unthrottled-per-1000-trips counter — at
/// least 2,685,000 trips in that one window alone.
const MAX_IDLE_TRIP_DELTA: u64 = 100_000;

/// Whether any CPU's idle-trip counter moved by more than [`MAX_IDLE_TRIP_DELTA`]
/// across `serial`, and which one if so.
///
/// **Not the same question a count of `sched: cpu=` lines answers**, and
/// deliberately not: `log_health` prints at most once per
/// `SNAPSHOT_INTERVAL_NS` now, so a CPU that spins through idle and one that
/// halts cleanly between rare wakes produce the same number of *lines* —
/// only the counter inside each line still moves at the two different
/// speeds (`issues/kernel/i8042-quarantine-health-line-count-is-vacuous.md`).
/// Per CPU, and the worst offender rather than every one, because a spin on
/// one CPU must not be hidden by averaging it against another CPU's healthy
/// rate.
fn idle_is_spinning(serial: &str) -> Option<(u32, u64)> {
    let mut spread: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for line in serial.lines() {
        let Some(rest) = line.split("sched: cpu=").nth(1) else { continue };
        let Some((id, rest)) = rest.split_once(' ') else { continue };
        let Some(trips) = rest.split("trips=").nth(1).and_then(|t| t.split_whitespace().next())
        else {
            continue;
        };
        let (Ok(id), Ok(trips)) = (id.parse::<u32>(), trips.parse::<u64>()) else { continue };
        spread
            .entry(id)
            .and_modify(|(min, max)| {
                *min = (*min).min(trips);
                *max = (*max).max(trips);
            })
            .or_insert((trips, trips));
    }
    spread.into_iter().map(|(id, (min, max))| (id, max - min)).find(|&(_, delta)| delta > MAX_IDLE_TRIP_DELTA)
}

/// [`idle_is_spinning`] against a healthy trace and a crafted one shaped like
/// the regression it exists to catch, with no guest — the same split
/// `control_regs`/`control_regs_verdict` use, and for the same reason: a
/// gate's own teeth are a claim a live boot cannot demonstrate on the
/// negative side, because nothing in this tree can stage a CPU into spinning
/// through idle on purpose.
///
/// This is the demonstration `i8042-quarantine-health-line-count-is-vacuous`
/// asked for: proof the restored assertion still fails when the condition it
/// names is violated, not just that it still passes when it is not.
fn idle_trip_verdict() -> Result<(), String> {
    let healthy = "\
[kernel 0.1 cpu0] sched: cpu=0 ready=0 dying=0 parked=0 current=None trips=1\n\
[kernel 0.1 cpu1] sched: cpu=1 ready=0 dying=0 parked=0 current=None trips=1\n\
[kernel 0.1 cpu1] sched: cpu=1 ready=0 dying=0 parked=0 current=None trips=3\n\
[kernel 0.1 cpu0] sched: cpu=0 ready=0 dying=0 parked=0 current=None trips=2\n";
    if let Some((cpu, delta)) = idle_is_spinning(healthy) {
        return Err(format!("a healthy trace was refused: cpu{cpu} moved by {delta}"));
    }

    // The regression's own shape: one CPU quarantines cleanly and stays
    // quiet, the other's undrained ring never lets it halt.
    let spinning = "\
[kernel 0.1 cpu0] sched: cpu=0 ready=0 dying=0 parked=0 current=None trips=1\n\
[kernel 0.1 cpu1] sched: cpu=1 ready=0 dying=0 parked=0 current=None trips=4\n\
[kernel 0.1 cpu0] sched: cpu=0 ready=0 dying=0 parked=0 current=None trips=2\n\
[kernel 0.1 cpu1] sched: cpu=1 ready=0 dying=0 parked=0 current=None trips=2685004\n";
    match idle_is_spinning(spinning) {
        Some((1, delta)) if delta > MAX_IDLE_TRIP_DELTA => {}
        Some((cpu, delta)) => {
            return Err(format!("refused the wrong CPU or by the wrong margin: cpu{cpu} delta {delta}"))
        }
        None => return Err("a spinning CPU's trace was accepted".to_string()),
    }

    // And the line the old, count-of-lines check would have been fooled by:
    // the same number of `sched: cpu=` lines either way, because the print
    // itself is rate-limited regardless of what is underneath it — which is
    // exactly the vacuity this replaces.
    assert_eq!(
        healthy.matches("sched: cpu=").count(),
        spinning.matches("sched: cpu=").count(),
        "the crafted traces must differ only in trips=, not in line count — otherwise this proves nothing about the old check's blindness"
    );

    eprintln!("  [i8042] the idle-trip verdict accepts a healthy trace and refuses a spinning one");
    Ok(())
}

/// Everything the driver derived its DMA pool from, off the two lines it
/// prints. Reading these is what makes a test see a *derivation* rather than
/// the fact that some number was printed: every fixed cap that ever stood
/// where `Layout::new`'s `.min(max_slots)` stands now leaves six devices
/// enumerating on six rings and is invisible from every other angle.
#[derive(Debug)]
struct XhciLayout {
    /// `xHCI: max_slots=64 max_ports=12 …`, straight off HCSPARAMS1.
    cap_slots: usize,
    pool_kib: usize,
    scratchpad: usize,
    blocks: usize,
    stride: usize,
}

fn parse_xhci_layout(log: &str) -> Option<XhciLayout> {
    let cap = log.lines().find_map(|l| l.split("xHCI: max_slots=").nth(1))?;
    let dma = log.lines().find_map(|l| l.split("xHCI: dma ").nth(1))?;
    let (pool_kib, rest) = dma.split_once(" KiB: scratchpad=")?;
    let (scratchpad, rest) = rest.split_once(" device blocks=")?;
    let (blocks, rest) = rest.split_once(" of ")?;
    let stride = rest.split_once(" B (max_slots=")?.0;
    Some(XhciLayout {
        cap_slots: cap.split_whitespace().next()?.parse().ok()?,
        pool_kib: pool_kib.parse().ok()?,
        scratchpad: scratchpad.parse().ok()?,
        blocks: blocks.parse().ok()?,
        stride: stride.parse().ok()?,
    })
}

/// Every slot id in an `xHCI: slot 3 enabled ...` line, in order.
fn parse_xhci_slots(log: &str) -> Vec<u32> {
    log.lines()
        .filter_map(|line| line.split("xHCI: slot ").nth(1)?.split_once(" enabled"))
        .filter_map(|(slot, _)| slot.parse().ok())
        .collect()
}

/// One step of the `input_events` sequence, and how many lines the guest owes
/// for it.
enum Poke {
    Move(i32, i32),
    Button(&'static str, bool),
    Tap(&'static str),
}

/// What tells the `input_events` client its host has finished.
///
/// The right button, which no sequence driving that client produces for any
/// other reason, and the release rather than the press so the pointer is left
/// with nothing held. Every caller owes it one: without it the client waits out
/// its liveness ceiling, and `xhci_hid_break`, `xhci_hotplug` and `xhci_flap`
/// each paid 30 s for the omission.
pub(crate) fn input_events_end(input: &mut qemu::QmpInput) {
    input.mouse(0, 0, Some(("right", true)));
    input.mouse(0, 0, Some(("right", false)));
}

/// The `input_events` sequence: land off the origin, move by a named delta,
/// click, type `hello`, and finish on the right button the client exits on.
///
/// Every step waits for the guest to print what the step before it produced, so
/// the host never has more than one packet in flight and a device queue cannot
/// swallow one. `xhci_second_controller` measured the alternative at width 4:
/// four pointer events arrived and all five keys were lost, which reads exactly
/// like the defect it exists to catch.
fn input_events_run(
    qemu: &mut QemuInstance,
    home: (i32, i32),
    delta: (i32, i32),
) -> (TestResult, usize) {
    let script = [
        Poke::Move(home.0, home.1),
        Poke::Move(delta.0, delta.1),
        Poke::Button("left", true),
        Poke::Button("left", false),
        Poke::Tap("h"),
        Poke::Tap("e"),
        Poke::Tap("l"),
        Poke::Tap("l"),
        Poke::Tap("o"),
        // `input_events_end`, spelled out because the script paces every step
        // against an arrival and cannot hand two of them to someone else.
        Poke::Button("right", true),
        Poke::Button("right", false),
    ];
    let sent = std::cell::Cell::new(0usize);
    let result = {
        let mut input: Option<qemu::QmpInput> = None;
        let (mut mev, mut kev) = (0usize, 0usize);
        let (mut want_mev, mut want_kev) = (0usize, 0usize);
        qemu.run_test_paced("test_rs_input_events", Duration::from_secs(60), |socket, line| {
            if line.contains("===INPUT_READY===") {
                input = Some(qemu::QmpInput::open(
                    socket.expect("input_events needs BootOptions { qmp: true }"),
                ));
            }
            mev += usize::from(line.contains("mev buttons="));
            kev += usize::from(line.contains("kev usage="));
            let Some(input) = input.as_mut() else { return };
            if mev < want_mev || kev < want_kev {
                return;
            }
            let Some(poke) = script.get(sent.get()) else { return };
            match poke {
                Poke::Move(dx, dy) => {
                    input.mouse(*dx, *dy, None);
                    want_mev += 1;
                }
                Poke::Button(name, down) => {
                    input.mouse(0, 0, Some((name, *down)));
                    want_mev += 1;
                }
                Poke::Tap(key) => {
                    input.keys(&[(key, true), (key, false)]);
                    want_kev += 2;
                }
            }
            sent.set(sent.get() + 1);
        })
    };
    (result, sent.get())
}

/// The per-axis relative-pointer scale out of `mouse: rel scale x=64 y=64`.
///
/// Read from the kernel rather than restated here: `kernel/src/mouse.rs`
/// derives it from the screen, so a copy of the constant would stop being a
/// check the moment either side changed.
fn parse_rel_scale(log: &str) -> Option<(i32, i32)> {
    let (x, rest) = log
        .lines()
        .find_map(|l| l.split("mouse: rel scale x=").nth(1))?
        .split_once(" y=")?;
    Some((x.parse().ok()?, rest.split_whitespace().next()?.parse().ok()?))
}

/// The `-device` arguments naming an xHCI controller. A machine's controller
/// count is a shape claim, and argv is the only place it is visible: two
/// controllers where one carries nothing look identical from inside a guest
/// that never enumerated the second.
fn xhci_argv(argv: &[String]) -> Vec<&str> {
    argv.windows(2)
        .filter(|w| w[0] == "-device")
        .map(|w| w[1].as_str())
        .filter(|v| v.contains("usb-xhci"))
        .collect()
}

/// `(slot, source)` out of every `xHCI: pointer on slot 3 merges as source 2`.
///
/// The slot is there so the test can show the collision it is guarding
/// against: two pointers on one slot id of two different controllers is
/// exactly what a slot-derived button-merge source folded into one entry.
fn parse_pointer_sources(log: &str) -> Vec<(u32, u32)> {
    log.lines()
        .filter_map(|line| {
            let rest = line.split("xHCI: pointer on slot ").nth(1)?;
            let (slot, source) = rest.split_once(" merges as source ")?;
            Some((
                slot.parse().ok()?,
                source.split_whitespace().next()?.parse().ok()?,
            ))
        })
        .collect()
}

/// The `-device usb-*` arguments a profile passes, boot stick included.
fn usb_argv(argv: &[String]) -> Vec<&str> {
    argv.windows(2)
        .filter(|w| w[0] == "-device")
        .map(|w| w[1].as_str())
        .filter(|v| v.starts_with("usb-"))
        .collect()
}

/// The `keys=` field of an `i8042: drain ...` trace line.
fn trace_keys(line: &str) -> Option<usize> {
    line.split("i8042: drain ")
        .nth(1)?
        .split("keys=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn build_test_registry(
    rust_bins: &[(String, Vec<u8>)],
    c_names: &[String],
) -> Vec<TestDef> {
    let mut tests = Vec::new();

    for name in discover_rust_tests(rust_bins) {
        let timeout = match name.as_str() {
            "panic_recovery" => Duration::from_secs(10),
            // Writes the child's whole image through bcachefs before it can run
            // it, which is the only thing here that is not a spawn.
            "disk_backtrace" => Duration::from_secs(15),
            // Its verdict is that a parked waiter woke, so the failing run is
            // the slow one: it spends its own patience before reporting, and
            // the report is worth more than the harness's timeout message.
            "inbox_cancel_wakes" => Duration::from_secs(30),
            _ => Duration::from_secs(5),
        };
        tests.push(TestDef {
            qemu_name: format!("test_rs_{name}"),
            check: check_for(&name),
            settle: settle_for(&name),
            timeout,
            name,
        });
    }

    for name in c_names {
        tests.push(TestDef {
            qemu_name: format!("test_c_{name}"),
            timeout: Duration::from_secs(10),
            check: check_c_result,
            settle: no_settle,
            name: name.clone(),
        });
    }

    tests
}

fn run_debug_mode(c_tests: &[(String, Vec<u8>)], rust_bins: &[(String, Vec<u8>)]) {
    let cmd_path = Path::new("/tmp/toyos-debug-cmd");
    let result_path = Path::new("/tmp/toyos-debug-result");
    let ready_path = Path::new("/tmp/toyos-debug-ready");

    let _ = fs::remove_file(cmd_path);
    let _ = fs::remove_file(result_path);
    let _ = fs::remove_file(ready_path);

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let mut qemu = QemuInstance::boot_with_options(
        &test_config,
        c_tests,
        rust_bins,
        BootOptions {
            gdb_stub: true,
            debug_wait: true,
            ..Default::default()
        },
    );

    let repo = compile::repo_root();
    let kernel_elf = repo.join(format!(
        "kernel/target/x86_64-unknown-none/{}/kernel",
        toyos_build::build::PROFILE
    ));

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  QEMU running with GDB stub on localhost:1234               ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  Kernel ELF: {}", kernel_elf.display());
    eprintln!("║                                                              ║");
    eprintln!("║  Send commands:                                              ║");
    eprintln!("║    echo 'run test_c_49_bracket_evaluation' > {}    ║", cmd_path.display());
    eprintln!("║    echo 'run test_rs_std_alloc' > {}               ║", cmd_path.display());
    eprintln!("║    cat {}                                 ║", result_path.display());
    eprintln!("║    echo 'quit' > {}                                ║", cmd_path.display());
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    fs::write(ready_path, "ready\n").unwrap();

    loop {
        thread::sleep(Duration::from_millis(200));

        let cmd = match fs::read_to_string(cmd_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = fs::remove_file(cmd_path);
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }

        if cmd == "quit" || cmd == "q" {
            eprintln!("[debug] Quit requested");
            let _ = fs::write(result_path, "quit\n");
            break;
        }

        if let Some(test_name) = cmd.strip_prefix("run ") {
            let test_name = test_name.trim();
            eprintln!("[debug] Running {test_name}...");
            let result = qemu.run_test(test_name, Duration::from_secs(60));

            let mut output = String::new();
            output.push_str(&format!("test: {}\n", result.name));
            output.push_str(&format!("exit_code: {:?}\n", result.exit_code));
            if let Some(err) = &result.error {
                output.push_str(&format!("error: {err}\n"));
            }
            if !result.stdout.is_empty() {
                output.push_str("--- stdout ---\n");
                output.push_str(&result.stdout);
            }
            eprintln!("[debug] {output}");
            fs::write(result_path, &output).unwrap();
        } else {
            eprintln!("[debug] Sending raw serial: {cmd}");
            writeln!(qemu.stdin_mut(), "{cmd}").expect("Failed to write to QEMU stdin");
            qemu.flush_stdin();
            fs::write(result_path, "sent\n").unwrap();
        }
    }

    let _ = fs::remove_file(ready_path);
    eprintln!("[debug] Shutting down QEMU...");
}

/// What one worker takes off the queue.
///
/// A boot, or the run of adjacent boots that share one guest — never a bare
/// test name, because [`group_boot`] makes adjacency in [`MACHINE_TESTS`]
/// load-bearing and a group split across two workers would boot two machines
/// and drain one console between them.
#[derive(Clone)]
enum Task<'a> {
    /// Rust and C tests on one guest, and the kernel that guest boots.
    ///
    /// Two blocks rather than one: [`ACTUATOR_TESTS`] needs `SYS_DEBUG` and
    /// everything else must not have it, which is what makes the second list
    /// the shipping binary's own coverage.
    Shared(Vec<&'a TestDef>, &'static [&'static str]),
    Machine(Vec<&'static str>),
    Screen(&'static str),
}

/// What the suite has to say about one test once it has finished.
struct Outcome {
    name: String,
    /// `None` is a pass — but only [`Outcome::verdict`] may read it as one.
    reason: Option<String>,
    elapsed: Duration,
    /// How long the host was suspended while this test ran. A verdict taken
    /// across that is not a verdict, whichever way it came out.
    suspended: Duration,
}

/// What the suite may conclude from one outcome.
///
/// `Pass` and `Fail` carry the entry that was consulted and **did not apply** —
/// the two ways an [`EXPECTED_FAILURES`] entry can be present and still leave
/// the ordinary verdict standing. Carried in the type so that no arm can treat
/// one as accounted for by forgetting to look it up.
#[derive(PartialEq, Debug)]
enum Verdict {
    /// `Some` when the name is listed and the entry's [`Stale`] says a pass
    /// proves nothing about it. Still a pass — and still worth a line, because
    /// a green run of a known-red test is not evidence that anything is fixed.
    Pass(Option<&'static ExpectedFailure>),
    /// Red. `Some` when the name *is* listed and the failure is not the one that
    /// entry covers — the case an exemption must not absorb, and the one the
    /// report has to explain rather than print as an ordinary red.
    Fail(Option<&'static ExpectedFailure>),
    /// The host stopped in the middle of it. Neither a pass nor a fail: the
    /// guest, QEMU's virtual clock and every wall-clock margin the test's
    /// assertion rests on all jumped by however long the lid was closed, so the
    /// run measured something and it was not this tree.
    Invalid,
    /// Listed, and failed the way its entry says. Not red — and reported by
    /// name, with its task, on every run.
    Expected(&'static ExpectedFailure),
    /// Listed with [`Stale::OnAPass`], and passed. **Red**: the entry claimed
    /// this test fails every run and it did not, so the entry is out of date
    /// about this tree.
    Stale(&'static ExpectedFailure),
}

impl Outcome {
    fn verdict(&self) -> Verdict {
        self.verdict_against(EXPECTED_FAILURES)
    }

    /// Whether this red is a blown liveness guard rather than an answer.
    ///
    /// Deliberately *not* a [`Verdict`] arm. A stall is red on exactly the same
    /// terms as any other red — the exit code, the expected-failure lookup and
    /// the alone re-run all have to treat it identically, and an arm would make
    /// each of those a place where somebody could decide otherwise. What it
    /// changes is only what the reader is told, which is the whole complaint:
    /// the run establishes nothing about this tree, so nobody should bisect it.
    fn stalled(&self) -> bool {
        self.reason.as_deref().is_some_and(|r| r.contains(STALLED))
    }

    /// The table is a parameter so the gates can state a case rather than
    /// depend on what the tree happens to be expecting today — which is the
    /// empty list whenever the tree is healthy, and therefore no case at all.
    fn verdict_against(&self, expected: &'static [ExpectedFailure]) -> Verdict {
        if self.suspended >= common::clock::SUSPENDED_AT_LEAST {
            return Verdict::Invalid;
        }
        let listed = expected.iter().find(|e| e.test == self.name);
        match (&self.reason, listed) {
            (None, None) => Verdict::Pass(None),
            (None, Some(entry)) => match entry.stale {
                Stale::OnAPass => Verdict::Stale(entry),
                Stale::OnThisDate(_) => Verdict::Pass(Some(entry)),
            },
            (Some(_), None) => Verdict::Fail(None),
            (Some(reason), Some(entry)) => {
                if entry.says.iter().any(|fragment| reason.contains(fragment)) {
                    Verdict::Expected(entry)
                } else {
                    Verdict::Fail(Some(entry))
                }
            }
        }
    }
}

/// The one line of a failure that names it.
///
/// A red's `reason` is the assertion's sentence with the whole capture pasted
/// after it, and the capture differs between any two boots — so the first line
/// is what "the same failure" can be asked about, and it is what the summary
/// already prints.
fn headline(reason: Option<&str>) -> String {
    reason.unwrap_or("check failed").lines().next().unwrap_or("check failed").to_string()
}

/// What the isolated re-run of one red is allowed to say about it.
///
/// **A red-again arm quotes the alone run's own failure**, and says so when it
/// is not the failure the wide run found. `red again — the defect is real` used
/// to be the whole line, and the `failures:` summary beside it always carries
/// the *wide* run's message: on PR #22's run `31424496450` the wide run failed
/// `xhci_hid_break`'s endpoint count and the alone re-run failed its pointer
/// delivery three minutes later, and the job said `red again` over the wide
/// run's sentence — so an adjudicator read one assertion's evidence for
/// another's. Two different assertions in one job is not a weaker finding than
/// one twice; it is a different and larger one, and the line now says which it
/// was (`issues/hardware/xhci-hid-break-counts-any-endpoint-3.md`).
///
/// The green arms are untouched. They are a classification the whole redlist is
/// written against, and nothing about them was wrong.
///
/// Pure, and every input a parameter, so [`alone_line_reports_the_alone_run`]
/// can stage the divergence rather than wait for CI to produce one.
fn alone_line(name: &str, wide: &str, shared_the_host: bool, alone: Option<&Outcome>) -> String {
    let Some(outcome) = alone else {
        return format!("  ALONE {name}: the lone run reported nothing about it");
    };
    match outcome.verdict() {
        // **Two different findings, and which one it is depends on whether the
        // first run shared the host** — the parallel phase's width, never the
        // run's, because the serial tail is one guest at any width. Beside other
        // guests, a green retry says this one was not, which is a classification
        // defect.
        Verdict::Pass(_) | Verdict::Stale(_) if shared_the_host => format!(
            "  ALONE {name}: GREEN — it fails only beside other guests, so its \
             Sched::Parallel is wrong. The run stays red on the classification."
        ),
        // Alone both times, nothing differed that the harness controls: it
        // failed once and passed once, which is a *rate* and says nothing about
        // `Sched`. CI runs one lane per machine, so every one of its retries is
        // the second kind.
        Verdict::Pass(_) | Verdict::Stale(_) => format!(
            "  ALONE {name}: GREEN, and it was alone both times — nothing the harness \
             controls differed, so it failed once and passed once. That is a rate and \
             not a classification."
        ),
        Verdict::Fail(_) | Verdict::Expected(_) => {
            let said = headline(outcome.reason.as_deref());
            if said == wide {
                format!("  ALONE {name}: red again, the same failure both times — the defect \
                         is real. {said}")
            } else {
                format!(
                    "  ALONE {name}: red again on a DIFFERENT failure — it failed twice, on two \
                     assertions, so this is not one defect reproduced and the divergence is \
                     itself the finding.\n      wide:  {wide}\n      alone: {said}"
                )
            }
        }
        Verdict::Invalid => format!("  ALONE {name}: the host was suspended during the retry too"),
    }
}

/// Whether the `ALONE:` line still reports the run it is a line about.
///
/// The staged pair is the one that was mis-reported: a wide failure and an
/// alone failure that are not the same sentence. A gate rather than a comment
/// because the defect is invisible from inside a green run — every arm prints
/// *a* plausible line, and only the quoted text says which run it came from.
fn alone_line_reports_the_alone_run() -> Result<(), String> {
    const WIDE: &str = "3 endpoint(s) were found Running after the break, want 2";
    const OTHER: &str = "input never came back: no pointer event moved by (2560, -1920)";
    let red = |reason: &str| Outcome {
        name: "a_test".to_string(),
        reason: Some(format!("{reason}\n[kernel 2.639 cpu0] a whole capture nobody diffs")),
        elapsed: Duration::from_secs(9),
        suspended: Duration::ZERO,
    };
    let green = Outcome {
        name: "a_test".to_string(),
        reason: None,
        elapsed: Duration::from_secs(9),
        suspended: Duration::ZERO,
    };

    // The two greens, byte for byte what they have always been: the redlist and
    // every issue file quote these, and a reworded classification would silently
    // invalidate the record rather than add to it.
    let wide_green = alone_line("a_test", WIDE, true, Some(&green));
    if !wide_green.contains(
        "GREEN — it fails only beside other guests, so its Sched::Parallel is wrong. \
         The run stays red on the classification.",
    ) {
        return Err(format!("the shared-host green arm has changed wording:\n{wide_green}"));
    }
    let lone_green = alone_line("a_test", WIDE, false, Some(&green));
    if !lone_green.contains(
        "GREEN, and it was alone both times — nothing the harness controls differed, so it \
         failed once and passed once. That is a rate and not a classification.",
    ) {
        return Err(format!("the alone-both-times green arm has changed wording:\n{lone_green}"));
    }
    for line in [&wide_green, &lone_green] {
        if line.contains(WIDE) {
            return Err(format!("a green quotes the wide run's failure:\n{line}"));
        }
    }

    // Red again on the same assertion: still "the defect is real", now with the
    // sentence the *alone* run produced under it.
    let same = alone_line("a_test", WIDE, false, Some(&red(WIDE)));
    if !same.contains("red again, the same failure both times") || !same.contains(WIDE) {
        return Err(format!("a reproduced failure does not say so, or does not quote it:\n{same}"));
    }
    if same.contains("[kernel 2.639") {
        return Err(format!("the line pasted the whole capture into the summary:\n{same}"));
    }

    // And the case the old line could not tell apart from it.
    let diverged = alone_line("a_test", WIDE, false, Some(&red(OTHER)));
    if !diverged.contains("DIFFERENT failure") {
        return Err(format!("two different failures read as one reproduced:\n{diverged}"));
    }
    for both in [WIDE, OTHER] {
        if !diverged.contains(both) {
            return Err(format!("the divergent line drops {both:?}:\n{diverged}"));
        }
    }
    if diverged.find(WIDE) > diverged.find(OTHER) {
        return Err(format!("the divergent line reads alone-then-wide:\n{diverged}"));
    }

    // The host stopping during the retry is neither, and a retry that never
    // reported is not a verdict about anything.
    let suspended = Outcome {
        suspended: common::clock::SUSPENDED_AT_LEAST,
        ..red(OTHER)
    };
    let asleep = alone_line("a_test", WIDE, false, Some(&suspended));
    if !asleep.contains("the host was suspended during the retry too") {
        return Err(format!("a suspended retry reads as a verdict:\n{asleep}"));
    }
    let missing = alone_line("a_test", WIDE, false, None);
    if !missing.contains("the lone run reported nothing about it") {
        return Err(format!("a retry that reported nothing reads as a verdict:\n{missing}"));
    }
    Ok(())
}

/// One live guest holds its lane's NVMe image, and the next one may not.
///
/// **The overlap this stages is the one the shared-boot reboot used to
/// produce.** `qemu = boot()` evaluates its right-hand side first, so the
/// replacement was launched while the guest it replaced still held the lane's
/// `test-nvme-*.img` open for write; QEMU's second process exited 1 on its own
/// image lock, `wait_for_ready` panicked, and the panic escaped the shared
/// block — 129 of one run's 131 reds on one sentence, 2026-08-17.
///
/// The ordering itself is now the type's: `boot` takes a [`qemu::LaneFree`] and
/// the only thing that makes one out of a guest is `QemuInstance::shutdown`,
/// which takes it by value. What is left to check at runtime is the claim
/// underneath — that a hold is real while a guest is up and gone once it is
/// not — and this checks it on the harness's own registry, in both directions,
/// with no guest.
fn nvme_image_is_held_by_one_guest() -> Result<(), String> {
    // Names, not files: a claim is a hold on a path and touches no disk, so
    // nothing here has to create or delete a hundred megabytes to ask.
    let dir = common::lane::dir();
    let image = dir.join("nvme-claim-gate.img");
    let other = dir.join("nvme-claim-gate-other.img");

    let held = qemu::NvmeClaim::take(&image).map_err(|why| {
        format!("a free image refused its first guest: {why}")
    })?;

    // The overlap. This is the direction that must red, and it is what the
    // reboot produced.
    match qemu::NvmeClaim::take(&image) {
        Ok(_) => {
            return Err(format!(
                "a second guest took {}, which a live one is holding — two QEMUs are then \
                 handed one image and the second dies on its lock",
                image.display()
            ))
        }
        Err(why) => {
            // The refusal has to name the image, or it cannot be acted on: a
            // run makes dozens of guests and the message is all a reader gets.
            if !why.contains(&image.display().to_string()) {
                return Err(format!("the refusal does not name the image it is about: {why}"));
            }
        }
    }

    // A different image is not a conflict, or every lane would refuse every
    // other lane's boot the moment this gate had teeth.
    let elsewhere = qemu::NvmeClaim::take(&other)
        .map_err(|why| format!("an unheld image was refused: {why}"))?;
    drop(elsewhere);

    // And the ordinary reboot: the replacement takes the image the guest it
    // replaces released. Green, and it is the half a fix that simply refused
    // every second boot would break.
    drop(held);
    let replacement = qemu::NvmeClaim::take(&image).map_err(|why| {
        format!("a replacement was refused the image its predecessor released: {why}")
    })?;
    drop(replacement);
    Ok(())
}

/// A blown guard stays red, and stops reading as an answer.
///
/// Both halves, because each fails the other's way round. An implementation
/// that made a stall its own non-red status would hide a guest that genuinely
/// stops; one that only renamed the line would leave the summary saying a test
/// found something. Staged against the strings a wait actually produces rather
/// than against the marker on its own, because a caller prefixes its own
/// sentence to [`await_marker`]'s and the classification has to survive that.
fn stall_is_not_a_verdict() -> Result<(), String> {
    // Built from the marker rather than copied, so a rename cannot leave the
    // gate asserting against a string nothing produces any more.
    let real = format!("{STALLED} waiting for the long tone to start — it went quiet");
    let under_a_sentence = format!("the compositor stopped painting\n{real}");
    let cases: [(&str, Option<&str>, bool); 4] = [
        ("an ordinary red", Some("the pointer never moved right"), false),
        ("a wait that expired", Some(real.as_str()), true),
        (
            "a wait that expired under a caller's own sentence",
            Some(under_a_sentence.as_str()),
            true,
        ),
        ("a pass", None, false),
    ];
    for (what, reason, want_stall) in cases {
        let outcome = Outcome {
            name: what.to_string(),
            reason: reason.map(str::to_string),
            elapsed: Duration::from_secs(1),
            suspended: Duration::ZERO,
        };
        if outcome.stalled() != want_stall {
            return Err(format!(
                "{what} reads as stalled={}, and it has to be {want_stall}",
                outcome.stalled()
            ));
        }
        // Red is red. A stall that stopped failing the run would be a gate that
        // reports and enforces nothing.
        let red = matches!(outcome.verdict_against(&[]), Verdict::Fail(_));
        if red != reason.is_some() {
            return Err(format!("{what} is red={red}, and a reason is always red"));
        }
    }

    let mut tally = Tally::new(&[], Day::today());
    tally.record(Outcome {
        name: "a_stalled_test".to_string(),
        reason: Some(format!("{STALLED} waiting for nothing at all — it went quiet")),
        elapsed: Duration::from_secs(1),
        suspended: Duration::ZERO,
    });
    tally.record(Outcome {
        name: "a_wrong_answer".to_string(),
        reason: Some("the pointer never moved right".to_string()),
        elapsed: Duration::from_secs(1),
        suspended: Duration::ZERO,
    });
    if tally.exit_code() != 1 {
        return Err(format!("two reds exited {}, and they have to red", tally.exit_code()));
    }
    if tally.stalls != ["a_stalled_test"] {
        return Err(format!(
            "the run named {:?} as blown guards; it has to name exactly the one that was",
            tally.stalls
        ));
    }
    let summary = tally.summary(2, Duration::from_secs(2), Duration::ZERO);
    if !summary.contains("1 of those reds are blown liveness guards") {
        return Err(format!("the summary does not separate the two kinds of red:\n{summary}"));
    }
    Ok(())
}

/// Whether a run that did not attempt most of the suite's measured cost says so.
///
/// **The failure mode the tier introduces is silence, not a wrong answer.** A
/// green run holding back 60 tests and a green run holding back none print the
/// same word, and the difference between them is the whole reason `--nightly`
/// exists. `src/tiers.rs`'s gates hold the declaration against the measured
/// profile and `check_registration` holds it against the registration; neither
/// can see whether the *run* mentions it, and a run nobody can tell apart from a
/// full one is how a temporary measure becomes permanent.
///
/// Both directions, because the second is the one that rots quietly: a suite
/// that ran everything must not claim to have held anything back either, or the
/// line stops carrying information the day somebody makes it unconditional.
fn nightly_tier_is_announced() -> Result<(), String> {
    let held: [&str; 2] = ["desktop_window_child", "sshd_fail_closed"];
    let announced =
        Tally::new(&[], Day::today()).holding_back(&held).summary(1, Duration::ZERO, Duration::ZERO);
    for want in [
        "not run — the nightly tier",
        "desktop_window_child, sshd_fail_closed",
        "`cargo test --test toyos-build -- --nightly` runs them",
        "src/tiers.rs",
        "2 held back for the nightly tier",
    ] {
        if !announced.contains(want) {
            return Err(format!("a run holding tests back never says {want:?}:\n{announced}"));
        }
    }
    // The cost, added up from `RELEGATED` rather than from anything this
    // function knows: a summary quoting a number the declaration does not
    // support is worse than one quoting none.
    let ms: u64 = tiers::RELEGATED
        .iter()
        .filter(|r| held.contains(&r.test))
        .map(|r| r.ci_ms)
        .sum();
    let want = format!("{:.1} s of effective CI test time", ms as f64 / 1000.0);
    if !announced.contains(&want) {
        return Err(format!("the summary does not price what it held back as {want:?}:\n{announced}"));
    }

    let whole = Tally::new(&[], Day::today()).summary(1, Duration::ZERO, Duration::ZERO);
    if whole.contains("nightly") || whole.contains("held back") {
        return Err(format!("a run that held nothing back says it did:\n{whole}"));
    }
    Ok(())
}

/// What a suspend is worth to a verdict, staged rather than reasoned about.
///
/// `common::clock::self_check` gates the detector; this gates what the suite
/// does with what it detects. Both halves are needed and neither implies the
/// other: **a suspend that silently passes is as bad as one that silently
/// fails**, and here the two are one line apart.
fn suspend_invalidates_a_verdict() -> Result<(), String> {
    let slept = common::clock::SUSPENDED_AT_LEAST + Duration::from_secs(120);
    let awake = Duration::ZERO;
    // Under the threshold on purpose: two clock reads jitter against each other
    // by microseconds, and a run must not be thrown away for that.
    let jitter = common::clock::SUSPENDED_AT_LEAST
        .checked_sub(Duration::from_millis(1))
        .expect("SUSPENDED_AT_LEAST must be at least 1ms for this case to mean anything");
    let cases: [(&str, Option<&str>, Duration, Verdict); 6] = [
        ("a pass on a host that stayed up", None, awake, Verdict::Pass(None)),
        ("a fail on a host that stayed up", Some("the guest said no"), awake, Verdict::Fail(None)),
        ("a pass across a suspend", None, slept, Verdict::Invalid),
        ("a fail across a suspend", Some("timed out"), slept, Verdict::Invalid),
        ("a pass across clock jitter", None, jitter, Verdict::Pass(None)),
        ("a fail across clock jitter", Some("the guest said no"), jitter, Verdict::Fail(None)),
    ];
    for (what, reason, suspended, want) in cases {
        let outcome = Outcome {
            name: what.to_string(),
            reason: reason.map(str::to_string),
            elapsed: Duration::from_secs(3),
            suspended,
        };
        let got = outcome.verdict();
        if got != want {
            return Err(format!("{what} is {got:?}, and it has to be {want:?}"));
        }
    }
    Ok(())
}

/// What a run has established, as it establishes it.
///
/// One place rather than five counters in `main`, because the interesting part
/// is not any one of them but the arithmetic between them — which reds, which
/// only reports, and what the process exits with. [`Tally::exit_code`] and
/// [`Tally::summary`] are that arithmetic, and both are gated.
struct Tally {
    expected: &'static [ExpectedFailure],
    passed: usize,
    failures: Vec<(String, String)>,
    /// The subset of `failures` whose guard expired rather than whose assertion
    /// failed, by name. Red like any other — and named apart, because a run
    /// that never got the guest going has measured the host and not the tree.
    stalls: Vec<String>,
    /// A listed test that failed. Reported, never red.
    fired: Vec<(String, &'static ExpectedFailure)>,
    /// A listed test that passed where its entry says a pass is the proof. Red.
    stale: Vec<&'static ExpectedFailure>,
    /// A listed test that passed where its entry says a pass proves nothing.
    /// Not red, and reported: a green of a known-red test is not a fix.
    quiet: Vec<(String, &'static ExpectedFailure)>,
    /// An entry whose own review date has arrived. Red, and independent of what
    /// ran: the declaration expired whether or not this run touched the test.
    expired: Vec<(&'static ExpectedFailure, String)>,
    invalid: Vec<(String, Duration)>,
    /// What the tier held back, by name. Not a verdict and never red — it is the
    /// one thing a reader of the last line cannot infer from anything else in
    /// it, because a run that skipped a third of its cost looks exactly like a
    /// run that had nothing to do.
    relegated: Vec<&'static str>,
}

impl Tally {
    fn new(expected: &'static [ExpectedFailure], today: Day) -> Self {
        Tally {
            expected,
            passed: 0,
            failures: Vec::new(),
            stalls: Vec::new(),
            fired: Vec::new(),
            stale: Vec::new(),
            quiet: Vec::new(),
            expired: expected
                .iter()
                .filter_map(|e| e.expired(today).map(|why| (e, why)))
                .collect(),
            invalid: Vec::new(),
            relegated: Vec::new(),
        }
    }

    /// The names this run's tier filter took out, for the summary to say so.
    fn holding_back(mut self, names: &[&'static str]) -> Self {
        self.relegated = names.to_vec();
        self
    }

    fn record(&mut self, outcome: Outcome) {
        let verdict = outcome.verdict_against(self.expected);
        let summary = || headline(outcome.reason.as_deref());
        match verdict {
            Verdict::Pass(None) => self.passed += 1,
            Verdict::Pass(Some(entry)) => {
                self.passed += 1;
                self.quiet.push((outcome.name.clone(), entry));
            }
            Verdict::Fail(None) => {
                if outcome.stalled() {
                    self.stalls.push(outcome.name.clone());
                }
                self.failures.push((outcome.name.clone(), summary()));
            }
            Verdict::Fail(Some(entry)) => {
                if outcome.stalled() {
                    self.stalls.push(outcome.name.clone());
                }
                self.failures.push((
                    outcome.name.clone(),
                    format!(
                        "{} — and this is NOT #{}'s failure, so the entry does not cover it",
                        summary(),
                        entry.task
                    ),
                ));
            }
            Verdict::Expected(entry) => self.fired.push((outcome.name.clone(), entry)),
            Verdict::Stale(entry) => self.stale.push(entry),
            Verdict::Invalid => self.invalid.push((outcome.name.clone(), outcome.suspended)),
        }
    }

    /// **Three statuses, and an expected failure is none of them.**
    ///
    /// It never reaches this function, which is the statement: a run whose only
    /// reds were declared, reviewed and pending on a task has established that
    /// this tree is what the declaration says it is, and that is exit 0. What a
    /// stale entry establishes is the opposite — the declaration is wrong about
    /// this tree — so it is exit 1 beside any other red.
    ///
    /// 2 keeps its existing meaning untouched: the run established nothing,
    /// because the host stopped in the middle of it.
    fn exit_code(&self) -> i32 {
        if !self.failures.is_empty() || !self.stale.is_empty() || !self.expired.is_empty() {
            return 1;
        }
        if !self.invalid.is_empty() {
            return 2;
        }
        0
    }

    /// Everything the run has to say, as one block, ending in the result line.
    ///
    /// A string rather than a pile of `eprintln!`s so that the gate can read
    /// what an agent reads. **The result line names every expected failure that
    /// fired**: the whole hazard of this mechanism is a run that looks clean
    /// because nobody scrolled up.
    fn summary(&self, total: usize, elapsed: Duration, suspended: Duration) -> String {
        let mut out = String::new();
        let mut say = |line: String| {
            out.push_str(&line);
            out.push('\n');
        };
        say(String::new());
        // What the run's liveness ceilings were actually worth, because the
        // number in the source is no longer the number that was enforced. A
        // reader comparing two runs' timings needs to know which host each was
        // taken on, and this is the suite's own measurement of that.
        let (fastest, reference, num, den) = qemu::host_speed();
        if let Some(fastest) = fastest {
            say(format!(
                "host: fastest boot {fastest} ms against the reference {reference} ms — liveness \
                 ceilings paid at {:.2}x width",
                f64::from(num) / f64::from(den)
            ));
        }
        if suspended >= common::clock::SUSPENDED_AT_LEAST {
            // The elapsed figure below is monotonic and therefore already
            // excludes it, which is worth saying: the two numbers do not add up
            // unless a reader knows that.
            say(format!(
                "note: the host was suspended for {suspended:.0?} during this run. \
                 The suite time below excludes it."
            ));
        }
        if !self.failures.is_empty() {
            say("failures:".to_string());
            for (name, reason) in &self.failures {
                say(format!("    {name}: {reason}"));
            }
            say(String::new());
        }
        if !self.stalls.is_empty() {
            say(format!(
                "{} of those reds are blown liveness guards, not answers: {}",
                self.stalls.len(),
                self.stalls.join(", ")
            ));
            say(
                "    The guest stopped making progress, so the run established nothing \
                 about this tree and there is nothing in it to bisect. Re-run; if one \
                 recurs with the host to itself, the guest really is stopping."
                    .to_string(),
            );
            say(String::new());
        }
        if !self.fired.is_empty() {
            say("expected failures — open defects this run reproduced:".to_string());
            for (name, entry) in &self.fired {
                say(format!("    {name}  #{}  {}", entry.task, entry.spec));
            }
            say(
                "    The exemption pins which assertion failed, not why. Read the \
                 pointer above before treating one as accounted for."
                    .to_string(),
            );
            say(String::new());
        }
        if !self.quiet.is_empty() {
            say("expected failures that did not fire — this proves nothing:".to_string());
            for (name, entry) in &self.quiet {
                say(format!("    {name}  #{}  {}", entry.task, entry.spec));
            }
            say(
                "    Each is intermittent by its own entry, so one green run is one \
                 sample. Do not close anything on it."
                    .to_string(),
            );
            say(String::new());
        }
        if !self.stale.is_empty() {
            say("stale expected-failure entries — these tests PASSED:".to_string());
            for entry in &self.stale {
                say(format!("    {}  #{}  {}", entry.test, entry.task, entry.spec));
            }
            say(
                "    Delete the entry if the defect is fixed. If it is not fixed and \
                 this test passes anyway, the failure was never reproducible and the \
                 entry should have said so."
                    .to_string(),
            );
            say(String::new());
        }
        if !self.expired.is_empty() {
            say("expected-failure entries past their review date:".to_string());
            for (entry, why) in &self.expired {
                say(format!("    {}  #{}  {why}", entry.test, entry.task));
                say(format!("        {}", entry.spec));
            }
            say(String::new());
        }
        if !self.invalid.is_empty() {
            say("invalidated by host suspend:".to_string());
            for (name, slept) in &self.invalid {
                say(format!("    {name}: the host was stopped for {slept:.0?} while it ran"));
            }
            say(String::new());
        }

        // Above the result line and not below it, so the pointer is the last
        // thing before the verdict rather than an afterthought under it.
        if !self.relegated.is_empty() {
            let ms: u64 = tiers::RELEGATED
                .iter()
                .filter(|r| self.relegated.contains(&r.test))
                .map(|r| r.ci_ms)
                .sum();
            say(format!(
                "not run — the nightly tier, {:.1} s of effective CI test time:",
                ms as f64 / 1000.0
            ));
            say(format!("    {}", self.relegated.join(", ")));
            say(
                "    `cargo test --test toyos-build -- --nightly` runs them. \
                 `src/tiers.rs`'s `RELEGATED` says what each one guarded and why it is not \
                 gated per pull request."
                    .to_string(),
            );
            say(String::new());
        }

        let expected_note = if self.fired.is_empty() {
            String::new()
        } else {
            let named: Vec<String> =
                self.fired.iter().map(|(n, e)| format!("{n} (#{})", e.task)).collect();
            format!(", {} expected: {}", self.fired.len(), named.join(", "))
        };
        // **In the result line, because that is the line a shard's job summary
        // extracts and the line anybody reads.** A count of what ran means
        // something different depending on how much was not attempted.
        let held = if self.relegated.is_empty() {
            String::new()
        } else {
            format!(", {} held back for the nightly tier", self.relegated.len())
        };
        match self.exit_code() {
            1 => say(format!(
                "test result: FAILED. {} passed, {} failed, {} stale or expired \
                 expected-failure entries{expected_note}, {} invalidated, {total} total \
                 ({elapsed:.1?}){held}",
                self.passed,
                self.failures.len(),
                self.stale.len() + self.expired.len(),
                self.invalid.len(),
            )),
            2 => {
                say(format!(
                    "test result: INVALID. {} passed{expected_note}, {} invalidated by a \
                     host suspend of {suspended:.0?}, {total} total ({elapsed:.1?}){held}",
                    self.passed,
                    self.invalid.len(),
                ));
                say(
                    "This is not a red. The machine stopped mid-run, so those verdicts \
                     are of nothing; re-run the suite."
                        .to_string(),
                );
            }
            _ if !self.fired.is_empty() => say(format!(
                "test result: ok, NOT clean. {} passed{expected_note}, {total} total \
                 ({elapsed:.1?}){held}",
                self.passed,
            )),
            _ => say(format!(
                "test result: ok. {} passed, {total} total ({elapsed:.1?}){held}",
                self.passed
            )),
        }
        out
    }
}

/// Every claim [`EXPECTED_FAILURES`] makes about itself, against the names this
/// run can actually produce a verdict for.
///
/// `runnable` is the whole registry rather than the two const lists, because the
/// shared boot's C and Rust tests are discovered and a name that only exists
/// there must still be listable.
fn check_expected_failures(
    expected: &'static [ExpectedFailure],
    runnable: &BTreeSet<&str>,
) -> Result<(), String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for entry in expected {
        if !seen.insert(entry.test) {
            return Err(format!("{} has two expected-failure entries", entry.test));
        }
        if !runnable.contains(entry.test) {
            return Err(format!(
                "{} is expected to fail and no list registers it — a renamed or deleted \
                 test must take its entry with it, or the exemption is waiting for \
                 whatever gets that name next",
                entry.test
            ));
        }
        if entry.says.is_empty() {
            return Err(format!(
                "{}'s entry names no failure, so it would absorb every red that test \
                 can produce",
                entry.test
            ));
        }
        if let Stale::OnThisDate(date) = entry.stale {
            if Day::parse(date).is_none() {
                return Err(format!(
                    "{}'s review date {date:?} is not a YYYY-MM-DD date, so the entry \
                     would never expire",
                    entry.test
                ));
            }
        }
    }
    Ok(())
}

/// What the declaration decides about one outcome, in both directions.
///
/// The anti-rot property is the third case and it is the reason the mechanism is
/// safe: **a listed test that passes is a red run.** The rest are its negative
/// controls — an unlisted failure is still an ordinary red, and a listed test
/// failing some *other* way is too.
fn expected_failure_verdicts() -> Result<(), String> {
    static LISTED: &[ExpectedFailure] = &[
        ExpectedFailure {
            test: "fails_every_run",
            task: 4242,
            spec: "nowhere.md",
            says: &["the guest never answered", "the shell never answered again"],
            stale: Stale::OnAPass,
        },
        ExpectedFailure {
            test: "fails_sometimes",
            task: 4243,
            spec: "nowhere.md",
            says: &["the shell never answered again"],
            stale: Stale::OnThisDate("2999-01-01"),
        },
    ];
    let awake = Duration::ZERO;
    let slept = common::clock::SUSPENDED_AT_LEAST + Duration::from_secs(120);
    let (every, sometimes) = (&LISTED[0], &LISTED[1]);
    let cases: [(&str, &str, Option<&str>, Duration, Verdict); 9] = [
        (
            "a listed test failing the way its entry says",
            "fails_every_run",
            Some("round 2: the shell never answered again:\n<log>"),
            awake,
            Verdict::Expected(every),
        ),
        (
            "the entry's second alternative",
            "fails_every_run",
            Some("the guest never answered"),
            awake,
            Verdict::Expected(every),
        ),
        // The anti-rot property, at the verdict where it is decided.
        (
            "a test whose entry says a pass is the proof, passing",
            "fails_every_run",
            None,
            awake,
            Verdict::Stale(every),
        ),
        // And the reason the property is not unconditional: one green of an
        // intermittent test is one sample, so it may not red the run.
        (
            "a test whose entry says a pass proves nothing, passing",
            "fails_sometimes",
            None,
            awake,
            Verdict::Pass(Some(sometimes)),
        ),
        (
            "that same test failing the way its entry says",
            "fails_sometimes",
            Some("the shell never answered again"),
            awake,
            Verdict::Expected(sometimes),
        ),
        (
            "a listed test failing some other way",
            "fails_every_run",
            Some("the client binary was not built"),
            awake,
            Verdict::Fail(Some(every)),
        ),
        (
            "an unlisted test failing the same way",
            "some_other_test",
            Some("the shell never answered again"),
            awake,
            Verdict::Fail(None),
        ),
        ("an unlisted test passing", "some_other_test", None, awake, Verdict::Pass(None)),
        (
            "a listed test across a host suspend",
            "fails_every_run",
            Some("the shell never answered again"),
            slept,
            Verdict::Invalid,
        ),
    ];
    for (what, name, reason, suspended, want) in cases {
        let outcome = Outcome {
            name: name.to_string(),
            reason: reason.map(str::to_string),
            elapsed: Duration::from_secs(3),
            suspended,
        };
        let got = outcome.verdict_against(LISTED);
        if got != want {
            return Err(format!("{what} is {got:?}, and it has to be {want:?}"));
        }
    }
    Ok(())
}

/// What a whole run exits with, and what its last line says.
///
/// Driven through [`Tally`] rather than asserted about it: the property that
/// matters is what `--land`'s gate reads off the process, and that is the exit
/// code after `record` has seen every outcome.
fn expected_failure_exit_status() -> Result<(), String> {
    static LISTED: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_test_pending_on_a_defect",
        task: 4242,
        spec: "nowhere.md §9",
        says: &["the shell never answered again"],
        stale: Stale::OnAPass,
    }];
    static EXPIRED: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_test_pending_on_a_defect",
        task: 4242,
        spec: "nowhere.md §9",
        says: &["the shell never answered again"],
        stale: Stale::OnThisDate("2020-02-29"),
    }];
    let today = Day::parse("2026-08-06").expect("a date this file wrote");
    let outcome = |name: &str, reason: Option<&str>| Outcome {
        name: name.to_string(),
        reason: reason.map(str::to_string),
        elapsed: Duration::from_secs(3),
        suspended: Duration::ZERO,
    };
    let expected_fired = || outcome("a_test_pending_on_a_defect", Some("the shell never answered again:\n<log>"));

    let mut only_expected = Tally::new(LISTED, today);
    only_expected.record(outcome("something_else", None));
    only_expected.record(expected_fired());
    let text = only_expected.summary(2, Duration::from_secs(9), Duration::ZERO);
    if only_expected.exit_code() != 0 {
        return Err(format!(
            "a run whose only red was declared exits {}, and it has to be 0:\n{text}",
            only_expected.exit_code()
        ));
    }
    // The whole hazard is a run that reads as clean. The result line is the one
    // line every reader and every log-scraper looks at, so it is the line that
    // has to carry it.
    let result = text.lines().last().unwrap_or_default();
    for wanted in ["a_test_pending_on_a_defect", "#4242", "NOT clean"] {
        if !result.contains(wanted) {
            return Err(format!("the result line does not say {wanted:?}: {result}"));
        }
    }
    if !text.contains("nowhere.md §9") {
        return Err(format!("the report never points at where the defect is written up:\n{text}"));
    }

    // The anti-rot property, end to end: nothing failed, and the run is red.
    let mut nothing_failed = Tally::new(LISTED, today);
    nothing_failed.record(outcome("something_else", None));
    nothing_failed.record(outcome("a_test_pending_on_a_defect", None));
    let text = nothing_failed.summary(2, Duration::from_secs(9), Duration::ZERO);
    if nothing_failed.exit_code() != 1 {
        return Err(format!(
            "a run where the expected failure PASSED exits {}, and it has to be 1:\n{text}",
            nothing_failed.exit_code()
        ));
    }
    for wanted in ["a_test_pending_on_a_defect", "#4242", "stale"] {
        if !text.contains(wanted) {
            return Err(format!("a stale entry is not reported as {wanted:?}:\n{text}"));
        }
    }

    // Negative control for both: an undeclared red is still an ordinary red,
    // and a declared one beside it does not soften the status.
    let mut real_red = Tally::new(LISTED, today);
    real_red.record(outcome("something_else", Some("the disk came back short")));
    real_red.record(expected_fired());
    if real_red.exit_code() != 1 {
        return Err(format!(
            "a run with an undeclared red exits {}, and it has to be 1",
            real_red.exit_code()
        ));
    }

    // And a listed test failing some other way: the exemption must not reach it.
    let mut wrong_failure = Tally::new(LISTED, today);
    wrong_failure.record(outcome("a_test_pending_on_a_defect", Some("the client was not built")));
    let text = wrong_failure.summary(1, Duration::from_secs(9), Duration::ZERO);
    if wrong_failure.exit_code() != 1 {
        return Err(format!(
            "a listed test failing another way exits {}, and it has to be 1:\n{text}",
            wrong_failure.exit_code()
        ));
    }
    if !text.contains("NOT #4242's failure") {
        return Err(format!("the report does not say why the entry did not cover it:\n{text}"));
    }

    // Exit 2 keeps its meaning: a suspended run establishes nothing, and a
    // declared red inside it is not an answer either.
    let mut suspended = Tally::new(LISTED, today);
    suspended.record(Outcome {
        name: "something_else".to_string(),
        reason: None,
        elapsed: Duration::from_secs(3),
        suspended: common::clock::SUSPENDED_AT_LEAST + Duration::from_secs(120),
    });
    if suspended.exit_code() != 2 {
        return Err(format!(
            "a suspended run exits {}, and it has to be 2",
            suspended.exit_code()
        ));
    }

    // The clean case, so that none of the above is passing because everything
    // reds.
    let mut clean = Tally::new(LISTED, today);
    clean.record(outcome("something_else", None));
    let text = clean.summary(1, Duration::from_secs(9), Duration::ZERO);
    if clean.exit_code() != 0 {
        return Err(format!("a clean run exits {}, and it has to be 0", clean.exit_code()));
    }
    if !text.lines().last().unwrap_or_default().starts_with("test result: ok.") {
        return Err(format!("a clean run does not say so plainly:\n{text}"));
    }

    // The anti-rot property for the entries a pass cannot judge: the review date
    // arrives, and it reds whether or not the test ran at all.
    let mut past_review = Tally::new(EXPIRED, today);
    past_review.record(outcome("something_else", None));
    let text = past_review.summary(1, Duration::from_secs(9), Duration::ZERO);
    if past_review.exit_code() != 1 {
        return Err(format!(
            "an entry past its review date exits {}, and it has to be 1:\n{text}",
            past_review.exit_code()
        ));
    }
    if !text.contains("2020-02-29") || !text.contains("review date") {
        return Err(format!("an expired entry is not reported as one:\n{text}"));
    }
    // Negative control: the same entry with a date ahead of the run is silent.
    let before_review = Tally::new(EXPIRED, Day::parse("2020-02-28").expect("a date this file wrote"));
    if before_review.exit_code() != 0 {
        return Err("an entry whose review date has not arrived reds anyway".to_string());
    }
    Ok(())
}

/// A wait that hands a death spelling to a scan of its own, asked of the
/// harness's source.
///
/// **The one place the vocabulary lives is the whole of the fix, so this is
/// what keeps it the one place.** `tests/common/qemu.rs` holds three waits on a
/// guest and they used to disagree: the boot half ended on three spellings, the
/// test half on one and `await_guest` on none at all — so a Rust `panic!` in the
/// kernel matched nothing while a test was running, the machine halted every
/// CPU, and the guard expired onto a verdict saying the guest had stopped
/// answering. All three ask `serial::died` now, which is the only thing in the
/// harness that knows the words and the only thing that knows the prefix decides
/// whose death they report.
/// `issues/build/every-recorded-stall-predates-the-panic-discriminator.md`
/// is what the years before it are worth.
///
/// The way that comes back is the obvious patch: one more spelling handed
/// straight to a `contains` beside the call. It would match a *program's* panic
/// as readily as the kernel's and take the run down with a guest binary that
/// was expected to die — so the shape is refused by name rather than left to a
/// reviewer. Comment lines go first: this file argues about these words at
/// length, and prose is not a second answer.
fn hand_rolled_deaths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for word in serial::spellings() {
            // The shape is the spelling as somebody's first argument —
            // `contains`, `starts_with`, `find`, any of them. A spelling
            // *inside* a longer staged line is how this file's own gates build
            // their inputs, and those are not scans.
            if line.contains(&format!("(\"{word}")) {
                found.push(format!("{}:{}: {}", n + 1, word, line.trim()));
            }
        }
    }
    found
}

/// [`hand_rolled_deaths`] over the file that has to stay clean, with its own
/// bad input beside it so a check that stopped finding anything says so.
fn one_vocabulary() -> Result<(), String> {
    const FILE: &str = "tests/common/qemu.rs";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FILE);
    let text = fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let found = hand_rolled_deaths(&text);
    if !found.is_empty() {
        return Err(format!(
            "{FILE} scans for a death spelling itself, and `serial::died` is where that is \
             decided for every wait at once — a second answer here is what let a kernel panic \
             read as a stall, and it matches a program's own panic besides:\n  {}",
            found.join("\n  ")
        ));
    }
    // The negative control. Every line of it is a shape this must name, and the
    // last two are shapes it must not: prose, and a staged capture built out of
    // the same words.
    let staged = "\
        } else if line.contains(\"KERNEL PANIC\") {\n\
        if line.starts_with(\"SEGFAULT\") {\n\
        // ends on `PANIC:` and nothing else, which is the defect\n\
        const KERNEL: &str = \"[kernel 1.450 cpu3] PANIC: panicked at reserve.rs:812:9:\";\n";
    let named = hand_rolled_deaths(staged);
    if named.len() != 2 {
        return Err(format!(
            "the check names {} of the two hand-rolled scans staged for it: {named:?}",
            named.len()
        ));
    }
    eprintln!("  [vocabulary] {FILE} asks `serial::died` and nothing else");
    Ok(())
}

/// What the declaration itself has to be, before any of it means anything.
/// Which shared-boot binaries need `SYS_DEBUG`, asked of their source.
///
/// A name reaches the syscall directly, or through a child it spawns —
/// `panic_recovery`'s three actions are all `test_panic_child`'s, and a rule
/// that only read the test's own source would miss the one test in the list
/// whose whole subject is the syscall.
fn needs_actuators(sources: &[(String, String)], registry: &[&str]) -> BTreeSet<String> {
    // The third spelling is the SDK's: `toyos::census` calls `debug_with` on the
    // caller's behalf, so a binary whose leak assertion is a census names no
    // syscall of its own and reads as innocent to the two above.
    let calls = |text: &str| {
        text.contains("SYS_DEBUG")
            || text.contains("syscall::debug(")
            || text.contains("census::Census")
    };
    let direct: BTreeSet<&str> =
        sources.iter().filter(|(_, t)| calls(t)).map(|(n, _)| n.as_str()).collect();
    let mut out = BTreeSet::new();
    for (name, text) in sources {
        if !registry.contains(&name.as_str()) {
            continue;
        }
        let spawns = direct.iter().any(|d| text.contains(&format!("test_rs_{d}")));
        if direct.contains(name.as_str()) || spawns {
            out.insert(name.clone());
        }
    }
    out
}

/// [`ACTUATOR_TESTS`] is exactly the shared-boot binaries that reach
/// `SYS_DEBUG`, and the binaries are what is asked.
///
/// **What this does not cover, stated because the hole is real:** a machine or
/// screen test that *drives* one of those binaries on a boot of its own.
/// `screen_recoverable_untouched` was the instance — it runs
/// `test_rs_test_panic_child` on a featureless kernel, where action 0 is answered
/// `InvalidArgument` and the child exits 0 — and no static rule here can say
/// which `BootOptions` a `run_test` call belongs to. What answers it instead is
/// the guest: `test_panic_child` names `InvalidArgument` as *this kernel carries
/// no actuators* rather than reporting a kernel that failed to kill anybody, so
/// the red says what is wrong wherever it happens.
///
/// **Both directions are the point.** A binary that gains a `debug()` call and
/// no entry would run on the shipping kernel, where the syscall answers
/// `InvalidArgument` — and a test whose verdict is that a process died would
/// then fail for a reason with nothing to do with what it is about. An entry
/// whose binary no longer calls it is a test kept off the shipping kernel for
/// nothing, which is the erosion this split exists to stop.
fn suite_split() -> Result<(), String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/toyos-rust-tests/src/bin");
    let mut sources: Vec<(String, String)> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).map_err(|e| format!("read {name}: {e}"))?;
        sources.push((name, text));
    }
    let registry: Vec<&str> =
        sources.iter().map(|(n, _)| n.as_str()).filter(|n| !RUST_SKIP.contains(n)).collect();

    // The negative control, and it carries its own bad input: a binary that
    // calls the syscall and is on no list must be named, or the check above is
    // a spelling of `true`.
    let staged = vec![
        ("a_listed_one".to_string(), "syscall::debug(3)".to_string()),
        ("an_unlisted_one".to_string(), "SYS_DEBUG".to_string()),
        ("its_parent".to_string(), "Command::new(\"/bin/test_rs_an_unlisted_one\")".to_string()),
        ("a_censor".to_string(), "use toyos::census::Census;".to_string()),
        ("innocent".to_string(), "println!()".to_string()),
    ];
    let staged_registry =
        ["a_listed_one", "an_unlisted_one", "its_parent", "a_censor", "innocent"];
    let found = needs_actuators(&staged, &staged_registry);
    let want: BTreeSet<String> = ["a_listed_one", "an_unlisted_one", "its_parent", "a_censor"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if found != want {
        return Err(format!("the check does not work: on staged input it named {found:?}"));
    }

    let want: BTreeSet<String> = needs_actuators(&sources, &registry);
    let listed: BTreeSet<String> = ACTUATOR_TESTS.iter().map(|s| s.to_string()).collect();
    let missing: Vec<&String> = want.difference(&listed).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{missing:?} reach SYS_DEBUG and are on the shipping boot, where the syscall answers \
             InvalidArgument. Add each to ACTUATOR_TESTS, or to RUST_SKIP if it is driven rather \
             than run."
        ));
    }
    let stale: Vec<&String> = listed.difference(&want).collect();
    if !stale.is_empty() {
        return Err(format!(
            "{stale:?} are held off the shipping kernel and no longer reach SYS_DEBUG. Delete \
             each entry — coverage of the binary an image ships is what it costs."
        ));
    }
    println!(
        "  [split] {} shared binaries on the shipping kernel, {} on the actuator one",
        registry.len() - listed.len(),
        listed.len()
    );
    Ok(())
}

fn expected_failure_entries() -> Result<(), String> {
    static NAMED_NOTHING: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_test_that_was_renamed",
        task: 1,
        spec: "nowhere.md",
        says: &["something"],
        stale: Stale::OnAPass,
    }];
    static ABSORBS_EVERYTHING: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_real_test",
        task: 1,
        spec: "nowhere.md",
        says: &[],
        stale: Stale::OnAPass,
    }];
    static TWICE: &[ExpectedFailure] = &[
        ExpectedFailure { test: "a_real_test", task: 1, spec: "s", says: &["x"], stale: Stale::OnAPass },
        ExpectedFailure { test: "a_real_test", task: 2, spec: "s", says: &["y"], stale: Stale::OnAPass },
    ];
    static NEVER_EXPIRES: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_real_test",
        task: 1,
        spec: "nowhere.md",
        says: &["x"],
        stale: Stale::OnThisDate("next month"),
    }];
    static GOOD: &[ExpectedFailure] = &[ExpectedFailure {
        test: "a_real_test",
        task: 1,
        spec: "nowhere.md",
        says: &["x"],
        stale: Stale::OnThisDate("2026-09-06"),
    }];
    let runnable: BTreeSet<&str> = ["a_real_test", "another_real_test"].into_iter().collect();
    let refused: [(&str, &'static [ExpectedFailure], &str); 4] = [
        ("an entry for a test that no longer exists", NAMED_NOTHING, "no list registers it"),
        ("an entry that names no failure", ABSORBS_EVERYTHING, "names no failure"),
        ("two entries for one test", TWICE, "two expected-failure entries"),
        ("a review date nothing can read", NEVER_EXPIRES, "would never expire"),
    ];
    for (what, table, expect) in refused {
        match check_expected_failures(table, &runnable) {
            Ok(()) => return Err(format!("{what} was accepted")),
            Err(refusal) if !refusal.contains(expect) => {
                return Err(format!("{what} was refused, but for {refusal:?}"))
            }
            Err(_) => {}
        }
    }
    // The negative control: the check is refusing those three and not refusing
    // everything put in front of it.
    check_expected_failures(GOOD, &runnable)
        .map_err(|e| format!("a well-formed entry was refused: {e}"))?;
    check_expected_failures(&[], &runnable)
        .map_err(|e| format!("an empty declaration was refused: {e}"))?;

    // The calendar the whole `OnThisDate` half rests on. An entry that expires
    // on the wrong day is an entry that expires never or immediately, and
    // neither announces itself.
    let day = |s: &str| Day::parse(s).ok_or_else(|| format!("{s} did not parse"));
    if day("1970-01-01")? != Day(0) {
        return Err("the epoch is not day zero".to_string());
    }
    // A leap day, a century that is not a leap year, and one that is.
    for (date, want) in [("2024-02-29", 19782), ("1900-03-01", -25508), ("2000-03-01", 11017)] {
        if day(date)? != Day(want) {
            return Err(format!("{date} is {:?}, and it has to be Day({want})", day(date)?));
        }
    }
    if !(day("2026-08-06")? < day("2026-08-07")? && day("2026-12-31")? < day("2027-01-01")?) {
        return Err("dates do not order".to_string());
    }
    for bad in ["2026-8-06", "2026-08-6", "26-08-06", "2026/08/06", "2026-13-01", "2026-08-00", ""] {
        if Day::parse(bad).is_some() {
            return Err(format!("{bad:?} parsed as a date"));
        }
    }
    Ok(())
}

/// The task that would run `name` again, by itself.
///
/// **Every red from the parallel phase is re-run alone**, and the two possible
/// answers are both findings. Same verdict: the defect is real and the width had
/// nothing to do with it. Green: the test is red only when it shares the host,
/// which makes its [`Sched::Parallel`] wrong — a bug in this file, not in the
/// kernel, and one the suite has no other way to notice.
///
/// **A green retry does not turn the run green.** A rerun-only pass counting as
/// a pass is selective test running by the back door; the failure line
/// says which of the two it was and the run stays red until somebody fixes the
/// classification. That is the whole safety argument for widening the parallel
/// phase: getting a scheduling answer wrong costs a red run, never a quiet one.
///
/// A group member is re-run **as its group**, not on its own, so that the only
/// thing that changed between the two attempts is how many guests the host had.
fn retry_task<'a>(name: &str, all_tests: &[&'a TestDef]) -> Option<Task<'a>> {
    if let Some(def) = all_tests.iter().find(|t| t.name == name) {
        return Some(Task::Shared(vec![def], shared_kernel(name)));
    }
    if let Some((registered, _, _)) = SCREEN_TESTS.iter().find(|(n, _, _)| *n == name) {
        return Some(Task::Screen(registered));
    }
    let (registered, _, _) = MACHINE_TESTS.iter().find(|(n, _, _)| *n == name)?;
    let names = match group_of(registered) {
        None => vec![*registered],
        Some(group) => MACHINE_TESTS
            .iter()
            .filter(|(n, _, _)| group_of(n) == Some(group))
            .map(|(n, _, _)| *n)
            .collect(),
    };
    Some(Task::Machine(names))
}

/// Which of the two shared boots a name belongs on — a *kernel build*, because
/// `SYS_DEBUG` is compiled in or it is not, and never a boot parameter.
fn shared_kernel(name: &str) -> &'static [&'static str] {
    if ACTUATOR_TESTS.contains(&name) {
        ACTUATOR_KERNEL
    } else {
        &[]
    }
}

/// The binaries and config every task boots with.
struct Bins<'a> {
    test_config: &'a Path,
    c_bins: &'a [(String, Vec<u8>)],
    rust_bins: &'a [(String, Vec<u8>)],
}

fn run_task(task: Task<'_>, bins: &Bins<'_>, report: &std::sync::mpsc::Sender<Outcome>) {
    // Both clocks, at every test, because what the host did *between* two of
    // them is a different question from what it did during one: a lid closed
    // while nothing was running invalidates nothing.
    let send = |name: String, reason: Option<String>, start: common::clock::Mark| {
        let _ = report.send(Outcome {
            name,
            reason,
            elapsed: start.elapsed(),
            suspended: start.suspended(),
        });
    };
    match task {
        Task::Shared(tests, features) => {
            // The boot itself can fail, and it used to take the run with it.
            // Reporting the block's tests against its reason keeps the count
            // honest and says which one it died on.
            let mut done = 0usize;
            let outcome = catching(|| {
                // **The lane is the argument, and it is what makes the reboot
                // below unwritable in the order that broke it.** `boot` cannot
                // be called without a `LaneFree`, the only two things that
                // produce one are this line and `QemuInstance::shutdown`, and
                // `shutdown` takes the guest by value.
                let boot = |_: qemu::LaneFree| {
                    QemuInstance::boot_with_options(
                        bins.test_config,
                        bins.c_bins,
                        bins.rust_bins,
                        BootOptions { kernel_features: features, ..Default::default() },
                    )
                };
                let mut qemu = boot(qemu::LaneFree::no_guest_yet());
                let mut reboots = 0usize;
                for test in &tests {
                    let start = common::clock::mark();
                    let mut result = qemu.run_test(&test.qemu_name, test.timeout);
                    // **A guest that stopped answering is answered with a new
                    // one.** Its turn came, its whole ceiling passed, and it was
                    // never announced — so what this measured is the previous
                    // test's wreckage and not this one. Run `31241099454` is the
                    // bill: `abuse_gpu_resolution` took the shared boot with it
                    // and the 150 tests behind it each paid a full ceiling for a
                    // guest that was gone, 65 minutes of nothing and a job
                    // cancelled at 90.
                    //
                    // A reboot rather than an abandonment because every one of
                    // those tests still has a verdict owed to it, and the
                    // alternative is a suite that reports 150 reds it never ran.
                    // Bounded, because a block whose every member kills the
                    // guest must not boot one per test.
                    //
                    // **The old guest goes before the new one exists.** This
                    // was `qemu = boot()`, and Rust evaluates the right-hand
                    // side first: the replacement was launched, and waited on,
                    // while the instance it replaced still held the lane's
                    // `test-nvme-*.img` open for write. It exited 1 on QEMU's
                    // own image lock before saying anything, `wait_for_ready`
                    // panicked, and that panic escaped this block — so **every
                    // test still owed a verdict was reported red on it**. 129
                    // of one run's 131 reds carried that one sentence on
                    // 2026-08-17, against two real failures. The ordering is
                    // now the type's: `shutdown` takes the guest by value and
                    // is the only thing `boot` can be called with.
                    if result.boot_stopped_answering() && reboots < MAX_SHARED_REBOOTS {
                        reboots += 1;
                        eprintln!(
                            "  ---- the shared boot stopped answering before {}; rebooting \
                             ({reboots}/{MAX_SHARED_REBOOTS}) ----",
                            test.name
                        );
                        qemu = boot(qemu.shutdown());
                        result = qemu.run_test(&test.qemu_name, test.timeout);
                    }
                    // Between the test and its check, with the guest still up:
                    // see [`TestDef::settle`].
                    (test.settle)(&mut qemu, &mut result);
                    let reason = (!(test.check)(&result)).then(|| {
                        result
                            .error
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| format!("exit code {:?}", result.exit_code))
                    });
                    done += 1;
                    send(test.name.clone(), reason, start);
                }
                Ok(())
            });
            if let Err(reason) = outcome {
                for test in &tests[done..] {
                    send(test.name.clone(), Some(reason.clone()), common::clock::mark());
                }
            }
        }
        Task::Machine(names) => {
            // Dropped with the task, so no group's guest outlives the worker
            // that booted it.
            let mut held: Grouped = None;
            for name in names {
                let start = common::clock::mark();
                let outcome = catching(|| {
                    run_machine_test(name, bins.test_config, bins.c_bins, bins.rust_bins, &mut held)
                });
                // **A member that failed does not hand its guest on.** The
                // shared block answers a boot that stopped answering with a new
                // one; a group's guest is the same single point of failure and
                // that repair does not reach it, because a member's body is
                // arbitrary Rust with no `===TEST_START` for `started` to read.
                // What a group *does* have is a verdict per member, and one that
                // failed is reason enough not to make the next member's answer
                // about the same machine. Run `31250706113`: three members
                // behind one, `metal_sim_client_death` last and reported at
                // 364 s of a ceiling for a desktop that had gone.
                //
                // Bounded by the group — six members at most, so at most six
                // boots where there would have been one, and only on a red.
                if outcome.is_err() {
                    held = None;
                }
                send(name.to_string(), outcome.err(), start);
            }
        }
        Task::Screen(name) => {
            let start = common::clock::mark();
            let outcome =
                catching(|| run_screen_test(name, bins.test_config, bins.c_bins, bins.rust_bins));
            send(name.to_string(), outcome.err(), start);
        }
    }
}

impl Task<'_> {
    /// Every name this task will report an outcome for.
    fn names(&self) -> Vec<&str> {
        match self {
            Task::Shared(tests, _) => tests.iter().map(|t| t.name.as_str()).collect(),
            Task::Machine(names) => names.to_vec(),
            Task::Screen(name) => vec![name],
        }
    }
}

/// Where the last run in this worktree left what each test cost it.
///
/// Under `target/`, so it is per-worktree: on a single dev host repeating runs
/// it is a *hint* about how to order a queue and never an input to a verdict,
/// where a wrong number costs some idle lane time and a missing one costs
/// nothing at all. **A sharded run does not read it** — [`shard_pricing`]
/// says why the same claim does not hold once `target/` is a cache twelve
/// separate processes restore.
fn durations_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-durations")
}

/// The profile a checkout that has never run the suite starts from.
///
/// A machine with no measurement at all prices every test the same, and
/// [`Shard::keep`]'s LPT then degenerates to round-robin — which is what put 191
/// of 268 tests on one CI shard and cut it off at its job timeout while another
/// finished in sixteen minutes. Every runner is that
/// machine on every push, because a fresh clone has no `target/`.
///
/// Measured on a runner rather than here, deliberately: it is read by the
/// machines that have nothing else, and the dev host overrides it with its own
/// numbers the first time it runs the suite. Cross-arch TCG on an M4 Pro and
/// KVM on four Azure cores do not agree about which tests are long.
fn committed_durations_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/test-durations")
}

fn read_durations(path: &Path, out: &mut BTreeMap<String, Duration>) {
    let Ok(text) = fs::read_to_string(path) else { return };
    for line in text.lines() {
        if let Some((name, ms)) = line.rsplit_once(' ') {
            if let Ok(ms) = ms.parse::<u64>() {
                out.insert(name.to_string(), Duration::from_millis(ms));
            }
        }
    }
}

/// The committed profile, with whatever this worktree has measured on top.
///
/// Per name rather than per file, so a checkout that has only ever run a filter
/// keeps the committed number for everything that filter did not name.
fn load_durations() -> BTreeMap<String, Duration> {
    let mut out = BTreeMap::new();
    read_durations(&committed_durations_path(), &mut out);
    read_durations(&durations_path(), &mut out);
    out
}

/// What [`longest_first`] and [`Shard::keep`] price a task against.
///
/// **Committed only — never [`durations_path`]'s worktree overlay.** That
/// overlay lives under `target/`, which a sharded CI run restores from one
/// build cache a sibling job can still be writing: `cache-writer` here and a
/// same-commit sibling workflow's `cache-writer` both race the twelve shards'
/// own restores for the identical key, so one shard can land on a fresh save
/// and another on an older prefix match. `durations_path`'s own doc says a
/// wrong number "costs some idle lane time" — true only when every shard
/// prices a task the same wrong way. `Shard::keep` assumes exactly that
/// agreement, twelve independent processes over it do not have it to give,
/// and run `31617589126` is what two of them disagreeing on one number looks
/// like: shard 1 and shard 9 both priced `abuse_kernel_addr`'s task low
/// enough to take it, and `--merge-durations` refused the run for measuring
/// it twice. `tests/test-durations` is `actions/checkout`, not
/// `actions/cache`, and every shard checks out the identical bytes.
fn shard_pricing() -> BTreeMap<String, Duration> {
    let mut out = BTreeMap::new();
    read_durations(&committed_durations_path(), &mut out);
    out
}

/// Merge this run's durations into the recorded profile.
///
/// Merged rather than replaced, because a filtered run knows about four tests
/// and would otherwise throw away what the last full one measured.
///
/// **A shard writes somewhere nothing reads.** The partition is a function of
/// the profile, so a shard that saved would move it under its siblings — three
/// shards of `nvme_` in one worktree ran one test twice and one nowhere, and
/// every one of the three reported green. What a shard measures is still a
/// measurement, and the six of them are a partition of the suite, so it goes to
/// a file named for the shard that took it: [`load_durations`] never opens one,
/// and merging them into the committed profile is a deliberate act with a
/// command behind it.
fn save_durations(
    mut known: BTreeMap<String, Duration>,
    timed: &[(String, Duration)],
    shard: Option<Shard>,
) {
    for (name, elapsed) in timed {
        known.insert(name.clone(), *elapsed);
    }
    let (path, body) = match shard {
        None => (
            durations_path(),
            known.iter().map(|(n, d)| format!("{n} {}\n", d.as_millis())).collect::<String>(),
        ),
        // This shard's own tests and no others: a third of the suite is a third
        // of a measurement, and the merge is what makes it a whole one.
        Some(s) => {
            let mut mine: Vec<&(String, Duration)> = timed.iter().collect();
            mine.sort_by(|a, b| a.0.cmp(&b.0));
            (
                durations_path()
                    .with_file_name(format!("test-durations.shard-{}-of-{}", s.index, s.count)),
                mine.iter().map(|(n, d)| format!("{n} {}\n", d.as_millis())).collect::<String>(),
            )
        }
    };
    let tmp = path.with_extension("tmp");
    if fs::create_dir_all(path.parent().expect("target/ has a parent")).is_ok()
        && fs::write(&tmp, body).is_ok()
    {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Longest job first, on what the last run measured.
///
/// A phase's wall clock is `max(sum / width, longest job)`, and FIFO reaches the
/// first term only if no long job is dispatched late. Declaration order puts the
/// feature-carrying tests last — deliberately, to keep the kernel rebuilds
/// together — which is exactly the worst order for a wide phase: `xhci_hid_break`
/// and `xhci_deaf_registers` are two of the three longest jobs in the suite and
/// both sit in the last quarter of `MACHINE_TESTS`.
///
/// **The profile is measured, not declared**, because the alternative is a
/// hand-maintained list of long tests — a second registration to keep true, and
/// one nothing would notice going stale. A name the file has never seen sorts
/// first, so a new test is assumed long until it has been timed once: the cost of
/// being wrong that way is one lane starting a short job early.
fn longest_first(tasks: &mut [Task<'_>], known: &BTreeMap<String, Duration>) {
    let cost = |task: &Task<'_>| -> Duration {
        task.names()
            .iter()
            .map(|n| known.get(*n).copied().unwrap_or(Duration::MAX))
            .fold(Duration::ZERO, |a, b| a.saturating_add(b))
    };
    tasks.sort_by_key(|task| std::cmp::Reverse(cost(task)));
}


/// One outcome, as the run prints it. Gate A goes through here too, so a
/// suspended audio boot cannot report itself differently from a suspended
/// machine test.
fn report_line(outcome: &Outcome) {
    let reason = || outcome.reason.as_deref().unwrap_or("check failed");
    match outcome.verdict() {
        Verdict::Pass(None) => eprintln!("  PASS  {}  ({:.0?})", outcome.name, outcome.elapsed),
        Verdict::Pass(Some(entry)) => eprintln!(
            "  PASS  {}  ({:.0?})  — #{} did not fire this run, which proves nothing",
            outcome.name, outcome.elapsed, entry.task
        ),
        Verdict::Fail(None) => {
            eprintln!("FAIL {}: {}", outcome.name, reason());
            if outcome.stalled() {
                eprintln!(
                    "  STALL {}  ({:.0?})  — the guard expired, so this says nothing about \
                     the tree",
                    outcome.name, outcome.elapsed
                );
            } else {
                eprintln!("  FAIL  {}  ({:.0?})", outcome.name, outcome.elapsed);
            }
        }
        Verdict::Fail(Some(entry)) => {
            eprintln!("FAIL {}: {}", outcome.name, reason());
            eprintln!(
                "  {} {}  ({:.0?})  — listed against #{}, and this is not that failure: \
                 the entry covers {:?}",
                if outcome.stalled() { "STALL" } else { "FAIL " },
                outcome.name,
                outcome.elapsed,
                entry.task,
                entry.says
            );
        }
        Verdict::Expected(entry) => {
            // The reason in full, exactly as a red would print it. An expected
            // failure is still a defect reproducing, and the run that reproduced
            // it is the only place its evidence exists.
            eprintln!("XFAIL {}: {}", outcome.name, reason());
            eprintln!(
                "  XFAIL {}  ({:.0?})  — expected, #{}, {}",
                outcome.name, outcome.elapsed, entry.task, entry.spec
            );
        }
        Verdict::Stale(entry) => eprintln!(
            "  STALE {}  ({:.0?})  — #{} says this test fails, and it passed",
            outcome.name, outcome.elapsed, entry.task
        ),
        Verdict::Invalid => eprintln!(
            "  INVL  {}  ({:.0?}) — the host was suspended for {:.0?} while it ran",
            outcome.name, outcome.elapsed, outcome.suspended
        ),
    }
}

/// Run `tasks` on `width` workers, printing each outcome as it lands.
///
/// One implementation for both phases: **the serial tail is this at width 1**,
/// so "serial" is a number rather than a second code path that could drift from
/// this one. It returns only once every worker has joined, which is what makes
/// "the parallel phase has drained" a fact about the call and not about where
/// it sits in `main`.
fn run_phase(
    tasks: Vec<Task<'_>>,
    width: usize,
    bins: &Bins<'_>,
    slots: &HostSlots,
) -> Vec<Outcome> {
    if tasks.is_empty() {
        return Vec::new();
    }
    let width = width.clamp(1, tasks.len());
    qemu::set_width(width as u32);
    let queue = std::sync::Mutex::new(std::collections::VecDeque::from(tasks));
    let mut all = Vec::new();
    thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel::<Outcome>();
        for lane in 0..width {
            let tx = tx.clone();
            let queue = &queue;
            scope.spawn(move || {
                common::lane::enter(lane);
                loop {
                    let next =
                        queue.lock().expect("a worker panicked holding the queue").pop_front();
                    let Some(task) = next else { return };
                    let _slot = slots.take(&task.names().join(" "));
                    run_task(task, bins, &tx);
                }
            });
        }
        drop(tx);
        for outcome in rx {
            report_line(&outcome);
            all.push(outcome);
        }
    });
    all
}

/// The selected machine tests as boots: a run of adjacent names of one group is
/// one task.
fn machine_tasks(selected: &[(&'static str, Sched)]) -> Vec<(Sched, Vec<&'static str>)> {
    let mut out: Vec<(Sched, Vec<&'static str>)> = Vec::new();
    for &(name, sched) in selected {
        let joins = group_of(name).is_some()
            && out.last().is_some_and(|(_, names)| {
                group_of(names[names.len() - 1]) == group_of(name)
            });
        match out.last_mut() {
            Some((_, names)) if joins => names.push(name),
            _ => out.push((sched, vec![name])),
        }
    }
    out
}

/// Every test with a boot, split into the parallel and serial phases.
///
/// Pulled out of `main` so [`check_shard_partition`] builds the identical
/// lists a real run would rather than a second, hand-written approximation
/// that could pass its own check while the real path still disagreed with
/// itself — which is exactly the shape of the defect run `31617589126` found.
fn build_tasks<'a>(
    tests_to_run: &[&'a TestDef],
    machine_to_run: &[(&'static str, Sched)],
    screen_to_run: &[(&'static str, Sched)],
) -> (Vec<Task<'a>>, Vec<Task<'a>>) {
    let mut parallel: Vec<Task> = Vec::new();
    let mut serial: Vec<Task> = Vec::new();
    if !tests_to_run.is_empty() {
        let (actuator, shipping): (Vec<&TestDef>, Vec<&TestDef>) =
            tests_to_run.iter().copied().partition(|t| ACTUATOR_TESTS.contains(&t.name.as_str()));
        for tests in [shipping, actuator] {
            if tests.is_empty() {
                continue;
            }
            let features = shared_kernel(&tests[0].name);
            let task = Task::Shared(tests, features);
            match SHARED_BLOCK {
                Sched::Parallel => parallel.push(task),
                Sched::Serial => serial.push(task),
            }
        }
    }
    for (sched, names) in machine_tasks(machine_to_run) {
        let task = Task::Machine(names);
        match sched {
            Sched::Parallel => parallel.push(task),
            Sched::Serial => serial.push(task),
        }
    }
    for &(name, sched) in screen_to_run {
        let task = Task::Screen(name);
        match sched {
            Sched::Parallel => parallel.push(task),
            Sched::Serial => serial.push(task),
        }
    }
    (parallel, serial)
}

/// **The property every merged CI run depends on, checked before any of the
/// twelve processes that would otherwise each discover it separately.** Every
/// name [`Shard::keep`] is handed for `count` must land in exactly one of
/// `1..=count`'s shards — run `31617589126` is what a violation costs: shard 1
/// and shard 9 each priced the same task low enough to take it, and
/// `--merge-durations` refused the run for measuring `abuse_kernel_addr`
/// twice.
///
/// This cannot reproduce *why* two real processes disagreed — that needs
/// [`shard_pricing`]'s fix, not a test, because the defect was two machines
/// pricing a task from two different `target/test-durations` a shared build
/// cache handed them. What this can and does check is the part a shared-fate
/// bug would otherwise hide behind: that pricing every task from the
/// committed profile alone — the one input every process is guaranteed to
/// agree on — still yields a clean partition, for real registration data, at
/// the width CI actually runs.
fn check_shard_partition(all_tests: &[TestDef]) {
    let pricing = shard_pricing();
    for &nightly in &[false, true] {
        let in_tier = |tier: Tier| nightly || tier == Tier::Fast;
        let tests_to_run: Vec<&TestDef> =
            all_tests.iter().filter(|_| in_tier(SHARED_TIER)).collect();
        let machine_to_run: Vec<(&str, Sched)> = MACHINE_TESTS
            .iter()
            .filter(|(_, _, tier)| in_tier(*tier))
            .map(|(n, s, _)| (*n, *s))
            .collect();
        let screen_to_run: Vec<(&str, Sched)> = SCREEN_TESTS
            .iter()
            .filter(|(_, _, tier)| in_tier(*tier))
            .map(|(n, s, _)| (*n, *s))
            .collect();
        let audio_names: Vec<&str> = AUDIO_TESTS
            .iter()
            .filter(|(_, tier)| in_tier(*tier))
            .map(|(name, _)| *name)
            .collect();

        let (parallel, serial) = build_tasks(&tests_to_run, &machine_to_run, &screen_to_run);
        // Owned, not borrowed: each shard below clones `parallel`/`serial` into
        // a scratch `Vec` that does not outlive its own loop iteration, so what
        // accumulates across iterations cannot hold a reference into it.
        let want: BTreeSet<String> =
            parallel.iter().chain(&serial).flat_map(Task::names).map(str::to_string).collect();

        const COUNT: usize = 12;
        let cost = |task: &Task<'_>| -> Option<Duration> {
            task.names().iter().try_fold(Duration::ZERO, |a, n| Some(a + *pricing.get(*n)?))
        };
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut audio_seen: BTreeSet<String> = BTreeSet::new();
        for index in 1..=COUNT {
            let shard = Shard { index, count: COUNT };
            let mut mine_p = parallel.clone();
            let mut mine_s = serial.clone();
            let mut mine_a = audio_names.clone();
            // One accumulator across the three pools, in the order `main` takes
            // them: the partition a shard gets is a function of all three calls,
            // so a check that took them apart would be checking something else.
            let mut load = shard.bins();
            shard.keep(&mut mine_p, &mut load, cost);
            shard.keep(&mut mine_s, &mut load, cost);
            shard.keep(&mut mine_a, &mut load, |name| {
                AUDIO_SMP.iter().try_fold(Duration::ZERO, |a, smp| {
                    Some(a + *pricing.get(&format!("{name} (smp={smp})"))?)
                })
            });
            for name in mine_p.iter().chain(&mine_s).flat_map(Task::names) {
                assert!(
                    seen.insert(name.to_string()),
                    "nightly={nightly}: {name} lands in shard {index}/{COUNT} and at least \
                     one earlier shard too — every execution label must belong to exactly one"
                );
            }
            for name in mine_a {
                assert!(
                    audio_seen.insert(name.to_string()),
                    "nightly={nightly}: audio config {name} lands in shard {index}/{COUNT} \
                     and at least one earlier shard too"
                );
            }
        }
        assert_eq!(
            seen, want,
            "nightly={nightly}: the twelve shards together do not equal the full selection — \
             {:?} present in the selection and missing from every shard",
            want.difference(&seen).collect::<Vec<_>>()
        );
        let want_audio: BTreeSet<String> = audio_names.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            audio_seen, want_audio,
            "nightly={nightly}: the twelve shards' audio configs do not equal the full \
             selection"
        );
    }
}

/// The conservative half of the CI cutoff: a Fast registration must have a
/// committed price at/below the line or the explicit one-run UNMEASURED marker.
/// Missing evidence is refused rather than quietly joining Fast. The one-off
/// measurement-branch bootstrap for a new name is recorded in the cost audit;
/// its provisional price is never the evidence a final change lands with.
///
/// **The line here is `FAST_COMMIT_MS`, not `FAST_CEILING_MS`** — the price a
/// test may be *committed* at, which is where the fast tier's margin rule bites
/// on a registration. `tiers::ci_profile_verdicts` says the same of a merged
/// measurement; two gates on one policy may not disagree about where it is.
fn assert_fast_profile_label(
    label: &str,
    tier: Tier,
    profile: &BTreeMap<String, Duration>,
) {
    if tier == Tier::Nightly {
        return;
    }
    let measured = profile.get(label).unwrap_or_else(|| {
        panic!(
            "{label} is registered Fast but the committed CI profile has no measurement; \
             obtain its one-off KVM measurement and commit the resulting profile before \
             assigning its final tier"
        )
    });
    if measured.as_millis() == tiers::UNMEASURED_MS as u128 {
        return;
    }
    assert!(
        measured.as_millis() <= tiers::FAST_COMMIT_MS as u128,
        "{label} is registered Fast but the committed CI profile measures it at {} ms, \
         over the {} ms price a Fast test may be committed at (the {} ms line less its \
         margin) — relegate it or make it faster",
        measured.as_millis(),
        tiers::FAST_COMMIT_MS,
        tiers::FAST_CEILING_MS,
    );
}

/// Every claim the three explicit registration lists make about themselves,
/// before anything boots.
///
/// A group whose members drifted apart still passes — each one boots its own
/// machine and reads its own console — so nothing downstream would notice, and
/// a group split across the two phases could not share a guest at all.
fn check_registration() {
    let mut profile = BTreeMap::new();
    read_durations(&committed_durations_path(), &mut profile);
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for (name, _, tier) in MACHINE_TESTS.iter().chain(SCREEN_TESTS) {
        assert!(seen.insert(name, ()).is_none(), "{name} is registered twice");
        assert_fast_profile_label(name, *tier, &profile);
    }
    for (name, tier) in AUDIO_TESTS {
        assert!(seen.insert(name, ()).is_none(), "{name} is registered twice");
        for smp in AUDIO_SMP {
            assert_fast_profile_label(&format!("{name} (smp={smp})"), *tier, &profile);
        }
    }

    let mut groups: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for (i, (name, _, _)) in MACHINE_TESTS.iter().enumerate() {
        let Some(group) = group_of(name) else { continue };
        let span = groups.entry(group).or_insert((i, i, 0));
        span.1 = i;
        span.2 += 1;
    }
    for (group, (first, last, count)) in groups {
        assert_eq!(
            last - first + 1,
            count,
            "{group}'s members are not adjacent in MACHINE_TESTS, so they cannot share a boot"
        );
        assert!(
            MACHINE_TESTS[first..=last].windows(2).all(|w| w[0].1 == w[1].1),
            "{group} shares one boot, so its members must share one scheduling answer"
        );
        // **One boot cannot be in two tiers.** A group whose members disagreed
        // would put the boot in the fast tier for whichever member ran first and
        // charge the fast tier the whole group's cost — which is the arithmetic
        // the ceiling exists to control. It is also what makes
        // `tiers::Why::RidesTheBootOf` an honest row rather than an excuse: the
        // cheap members of the metal-sim boot are relegated *because* this is
        // enforced.
        assert!(
            MACHINE_TESTS[first..=last].windows(2).all(|w| w[0].2 == w[1].2),
            "{group} shares one boot, so its members must share one tier"
        );
    }
    for row in tiers::RELEGATED {
        let tiers::Why::RidesTheBootOf(carrier) = row.why else { continue };
        let rider_group = group_of(row.test);
        let carrier_group = group_of(carrier);
        assert!(
            rider_group.is_some() && rider_group == carrier_group,
            "{} says it rides {carrier}, but group_of gives {:?} and {:?}",
            row.test,
            rider_group,
            carrier_group,
        );
    }

    // The declaration and the registration, against each other and in both
    // directions. `src/tiers.rs` carries what each relegated name cost and what
    // it guarded — the record the owner reads and the input a future scheduled
    // workflow takes — and this is the only place the two can be compared: the
    // registration is here, and `cargo test --lib` cannot see it.
    let registered: BTreeSet<&str> = MACHINE_TESTS
        .iter()
        .chain(SCREEN_TESTS)
        .filter(|(_, _, tier)| *tier == Tier::Nightly)
        .map(|(name, _, _)| *name)
        .chain(AUDIO_TESTS.iter().filter(|(_, tier)| *tier == Tier::Nightly).map(|(name, _)| *name))
        .collect();
    let declared = tiers::relegated_names();
    let undeclared: Vec<&&str> = registered.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "{undeclared:?} are registered Tier::Nightly and src/tiers.rs says nothing about \
         them — a test that stops being gated per pull request without a row saying what \
         it guarded is exactly the silence this mechanism exists to refuse"
    );
    let unregistered: Vec<&&str> = declared.difference(&registered).collect();
    assert!(
        unregistered.is_empty(),
        "src/tiers.rs relegates {unregistered:?} and no registration marks them \
         Tier::Nightly — either the name is stale or the test is still running in the \
         fast tier while the record says it is not"
    );
}

/// The half [`check_registration`] could not ask: the shared boot's tests are
/// *discovered* from the binaries in `tests/toyos-rust-tests` and `tests/c`, so
/// nothing declared can be compared against them until they exist.
///
/// A name in both places is two tests reporting one name, and the damage is not
/// a duplicate line. [`retry_task`] searches the shared registry first, so a
/// machine test of that name which failed wide is re-run *as the other test* and
/// its `ALONE:` verdict is about neither. Four names were doing this and the
/// suite had never been able to see them.
fn check_no_collisions(shared: &[TestDef]) {
    let mut shared_seen = BTreeSet::new();
    let shared_twice: Vec<&str> = shared
        .iter()
        .map(|test| test.name.as_str())
        .filter(|name| !shared_seen.insert(*name))
        .collect();
    assert!(
        shared_twice.is_empty(),
        "{shared_twice:?} name two binaries on the shared boot; every verdict and duration \
         label must identify exactly one execution"
    );
    let declared: BTreeSet<&str> = MACHINE_TESTS
        .iter()
        .map(|(n, _, _)| *n)
        .chain(SCREEN_TESTS.iter().map(|(n, _, _)| *n))
        .chain(AUDIO_TESTS.iter().map(|(name, _)| *name))
        .collect();
    let clash: Vec<&str> =
        shared.iter().map(|t| t.name.as_str()).filter(|n| declared.contains(n)).collect();
    assert!(
        clash.is_empty(),
        "{clash:?} name both a binary on the shared boot and a test that declares its own \
         machine — two verdicts under one name, and `retry_task` takes the shared one. Add \
         each to RUST_SKIP with the reason its own test exists, or rename one of the two."
    );
    if SHARED_TIER == Tier::Fast {
        let mut profile = BTreeMap::new();
        read_durations(&committed_durations_path(), &mut profile);
        for test in shared {
            assert_fast_profile_label(&test.name, SHARED_TIER, &profile);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // First, before any lock and before anything is compiled: a flag this suite
    // does not have would otherwise cost nothing and hand its value to the
    // filter below.
    let filter = match toyos_build::testargs::parse(&args) {
        Ok(filter) => filter,
        Err(refusal) => {
            eprintln!("[toyos] {refusal}");
            std::process::exit(1);
        }
    };

    let debug_mode = args.iter().any(|a| a == "--debug");
    let list_mode = args.iter().any(|a| a == "--list");
    // The nightly tier, on. A flag and not an env var for `--audio-gate`'s
    // reason: an env var is invisible in the command line and easy to leave set,
    // and the whole point of the split is that a run says what it ran.
    let nightly = args.iter().any(|a| a == "--nightly");
    if args.iter().any(|a| a == "--slow-usb") {
        SLOW_USB.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let nocapture = args.iter().any(|a| a == "--nocapture" || a == "--show-output");

    // Thorough tier. A flag rather than an env var or a test name: an env var
    // is invisible in the command line and easy to leave set, and a test name
    // would drag ~17 minutes into every plain `cargo test`.
    let mut audio_gate: Option<u32> = None;
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--audio-gate=") {
            v
        } else if a == "--audio-gate" {
            args.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| {
                panic!("--audio-gate needs an iteration count, e.g. --audio-gate 30")
            })
        } else {
            continue;
        };
        let n: u32 = n
            .parse()
            .unwrap_or_else(|_| panic!("--audio-gate: {n:?} is not an iteration count"));
        assert!(n >= 2, "--audio-gate needs at least 2 iterations to compare anything");
        audio_gate = Some(n);
    }

    // How many guests the parallel phase runs at once. The serial tail and gate
    // A ignore it — that is what they are.
    let mut width = DEFAULT_WIDTH;
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--jobs=") {
            v
        } else if a == "--jobs" || a == "-j" {
            args.get(i + 1)
                .map(|s| s.as_str())
                .unwrap_or_else(|| panic!("--jobs needs a width, e.g. --jobs 4"))
        } else {
            continue;
        };
        width = n.parse().unwrap_or_else(|_| panic!("--jobs: {n:?} is not a width"));
        assert!(width >= 1, "--jobs needs at least one worker");
    }

    // Which slice of the suite this machine runs. Absent is the whole of it.
    let shard = match toyos_build::testargs::parse_shard(&args) {
        Ok(shard) => shard,
        Err(refusal) => {
            eprintln!("[toyos] {refusal}");
            std::process::exit(1);
        }
    };

    // How many guests may be up on the *host* at once, across every worktree.
    // `--jobs` is this run's demand; this is what the machine will supply, and
    // zero turns it off.
    let mut host_budget = toyos_build::buildlock::HOST_GUESTS;
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--host-slots=") {
            v
        } else if a == "--host-slots" {
            args.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| {
                panic!("--host-slots needs a budget, e.g. --host-slots 12 (0 turns it off)")
            })
        } else {
            continue;
        };
        host_budget =
            n.parse().unwrap_or_else(|_| panic!("--host-slots: {n:?} is not a budget"));
    }

    // And how many of this host's *compiles* may run at once, across every
    // worktree. A worker holds a guest slot from the moment it takes a task and
    // spends the first part of it building a kernel variant, so twelve workers
    // are twelve concurrent `cargo build`s and no guest at all.
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--host-builds=") {
            v
        } else if a == "--host-builds" {
            args.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| {
                panic!("--host-builds needs a budget, e.g. --host-builds 4 (0 turns it off)")
            })
        } else {
            continue;
        };
        toyos_build::buildlock::set_host_builds(
            n.parse().unwrap_or_else(|_| panic!("--host-builds: {n:?} is not a budget")),
        );
    }
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();

    // For the whole run, and outermost: a `--claim-sysroot` in another worktree
    // rebuilds the sysroot this run's every later build reads, and the run's
    // answer to that used to be a hundred identical refusals and a dead gate.
    // Taken once, before any build lock, so the order is always sysroot →
    // global — a second acquisition here would be a cycle with the claim's
    // writer preference.
    let _sysroot = toyos_build::buildlock::run_against_sysroot(&repo_root, "cargo test");

    let slots = HostSlots {
        label: repo_root
            .file_name()
            .map_or_else(|| "this worktree".to_string(), |n| n.to_string_lossy().into_owned()),
        root: repo_root,
        budget: host_budget,
    };

    check_registration();

    if nocapture || debug_mode {
        common::qemu::VERBOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let c_names = discover_c_tests();
    eprintln!(
        "[toyos] Compiling {} C tests, and attempting {} declared ones...",
        c_names.len(),
        NOT_RUN.len()
    );
    check_not_run();
    let c_bins = compile_c_tests(&c_names);
    let c_compiled: Vec<String> = c_bins.iter().map(|(n, _)| n.clone()).collect();

    let rust_tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/toyos-rust-tests");
    eprintln!("[toyos] Building Rust tests...");
    let rust_bins = qemu::build_toyos_bins(&rust_tests_dir);

    // --list: print test names and exit
    if list_mode {
        let tests = build_test_registry(&rust_bins, &c_compiled);
        for t in &tests {
            println!("{}", t.name);
        }
        for (name, _) in AUDIO_TESTS {
            println!("{name}");
        }
        for (name, _, _) in SCREEN_TESTS {
            println!("{name}");
        }
        for (name, _, _) in MACHINE_TESTS {
            println!("{name}");
        }
        return;
    }

    if debug_mode {
        run_debug_mode(&c_bins, &rust_bins);
        return;
    }

    if let Some(iterations) = audio_gate {
        let mut audio_to_run: Vec<&str> = AUDIO_TESTS
            .iter()
            .map(|(name, _)| *name)
            .filter(|n| filter.is_none_or(|f| n.contains(f)))
            .collect();
        assert!(!audio_to_run.is_empty(), "no audio test matches filter {filter:?}");
        // Sharded too, and this is the tier it buys the most for: the thorough
        // tier is N boots per config taken one at a time by construction, so
        // splitting it is the only thing that shortens it. A filter cannot do
        // the same job — `audio_tone` is a substring of `audio_tone_load`.
        if let Some(shard) = shard {
            // Gate A runs this tier and nothing else, so its configs are the
            // whole of what this process's bins ever hold.
            shard.keep(&mut audio_to_run, &mut shard.bins(), |_| None);
            assert!(
                !audio_to_run.is_empty(),
                "shard {}/{} owns no audio config, and a gate that ran nothing would \
                 report itself green",
                shard.index,
                shard.count,
            );
        }
        let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
        // One slot for the whole tier: it boots one guest at a time for the
        // length of it, so one slot is what it occupies. The owner has ruled
        // that gate A does not get a quiet host (CLAUDE.md, 2026-08-04), so it
        // takes its share of the machine like everything else and does not
        // reserve it.
        let _slot = slots.take("gate A, thorough");
        let ok = run_audio_gate(
            iterations,
            &load_audio_baseline(),
            &audio_to_run,
            &test_config,
            &c_bins,
            &rust_bins,
        );
        if !ok {
            std::process::exit(1);
        }
        return;
    }

    let all_tests = build_test_registry(&rust_bins, &c_compiled);
    check_no_collisions(&all_tests);
    check_shard_partition(&all_tests);
    // Every name this process could produce a verdict for, which is what an
    // EXPECTED_FAILURES entry has to be one of. Taken before the filter, so a
    // filtered run cannot make a stale entry look well-formed.
    let runnable: BTreeSet<&str> = all_tests
        .iter()
        .map(|t| t.name.as_str())
        .chain(AUDIO_TESTS.iter().map(|(name, _)| *name))
        .chain(SCREEN_TESTS.iter().map(|(n, _, _)| *n))
        .chain(MACHINE_TESTS.iter().map(|(n, _, _)| *n))
        .collect();
    if let Err(refusal) = check_expected_failures(EXPECTED_FAILURES, &runnable) {
        eprintln!("[toyos] EXPECTED_FAILURES: {refusal}");
        std::process::exit(1);
    }

    let keep = |name: &str| filter.is_none_or(|f| name.contains(f));
    // The tier filter, and it is not conditional on the name filter: a rule with
    // an exception for filtered runs is two rules, and the second one is the one
    // nobody remembers. `cargo test -- desktop_window_child` refuses below and
    // says what to type instead, which is the same information a silent skip
    // would have withheld.
    let in_tier = |tier: Tier| nightly || tier == Tier::Fast;
    let tests_to_run: Vec<&TestDef> = all_tests
        .iter()
        .filter(|t| keep(t.name.as_str()) && in_tier(SHARED_TIER))
        .collect();
    let mut audio_to_run: Vec<&str> = AUDIO_TESTS
        .iter()
        .filter(|(name, tier)| keep(name) && in_tier(*tier))
        .map(|(name, _)| *name)
        .collect();
    let screen_to_run: Vec<(&str, Sched)> = SCREEN_TESTS
        .iter()
        .filter(|(n, _, tier)| keep(n) && in_tier(*tier))
        .map(|(n, s, _)| (*n, *s))
        .collect();
    let machine_to_run: Vec<(&str, Sched)> = MACHINE_TESTS
        .iter()
        .filter(|(n, _, tier)| keep(n) && in_tier(*tier))
        .map(|(n, s, _)| (*n, *s))
        .collect();

    // **What this run is not doing, said before it does anything.** A run that
    // quietly does less than the last one is the failure mode the tier
    // introduces, so the names are printed rather than counted, and the line
    // carries both the command that runs them and the record that says what each
    // one guarded.
    let held_back: Vec<&str> = MACHINE_TESTS
        .iter()
        .chain(SCREEN_TESTS)
        .filter(|(n, _, tier)| keep(n) && !in_tier(*tier))
        .map(|(n, _, _)| *n)
        .chain(
            AUDIO_TESTS
                .iter()
                .filter(|(name, tier)| keep(name) && !in_tier(*tier))
                .map(|(name, _)| *name),
        )
        .collect();
    if !held_back.is_empty() {
        let ms: u64 = tiers::RELEGATED
            .iter()
            .filter(|r| held_back.contains(&r.test))
            .map(|r| r.ci_ms)
            .sum();
        eprintln!(
            "[toyos] nightly tier: {} test(s) NOT run, {:.1} s of effective CI test time. \
             `cargo test --test toyos-build -- --nightly` runs them manually; \
             .github/workflows/ci.yml runs them every night at 03:00 UTC. \
             `src/tiers.rs`'s `RELEGATED` says what each one guards.",
            held_back.len(),
            ms as f64 / 1000.0,
        );
        eprintln!("[toyos]   {}", held_back.join(", "));
    }

    if tests_to_run.is_empty()
        && audio_to_run.is_empty()
        && screen_to_run.is_empty()
        && machine_to_run.is_empty()
    {
        if !held_back.is_empty() {
            eprintln!(
                "[toyos] filter {filter:?} matches only tests in the nightly tier. Add \
                 --nightly to run them."
            );
        } else {
            eprintln!("No tests match filter {filter:?}");
        }
        std::process::exit(1);
    }
    for entry in EXPECTED_FAILURES {
        // Before anything boots, so that the run reads as what it is from its
        // first line: a suite carrying declared reds is not a clean suite.
        eprintln!(
            "[toyos] expected to fail: {} — #{}, {}",
            entry.test, entry.task, entry.spec
        );
    }

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let mut tally = Tally::new(EXPECTED_FAILURES, Day::today()).holding_back(&held_back);
    let suite_start = common::clock::mark();

    let bins = Bins {
        test_config: &test_config,
        c_bins: &c_bins,
        rust_bins: &rust_bins,
    };

    // Everything that owns a boot, split by the one question its registration
    // asks. Dispatch is declaration order and the queue is FIFO, so
    // MACHINE_TESTS keeping the plain-kernel names first and SCREEN_TESTS
    // putting its feature-carrying ones last still holds inside each phase.
    //
    // No longest-first heuristic, deliberately: the phase's wall clock is set by
    // its longest job and the durations that would order it are not in the
    // tree.
    if !tests_to_run.is_empty() {
        let actuator_count =
            tests_to_run.iter().filter(|t| ACTUATOR_TESTS.contains(&t.name.as_str())).count();
        eprintln!(
            "[toyos] The shared boot carries {} C + {} Rust binaries: {} on the shipping \
             kernel, {} on the actuator one",
            c_bins.len(),
            rust_bins.len(),
            tests_to_run.len() - actuator_count,
            actuator_count,
        );
    }
    let (mut parallel, mut serial) = build_tasks(&tests_to_run, &machine_to_run, &screen_to_run);

    // Every red the wide phase produced, re-run by itself before anything is
    // believed about it. See [`retry_task`] for why both answers are findings
    // and why neither turns the run green.
    let known = load_durations();
    // After the phases are decided and before either is ordered: what a shard
    // divides is the work, and a task's answer to `Sched` is a property of the
    // test rather than of how many machines are running it.
    if let Some(shard) = shard {
        // [`shard_pricing`], and not `known`: every process partitioning the
        // same run must price a task identically, which only the committed
        // profile guarantees.
        let pricing = shard_pricing();
        // A task whose every name has been timed costs their sum; one carrying
        // a name the profile has never seen is unmeasured, which is the same
        // all-or-nothing rule [`longest_first`] states with `Duration::MAX`.
        let cost = |task: &Task<'_>| -> Option<Duration> {
            task.names()
                .iter()
                .try_fold(Duration::ZERO, |a, n| Some(a + *pricing.get(*n)?))
        };
        longest_first(&mut parallel, &pricing);
        longest_first(&mut serial, &pricing);
        // One accumulator for the whole run, heaviest pool first: this process
        // runs all three pools one after another, so its wall clock is the one
        // bin they share and the serial tail belongs in whichever bin the
        // parallel phase left lightest. Three partitions from three empty
        // accumulators are each good and their sum is not
        // (`Shard::keep`, and run `31377439504`'s 466.1 s against a 369.1 s
        // even split).
        let mut load = shard.bins();
        shard.keep(&mut parallel, &mut load, cost);
        shard.keep(&mut serial, &mut load, cost);
        shard.keep(&mut audio_to_run, &mut load, |name| {
            AUDIO_SMP.iter().try_fold(Duration::ZERO, |a, smp| {
                Some(a + *pricing.get(&format!("{name} (smp={smp})"))?)
            })
        });
        eprintln!(
            "[toyos] shard {}/{}: {} parallel task(s), {} serial, {} audio config(s)",
            shard.index,
            shard.count,
            parallel.len(),
            serial.len(),
            audio_to_run.len() * AUDIO_SMP.len(),
        );
    }

    // Counted from the task lists rather than from the filtered ones, because a
    // shard's own total is what its summary has to add up against.
    let total = parallel.iter().chain(serial.iter()).map(|t| t.names().len()).sum::<usize>()
        + audio_to_run.len() * AUDIO_SMP.len();
    if let Err(refusal) = toyos_build::testargs::validate_ordinary_shard(shard, filter, total) {
        eprintln!("[toyos] {refusal}");
        std::process::exit(1);
    }
    eprintln!("\nrunning {total} tests\n");

    let mut timed: Vec<(String, Duration)> = Vec::new();
    // Every red, with whether it had the host to itself when it happened and
    // *what it said* — the third field, because the re-run below has to be able
    // to answer whether the two runs failed the same way, and by the time it
    // runs this outcome has been moved into the tally. Reds only: a test the
    // host slept through has no verdict to confirm, and re-running it would put
    // a second guess beside the first — and an expected failure has already been
    // answered by its entry, which names the task rather than asking which of
    // the retry's two answers it was.
    let mut reds: Vec<(String, bool, String)> = Vec::new();
    let mut collect = |outcomes: &[Outcome], shared_the_host: bool| {
        reds.extend(
            outcomes
                .iter()
                .filter(|o| matches!(o.verdict(), Verdict::Fail(_)))
                .map(|o| (o.name.clone(), shared_the_host, headline(o.reason.as_deref()))),
        );
    };
    if !parallel.is_empty() {
        longest_first(&mut parallel, &known);
        eprintln!("  --- parallel, {width} wide ---");
        let started = std::time::Instant::now();
        let outcomes = run_phase(parallel, width, &bins, &slots);
        eprintln!("  --- parallel done in {:.1?} ---", started.elapsed());
        collect(&outcomes, width > 1);
        timed.extend(outcomes.iter().map(|o| (o.name.clone(), o.elapsed)));
        outcomes.into_iter().for_each(|o| tally.record(o));
    }
    if !serial.is_empty() {
        eprintln!("  --- serial ---");
        let started = std::time::Instant::now();
        let outcomes = run_phase(serial, 1, &bins, &slots);
        eprintln!("  --- serial done in {:.1?} ---", started.elapsed());
        // **The serial tail is one guest whatever `--jobs` says**, which is
        // exactly why its reds were never re-run: the loop below was written for
        // the parallel phase and read the *run's* width. So the two
        // `Sched::Serial` reds of run `31252989653` — `screen_pager_keys` and
        // `usb_transport_break` — carried no `ALONE:` line at all and nobody
        // could say whether either was reproducible.
        collect(&outcomes, false);
        timed.extend(outcomes.iter().map(|o| (o.name.clone(), o.elapsed)));
        outcomes.into_iter().for_each(|o| tally.record(o));
    }
    qemu::set_width(1);

    if !reds.is_empty() {
        eprintln!("  --- re-running {} failure(s) alone ---", reds.len());
        for (name, shared_the_host, wide) in &reds {
            let Some(task) = retry_task(name, &tests_to_run) else {
                eprintln!("  ALONE {name}: no way to run it by itself; verdict stands");
                continue;
            };
            let outcomes = run_phase(vec![task], 1, &bins, &slots);
            let alone = outcomes.iter().find(|o| &o.name == name);
            eprintln!("{}", alone_line(name, wide, *shared_the_host, alone));
        }
    }

    // Gate A, alone. `tests/audio-baseline.toml`'s numbers were recorded with
    // one QEMU on the host and no concurrent agents, so a run beside anything
    // else is not the instrument they describe — which makes this a
    // precondition rather than an ordering convention, and worth asserting.
    // `run_phase` joins its workers before it returns, and this is what says so.
    if !audio_to_run.is_empty() {
        assert_eq!(
            qemu::live_instances(),
            0,
            "gate A ran with another guest still up; its baseline is a quiet host"
        );
        let audio_baseline = load_audio_baseline();
        eprintln!("  --- audio ---");
        for name in &audio_to_run {
            for &smp in AUDIO_SMP {
                let label = format!("{name} (smp={smp})");
                let baseline = config_baseline(&audio_baseline, name, smp);
                let _slot = slots.take(&label);
                let start = common::clock::mark();
                // A boot that never reaches its marker panics, and gate A is the
                // last thing the suite runs: unwrapped, that panic took the
                // whole run's verdict with it and printed no result line at all.
                let outcome = catching(|| {
                    run_audio_test(name, smp, &baseline, &test_config, &c_bins, &rust_bins)
                });
                // Gate A's every number comes off a clock — wake lateness, a
                // period's worth of samples, the position of a gap in the
                // capture. A host that stopped in the middle of one moved all of
                // them, so this outcome is not a reading of anything.
                let outcome = Outcome {
                    name: label,
                    reason: outcome.err(),
                    elapsed: start.elapsed(),
                    suspended: start.suspended(),
                };
                report_line(&outcome);
                timed.push((outcome.name.clone(), outcome.elapsed));
                tally.record(outcome);
            }
        }
    }

    // After gate A, because its configs are the only tasks a shard prices by a
    // name with an `smp=` in it and they are the last thing measured.
    save_durations(known, &timed, shard);

    // Three exit statuses, because there are three things a run can establish,
    // and an expected failure is deliberately none of them — see
    // [`Tally::exit_code`], which is where the whole decision now lives.
    //
    // A green run is a claim that this tree passed, and `--land`'s gate consumes
    // exactly this number. A run that spanned a suspend did not establish that:
    // its timing verdicts were taken across a stopped host and its liveness
    // ceilings were measured on a clock that stopped with it, so exit 0 would be
    // a claim it cannot support.
    //
    // Nor may it be 1. A red sends an agent hunting a defect, and the defect is
    // not there — the lid was closed. CLAUDE.md already documents the signature
    // and documents it as something a *human* must notice before recording a
    // finding, which is exactly the judgement a status code should carry
    // instead. So: 2, with a headline that names it and says re-run.
    //
    // A run with both real failures and invalidated tests exits 1: a red that
    // survives is still a red, and re-running the suspended ones does not make
    // it green.
    // What this run cost cargo: a kernel build is ~6.9 s of wall clock and
    // ~29.6 s of CPU after any edit under `kernel/`, and a full run used to
    // make 45 of them.
    let (boots, feature_boots, kernels) = qemu::boot_census();
    eprintln!(
        "  --- {boots} guests, {feature_boots} of them not the shipping kernel, {} kernel \
         build(s): {kernels:?}",
        kernels.len(),
    );

    eprint!("{}", tally.summary(total, suite_start.elapsed(), suite_start.suspended()));
    std::process::exit(tally.exit_code());
}
