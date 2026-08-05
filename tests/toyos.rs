mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::qemu::{self, BootOptions, QemuInstance, TestResult};
use common::{audio, compile, faults, screen, serial, stats, storage, usb};

struct TestDef {
    name: String,
    qemu_name: String,
    timeout: Duration,
    check: fn(&TestResult) -> bool,
}

/// Whether a test may run while other guests are up.
///
/// Every entry of [`MACHINE_TESTS`] and [`SCREEN_TESTS`] answers this or does
/// not compile. That is `specs/test-cost-audit.md` §3.3's serial-by-default rule
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
/// `specs/test-cost-audit.md` §5.4.7 carries the tables, including the one that
/// said eight, which was taken while `drain_serial` was still width-scaled and
/// `metal_sim_pointer_churn`'s twenty-four paced drains *were* the phase.
const DEFAULT_WIDTH: usize = 12;

/// This run's claim on the host's guest budget.
///
/// [`DEFAULT_WIDTH`] is a number for *one* suite, and nothing was handing out
/// the cores that two suites both spend (`specs/test-cost-audit.md` §4.1
/// constraint 3). A second suite on this machine is not a slower first suite, it
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

// Rust helper binaries that are spawned by tests, not tests themselves.
const RUST_SKIP: &[&str] = &[
    "segfault_child",
    "test_panic_child",
    "i8042_keyboard",
    "i8042_mouse",
    "input_events",
    "va_exhaustion",
    // Needs SYS_DEBUG actions 5 and 6, which only the `test-heap-ceiling`
    // kernel has. `heap_ceiling_recovery` boots that kernel.
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
    // Needs netd with a NIC. `netd_connection_caps` runs it on tests/netcase.
    "netd_caps",
    // Same reason, same config: `netd_hostile_peer` runs it there.
    "netd_hostile_peer",
    // Needs a boot image the harness staged a file into before the machine
    // started, which only `esp_filesystem` builds.
    "esp_files",
    // Two modes, each waiting to be typed at through QMP; on its own nothing
    // ever answers it. `swiss_german_layout`, `locale_detect` and
    // `locale_detect_unrecognized` drive it.
    "locale_gate",
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
];

// Audio glitch tests. Each runs in its own QEMU boot per SMP config and
// asserts on the wav the virtio-sound device captured, so they are excluded
// from the shared multi-test boot.
const AUDIO_TESTS: &[&str] = &["audio_tone", "audio_tone_load"];

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
/// Feature-carrying tests last: each distinct kernel feature set is one more
/// kernel rebuild, and ending on one leaves the plain-kernel tests above it
/// untouched by the thrash.
const SCREEN_TESTS: &[(&str, Sched)] = &[
    ("screen_decoder", Sched::Parallel),
    ("screen_diag_boot", Sched::Parallel),
    ("screen_log_absent", Sched::Parallel),
    ("screen_console_shell", Sched::Parallel),
    ("screen_console_clear", Sched::Parallel),
    ("screen_console_scroll", Sched::Parallel),
    ("screen_i8042_health", Sched::Parallel),
    ("screen_recoverable_untouched", Sched::Parallel),
    ("screen_early_panic", Sched::Parallel),
    ("screen_late_panic", Sched::Parallel),
    ("screen_paged_scrollback", Sched::Parallel),
    ("screen_panic_muted", Sched::Parallel),
    ("screen_console_panic", Sched::Parallel),
    ("screen_fatal_halt", Sched::Parallel),
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
const MACHINE_TESTS: &[(&str, Sched)] = &[
    ("ioapic_topology", Sched::Parallel),
    ("input_merge", Sched::Parallel),
    ("metal_sim_input", Sched::Parallel),
    // One boot from here to `metal_sim_compositor_stall` (`METAL_SIM_DESKTOP`).
    ("metal_sim_compositor", Sched::Parallel),
    // Reads the boot log this group already has, after the member above has
    // drained it. Text only, no clock in the verdict.
    ("metal_sim_scanout_wc", Sched::Parallel),
    ("metal_sim_window_caps", Sched::Parallel),
    ("metal_sim_ipc_hostile_peer", Sched::Parallel),
    ("metal_sim_compositor_stall", Sched::Parallel),
    // Last of the group: it drops clients on purpose and its verdict is that
    // the desktop outlived every one of them.
    ("metal_sim_client_death", Sched::Parallel),
    // A thousand pointer packets paced from the host, and not one assertion on
    // when any of them arrived: the settles are 400 ms against a driver that
    // acts in microseconds, both liveness loops run to 20 s, and the three
    // verdicts are a count of bound sources, a frame batch above the taskbar's
    // two, and a desktop still painting afterwards.
    ("metal_sim_pointer_churn", Sched::Parallel),
    // A window dragged by injected pointer packets, and the exact opposite of
    // the churn above on the one question that decides this: here each packet's
    // effect has to be on screen before the next is sent. The press that starts
    // the drag must land on a title bar the previous motion put under the
    // cursor, and the drag's displacement is read back as a coordinate — so a
    // guest one batch behind aims at the content instead, which is a different
    // verdict rather than a slower one. Watched to happen, on a compositor made
    // slow on purpose. Its own boot too: it leaves the pointer somewhere else
    // and the window in a different place than it found them.
    ("metal_sim_window_drag", Sched::Serial),
    // A host-measured drain rate with an 8 s ceiling on a 3.3 s expectation.
    // Not gate A, but the same instrument: what it measures is how fast a
    // client's audio leaves the machine.
    ("metal_sim_null_audio", Sched::Serial),
    // Parallel, and this one is argued rather than assumed: not a verdict in it
    // is a wall-clock margin. The flood's size is asserted against the audio
    // callback's own period counter standing still, both playback checks are
    // counted in periods, and the capture is read for amplitude and never for
    // timing. Its own boot, its own config, and the only client its soundd has.
    ("doom_sound_flood", Sched::Parallel),
    ("netd_connection_caps", Sched::Parallel),
    // Serial: it measures netd's 2 s handshake deadline against the host's
    // clock, and counts how many connections survived a 48 ms paced burst
    // before that deadline could expire any of them. Both are wall-clock
    // margins, which is the definition of [`Sched::Serial`].
    ("netd_hostile_peer", Sched::Serial),
    ("foreign_disk_untouched", Sched::Parallel),
    ("boot_partition_identity", Sched::Parallel),
    ("double_fault_stack", Sched::Parallel),
    // Its own boot, its own feature, and it drives the guest only through
    // stdin — nothing it touches is shared with another test.
    ("idle_stack_guard", Sched::Parallel),
    ("diskless_boot", Sched::Parallel),
    ("xhci_many_devices", Sched::Parallel),
    // Its whole assertion is that a keystroke injected from the host crossed a
    // USB keyboard on the *second* controller, and `input_events_run` sends
    // each one only after the guest has printed the last — so a key the host
    // never got to send is a stall it names, and never a key the driver lost.
    ("xhci_second_controller", Sched::Parallel),
    ("xhci_two_controllers", Sched::Parallel),
    ("xhci_msi_only", Sched::Parallel),
    ("xhci_no_interrupt", Sched::Parallel),
    ("nvme_large_device", Sched::Parallel),
    ("nvme_wide_sector", Sched::Parallel),
    ("iommu_discovery", Sched::Parallel),
    ("readdir_bound", Sched::Parallel),
    ("i8042_health", Sched::Parallel),
    // And one from here to `i8042_mouse` (`I8042_TRACE`), which is why all
    // three carry the answer the last of them needs.
    //
    // None of the three measures a rate. `i8042_mouse` sends each pointer
    // packet only once the guest has printed the one before it, so a guest with
    // less of the host is a longer run and not a smaller count; the keystrokes
    // above it put fewer bytes in flight than QEMU's controller holds.
    ("i8042_keyboard", Sched::Parallel),
    ("i8042_no_spurious_wake", Sched::Parallel),
    ("i8042_mouse", Sched::Parallel),
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
    ("swiss_german_layout", Sched::Parallel),
    ("locale_detect", Sched::Parallel),
    ("locale_detect_unrecognized", Sched::Parallel),
    // The wizard on the two surfaces the machine actually has, rather than on
    // the stand-in `locale_gate` is. Each costs a boot of a different image.
    ("console_locale_detect", Sched::Parallel),
    ("desktop_locale_detect", Sched::Parallel),
    // Typing at the same desktop, measured rather than transcribed: it waits
    // for its eight echoes instead of asserting how many arrived in a window,
    // so a guest that is slow costs seconds and not a verdict, and the verdict
    // itself is a fraction of the screen that no amount of load moves.
    ("desktop_typing_damage", Sched::Parallel),
    // Two boots of one machine compared on the guest's own `Boot: complete`
    // with a 300 ms allowance, which is the whole assertion.
    ("i8042_absent", Sched::Serial),
    ("i8042_quarantine", Sched::Parallel),
    ("i8042_budget_expiry", Sched::Parallel),
    ("i8042_fadt_denial", Sched::Parallel),
    ("i8042_kbd_echo", Sched::Parallel),
    ("i8042_undecoded_bytes", Sched::Parallel),
    // Its verdict is a cadence, and its absence is the assertion — both read
    // off the guest's own `last byte at Nms` stamps. The gap it injects is
    // 3 s against a 500 ms period, so six periods of margin decide whether the
    // report is on the pin or on a timer.
    ("i8042_health_cadence", Sched::Parallel),
    ("xhci_xecp_walk", Sched::Parallel),
    ("xhci_slot_exhaustion", Sched::Parallel),
    ("usb_storage_gate", Sched::Parallel),
    ("usb_storage_shapes", Sched::Parallel),
    ("usb_refused_disk_first", Sched::Parallel),
    ("usb_pool_exhausted", Sched::Parallel),
    ("usb_short_read", Sched::Parallel),
    // A plug over QMP and two host-side verdicts, neither of them a duration:
    // the disk that arrives comes back byte-identical, and the log on the boot
    // stick carries a line printed after it. The 1.2 s wait is against a 100 ms
    // debounce the driver finishes in microseconds under TCG.
    ("usb_disk_index_stable", Sched::Parallel),
    ("usb_storage_write_error", Sched::Parallel),
    ("usb_flush_optional", Sched::Parallel),
    ("xhci_deaf_registers", Sched::Parallel),
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
    ("xhci_slow_connect", Sched::Serial),
    ("xhci_portsc_rw1c", Sched::Parallel),
    // One staged break and no other, which puts the driver's recovery finishing
    // on its first try in the verdict: a retried command that reaches an
    // endpoint still halted from the staged break logs a second `transport
    // broke`, and how many tries it takes is how much of the host the guest had.
    ("usb_transport_break", Sched::Serial),
    ("xhci_full_speed_device", Sched::Parallel),
    // Two of the three below stage plug and unplug with fixed waits, and both
    // waits are 600-800 ms against a 100 ms debounce the driver finishes in
    // microseconds under TCG — a margin, not a race, and every verdict either
    // makes is a count of what the guest logged.
    ("xhci_hotplug", Sched::Parallel),
    // `xhci_flap` is the one that genuinely races the host against the guest:
    // its two QMP writes have to land inside *one* 100 ms debounce or the state
    // under test never happens, and it says so — `no replug collapsed inside a
    // debounce, so this run never staged the race`. A host that delays the
    // second write past 100 ms turns a green machine red with that sentence,
    // which is indistinguishable from the driver defect it hunts.
    ("xhci_flap", Sched::Serial),
    ("xhci_hid_break", Sched::Parallel),
    ("xhci_descriptor_walk", Sched::Parallel),
    ("esp_filesystem", Sched::Parallel),
    ("toybox_cp_volume", Sched::Parallel),
    ("kernel_log_file", Sched::Parallel),
    // Both own their images and their lanes, and neither verdict is a
    // wall-clock margin: the guest's clock starts from an instant the host set
    // and the only duration either measures is how long a boot takes to reach
    // its log sink, against a bound five minutes wide. A host so loaded that
    // this failed would have failed every timed test in the phase first.
    ("wall_clock_file", Sched::Parallel),
    ("wall_clock_refusals", Sched::Parallel),
    // `xhci_slow_connect`'s shape against the disk's port, and serial for the
    // same reason and not by association: it shares `SLOW_CONNECT_NS`, so a boot
    // that outgrows the window binds the disk in the port scan and it reports
    // `the boot scan bound a disk, so the port was not held empty`. Same
    // measurement, same afternoon.
    ("late_storage_connect", Sched::Serial),
    ("log_backing_read_error", Sched::Parallel),
    ("log_partition_layout", Sched::Parallel),
    ("log_partition_identity", Sched::Parallel),
    ("cache_eviction", Sched::Parallel),
    ("va_exhaustion", Sched::Parallel),
    ("heap_ceiling_recovery", Sched::Parallel),
    ("iommu_context_absent", Sched::Parallel),
    ("iommu_empty_domain", Sched::Parallel),
    // Two boots, one kernel build each: the probe's own, and the plain kernel
    // on the same machine to show it stays out of an ordinary boot.
    ("hda_probe", Sched::Parallel),
    ("serial_vocabulary", Sched::Parallel),
    // Host-side, no guest: the harness asking whether it can still tell a
    // suspended machine from a slow one, and whether it reports one as a
    // verdict it does not have.
    ("suspend_detector", Sched::Parallel),
    ("suspend_invalidates_a_verdict", Sched::Parallel),
];

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
/// columns. QEMU's stdvga GOP is *larger* — the bootloader picks the
/// most-pixels mode and `MAX_ROWS`/`MAX_COLS` cap that at 96x256 — so a line
/// can sit comfortably on the test's screen and fall off the laptop's. Every
/// geometry claim `screen_diag_boot` makes is made against these two numbers
/// and not against the screen it is reading.
const T14_ROWS: usize = 1080 / 16;
const T14_COLS: usize = 1920 / 8;

/// The line `SYS_DEBUG` action 3 logs immediately before halting every CPU.
/// Action 3 exists only under the `test-fatal-halt` kernel feature, which
/// screen_fatal_halt is the only caller of. Kept in sync with
/// `kernel/src/arch/syscall.rs` by this comment and by screen_fatal_halt
/// failing loudly if it drifts.
const FATAL_HALT_NONCE: &str = "SYS_DEBUG: fatal halt 4b1d9e2c";

// C tests that can't compile yet (missing toyos-cc features or unsupported platform APIs).
// Tests that compile successfully are discovered automatically — only list failures here.
const C_SKIP: &[&str] = &[
    "03_struct",              // needs _Generic
    "18_include",             // needs system headers we don't provide
    "31_args",                // needs argc/argv
    "32_led",                 // needs system APIs
    "33_ternary_op",          // needs _Generic
    "40_stdio",               // needs FILE* APIs
    "46_grep",                // needs argc/argv + FILE*
    "60_errors_and_warnings", // meta-test for compiler errors
    "73_arm64",               // wrong architecture
    "101_cleanup",            // needs __attribute__((cleanup))
    "102_alignas",            // needs _Alignas
    "103_implicit_memmove",   // needs __builtin_memmove
    "104_inline",             // needs weak symbols in linker
    "106_versym",             // needs pthread
    "107_stack_safe",         // needs alloca
    "108_constructor",        // needs __attribute__((constructor))
    "109_float_struct_calling", // needs struct-in-register calling convention
    "112_backtrace",          // needs tcc_backtrace
    "113_btdll",              // needs tcc_backtrace
    "114_bound_signal",       // needs sigaction
    "115_bound_setjmp",       // needs setjmp
    "116_bound_setjmp2",      // needs setjmp
    "117_builtins",           // needs __builtin_memmove
    "120_alias",              // needs asm aliases
    "122_vla_reuse",          // VLA codegen bug
    "123_vla_bug",            // VLA codegen bug
    "124_atomic_counter",     // needs stdatomic.h (calls process::exit, not catchable)
    "125_atomic_misc",        // needs stdatomic.h (calls process::exit, not catchable)
    "126_bound_global",       // needs bounds checking
    "127_asm_goto",           // needs inline asm
    "128_run_atexit",         // needs on_exit, and a -D per config to have a main
    "132_bound_test",         // needs bounds checking
    "136_atomic_gcc_style",   // needs stdatomic.h (calls process::exit, not catchable)
];

/// C tests that are discovered, compiled and then thrown away because they do
/// not build. Unlike [`C_SKIP`] these are not a decision, they are the current
/// state of the toolchain — the reason each gives is printed by every run.
const C_DOES_NOT_BUILD: &[(&str, &str)] = &[
    ("78_vla_label", "cranelift verifier rejects the VLA's stack address across blocks"),
    ("79_vla_continue", "same VLA defect, reached through `continue`"),
    ("83_utf8_in_identifiers", "the lexer rejects a non-ASCII byte in an identifier"),
    ("85_asm_outside_function", "file-scope asm is parsed and not emitted, so `vide` is undefined"),
    ("89_nocode_wanted", "expr_type cannot type an identifier under `sizeof` in dead code"),
    ("94_generic", "_Generic type dispatch is not implemented"),
    ("95_bitfields", "aligned(16) on a bitfield member, which toyos-cc refuses"),
    ("95_bitfields_ms", "the same file again, through 95_bitfields.c"),
    ("96_nodata_wanted", "every branch wants a -D the harness does not pass, so no `main`"),
    ("98_al_ax_extend", "file-scope asm again: `_us`, `_ss`, `_uc`, `_sc` are declared in it"),
    ("99_fastcall", "typeof of an `&function` expression is not implemented"),
];

/// Discover C tests by scanning tests/testcases/tinycc/*.c.
/// Skips companion files (contain '+') and tests in C_SKIP.
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
            if C_SKIP.contains(&stem) {
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
fn discover_rust_tests(bins: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut names: Vec<String> = bins
        .iter()
        .filter_map(|(name, _)| {
            if name.ends_with(".so") {
                return None;
            }
            if RUST_SKIP.contains(&name.as_str()) || AUDIO_TESTS.contains(&name.as_str()) {
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

    for (name, why) in &broken {
        eprintln!("[toyos] c::{name} does not build: {why}");
    }
    check_c_build_fixture(&broken.iter().map(|(n, _)| *n).collect::<Vec<_>>());

    bins
}

fn panic_message(e: &Box<dyn std::any::Any + Send>) -> String {
    let full = e
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_string());
    full.lines().next().unwrap_or_default().chars().take(160).collect()
}

/// The suite runs a C test by booting the binary, so one that does not build is
/// not run — and until this fixture, not reported either. The set is asserted
/// in both directions: a case that stops building is a regression, and one that
/// starts building is a fix whose entry has to go.
fn check_c_build_fixture(broken: &[&str]) {
    let expected: BTreeSet<&str> = C_DOES_NOT_BUILD.iter().map(|(n, _)| *n).collect();
    let actual: BTreeSet<&str> = broken.iter().copied().collect();
    if actual == expected {
        return;
    }
    let new: Vec<&str> = actual.difference(&expected).copied().collect();
    let fixed: Vec<&str> = expected.difference(&actual).copied().collect();
    let mut msg = String::from("C_DOES_NOT_BUILD is out of date.\n");
    if !new.is_empty() {
        msg += &format!(
            "  stopped building, and so stopped being run at all: {}\n  \
             (the reason for each is printed above)\n",
            new.join(", ")
        );
    }
    if !fixed.is_empty() {
        msg += &format!("  builds now — delete from C_DOES_NOT_BUILD: {}\n", fixed.join(", "));
    }
    panic!("{msg}");
}

fn check_c_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL c::{test_name}: {err}");
        return false;
    }

    match result.exit_code {
        Some(0) => {
            let expect_file = compile::testcases_dir().join(format!("{test_name}.expect"));
            if expect_file.exists() {
                let expected = fs::read_to_string(&expect_file).unwrap();
                if result.stdout.trim_end() != expected.trim_end() {
                    eprintln!("FAIL c::{test_name}: output mismatch");
                    eprintln!("--- expected ---\n{}", expected.trim_end());
                    eprintln!("--- actual ---\n{}", result.stdout.trim_end());
                    return false;
                }
            }
            true
        }
        Some(code) => {
            eprintln!("FAIL c::{test_name}: exit code {code}\nstdout: {}", result.stdout);
            false
        }
        None => {
            eprintln!("FAIL c::{test_name}: no exit code");
            false
        }
    }
}

fn check_rust_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL rs::{test_name}: {err}");
        return false;
    }

    match result.exit_code {
        Some(0) => true,
        Some(code) => {
            eprintln!("FAIL rs::{test_name}: exit code {code}\nstdout:\n{}", result.stdout);
            false
        }
        None => {
            eprintln!("FAIL rs::{test_name}: no exit code\nstdout:\n{}", result.stdout);
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
        ("!!! PANIC !!!", "expected PANIC header"),
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
    ok
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
    const HEADER: &str = "!!! PANIC !!!";
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

/// Select check function by test name convention.
fn check_for(name: &str) -> fn(&TestResult) -> bool {
    match name {
        "panic_recovery" => check_panic_recovery,
        "audio_idle_suspend" => check_audio_idle_suspend,
        _ => check_rust_result,
    }
}

/// Minimum active (non-silent) playback the 3s test tone must produce.
/// Guards against a vacuous pass when nothing plays at all.
const TONE_MIN_ACTIVE_SECS: f64 = 2.5;
/// The tone is generated at amplitude 16000; a far lower peak proves the
/// signal path is broken even if technically "active".
const TONE_MIN_PEAK: i32 = 4000;

/// Recorded per-(test, smp) baselines — the scheduler-core migration's gate A
/// (specs/scheduler-core-spec.md §11). Two independent instruments per config:
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
            ..Default::default()
        },
    );

    let result = qemu.run_test(&format!("test_rs_{name}"), Duration::from_secs(30));
    if let Some(err) = &result.error {
        return Err(err.clone());
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
    eprintln!(
        "        {label}{name} smp={smp} gaps: {} (baseline {}) peak {} active {:.2}s dither {:.1}%",
        audio::format_histogram(&gaps),
        audio::format_histogram(&baseline.gaps),
        analysis.peak,
        secs(analysis.active_samples),
        analysis.dither_ratio.unwrap_or(0.0) * 100.0,
    );
    eprintln!(
        "        {label}{name} smp={smp} soundd: wake_lat {}us ({:.2} pipelines, limit {}us) \
         drains {}/{} underruns {}/{} submitted {} wakes {} batch {} windows {}",
        counters.max_wake_lat_us,
        counters.max_wake_lat_us as f64 / audio::PIPELINE_DEPTH_US as f64,
        baseline.counters.max_wake_lat_us,
        counters.drains,
        baseline.counters.drains,
        counters.underruns,
        baseline.counters.underruns,
        counters.submitted,
        counters.wakes,
        counters.max_batch,
        counters.windows,
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
/// *distributions* rather than on single outcomes. This is what a
/// scheduler-migration stage transition must pass (spec §11 gate A).
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

/// Assert the two colour decisions `text()` cannot see: the fill, and the
/// alert highlight on a `!!!` line against white everywhere else.
fn check_colors(dump: &screen::Ppm, fill: [u8; 3], alert_line: &str) -> Result<(), String> {
    if dump.fill() != fill {
        return Err(format!("fill is {:?}, want {fill:?}", dump.fill()));
    }
    let rows = dump.rows();
    let Some(cy) = dump.row_index(alert_line) else {
        return Err(format!("{alert_line:?} not on screen"));
    };
    if dump.row_fg(cy) != Some(ALERT) {
        return Err(format!(
            "{alert_line:?} drawn in {:?}, want alert {ALERT:?}",
            dump.row_fg(cy)
        ));
    }
    let Some(plain) = rows.iter().position(|r| !r.is_empty() && !r.contains("!!!")) else {
        return Err("no ordinary row to compare the highlight against".to_string());
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

            for want in ["Boot: complete", "i8042:", "log: this boot is on the console and in"] {
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
            if let Some(row) = dump.rows().iter().find(|r| r.contains("!!!")) {
                return Err(format!(
                    "an alert row on a boot where everything worked: {row:?}\n\
                     decoded screen:\n{text}"
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
            if console.contains("log-file: this boot's kernel log is") {
                return Err(format!(
                    "the sink installed anyway — a fallback is what this must not do:\n{console}"
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
            check_colors(&dump, FILL_BOOT, common::volumes::NO_LOG_ALERT)?;
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
                kernel_features: &["test-screen-graffiti"],
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
                kernel_features: &["test-screen-graffiti"],
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
                let mid: String = std::iter::repeat(fill).take(body).collect();
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
                let done = format!("CHURN-DONE {start} {count}");
                if !qemu.wait_for_console(&done, Duration::from_secs(45)) {
                    return Err(format!("round {round}: the guest never printed `{done}`"));
                }
                let dump = qemu.screendump_while(
                    Duration::from_secs(15),
                    Duration::from_millis(100),
                    |d| {
                        d.console_rows(&font)
                            .last()
                            .is_some_and(|l| l.trim_end().starts_with(CONSOLE_PROMPT))
                    },
                );
                let decoded = dump.console_rows(&font);
                let text = dump.console_text(&font);
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
                kernel_features: &["test-fatal-halt"],
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
                kernel_features: &["test-late-panic"],
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
            let dump = qemu.screendump_until("!!! PANIC !!!", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! PANIC !!!", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!(
                        "{want:?} not on screen of a guest with no serial port at all\ndecoded screen:\n{text}"
                    ));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! PANIC !!!")?;
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
                    kernel_features: &["test-early-panic"],
                    ready_marker: "!!! EARLY PANIC !!!",
                    ..Default::default()
                },
            );
            let dump = qemu.screendump();
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! EARLY PANIC !!!", "test-early-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! EARLY PANIC !!!")?;
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
                    kernel_features: &["test-late-panic"],
                    ready_marker: "!!! PANIC !!!",
                    ..Default::default()
                },
            );
            // Here the marker reaches serial *before* the paint — the drain is
            // what emits it — so unlike the halt paths this one has to look
            // more than once. And once the report outgrows one screen the
            // pager cycles it, so the window in which any given page is up is
            // `PAGE_HOLD_NS`, not forever: the timeout has to cover a whole
            // cycle rather than just the paint.
            let dump = qemu.screendump_until("!!! PANIC !!!", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! PANIC !!!", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! PANIC !!!")?;
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
                    kernel_features: &["test-late-panic"],
                    ready_marker: "!!! PANIC !!!",
                    ..Default::default()
                },
            );

            // The first kernel line of the boot, and the one a photograph of
            // the final screen has never been able to show.
            const HEAD: &str = "panic console: armed";
            const TAIL: &str = "!!! PANIC !!!";

            let mut pages: Vec<String> = Vec::new();
            let mut report: Option<String> = None;
            let mut head_seen = false;
            let deadline = Instant::now() + Duration::from_secs(40);
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
                return Err(format!("{TAIL:?} never reached the screen; footers seen: {seen}"));
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
                    kernel_features: &["test-fatal-halt"],
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
        kernel_features: &["i8042-trace"],
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
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline
        && !WANT.iter().all(|w| boot.console.contains(w))
    {
        boot.drain(Duration::from_millis(250));
    }
    for want in WANT {
        if !boot.console.contains(want) {
            return Err(format!("{want:?} never reached the console:\n{}", boot.console));
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
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline
        && boot
            .console
            .lines()
            .filter(|l| l.contains("compositor: frames=") && l.contains("windows="))
            .count()
            < 3
    {
        boot.drain(Duration::from_millis(250));
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

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline
        && ![PAT, SCANOUT, MAPPED].iter().all(|w| boot.console.contains(w))
    {
        boot.drain(Duration::from_millis(250));
    }
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
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline
        && !boot_log.contains("compositor: frames=")
    {
        boot_log.push_str(&qemu.drain_serial(Duration::from_millis(250)));
    }
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
        return Err(format!(
            "window_drag exited {:?} ({:?}):\n{}",
            result.exit_code, result.error, result.stdout
        ));
    }

    // The client ends on the host's second press, so the interval the drag is
    // in is still open when it exits. Waiting for the line that closes it keeps
    // a slower guest a longer run rather than a different verdict.
    let mut text = result.serial.clone();
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

/// A key injected at the controller, decoded, mapped and delivered to a
/// userland process — IRQ delivery, set-1 decode, the HID mapping and the
/// shared translate/layout path, in one run.
fn i8042_keyboard(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let boot = qemu.boot_log().to_string();
    if !boot.contains("i8042: kbd set2+xlat (readback 0x41)") {
        return Err(format!("the PS/2 keyboard never came up:\n{boot}"));
    }

    let result = qemu.run_test_hooked(
        "test_rs_i8042_keyboard",
        Duration::from_secs(20),
        "===I8042_READY===",
        |socket| {
            for key in ["h", "e", "l", "l", "o"] {
                qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(20));
            }
            qemu::qmp_send_keys(
                socket,
                &[("shift", true), ("b", true), ("b", false), ("shift", false)],
            );
            thread::sleep(Duration::from_millis(20));
            for key in ["left", "esc"] {
                qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                thread::sleep(Duration::from_millis(20));
            }
            // A modifier on its own, so a stuck one is visible.
            qemu::qmp_send_keys(socket, &[("shift", true)]);
            thread::sleep(Duration::from_millis(20));
            qemu::qmp_send_keys(socket, &[("shift", false)]);
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
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
    // Every press is matched by a release.
    for usage in [0x0Bu8, 0x08, 0x0F, 0x12, 0x05, 0x29, 0x50, 0xE1] {
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
    eprintln!("  [i8042] {} events to userland, {drained} from the driver", events.len());
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
/// property from the other side: a guest polling its fd keeps it moving.
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
        "surface: pid",
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

/// Keep collecting serial into `log` until `marker` shows up.
///
/// The layout surfaces have no in-guest test runner, so there is no
/// `===TEST_END===` and nothing to hook: the assertion is what a real program
/// printed on the console of a real image.
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
fn shell_answers(qemu: &mut QemuInstance, log: &mut String) -> bool {
    const NONCE: &str = "surface-up-zqjxk";
    // Retyping rather than waiting longer, because until the terminal has a
    // shell behind it there is nothing to swallow the line and an attempt that
    // lands early leaves no trace. The ceiling on the retries is the phase's:
    // how long a desktop takes to come up is not a verdict.
    let deadline = Instant::now() + qemu::budget(Duration::from_secs(20));
    while Instant::now() < deadline {
        {
            let mut input = qemu::QmpInput::open(qemu.qmp_socket());
            type_line(&mut input, &format!("echo {NONCE}"));
        }
        if serial_until(qemu, log, NONCE, Duration::from_secs(2)) {
            return true;
        }
    }
    false
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
    if !shell_answers(&mut qemu, &mut log) {
        return Err(format!("nothing typed at the terminal window reached a shell:\n{log}"));
    }
    let screen_px = compositor_screen_px(&log)?;

    // Let the interval carrying the boot's full-screen repaint and the
    // terminal's first paint close before anything here is measured. Those are
    // real frames and they are not what this is about.
    if !serial_until(&mut qemu, &mut log, "compositor: frames=", Duration::from_secs(20)) {
        return Err(format!("the compositor never reported an interval:\n{log}"));
    }
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
    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        for _ in 0..8 {
            type_line(&mut input, &format!("echo {NONCE}"));
            thread::sleep(Duration::from_millis(250));
        }
    }
    // Wait for the eight echoes rather than count how many arrived inside a
    // window. A guest that is slow has typed the same eight characters and
    // damaged the same cells; only the wall clock differs, and a verdict that
    // read the clock here would be a verdict about the host's load.
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && log[before..].matches(NONCE).count() < 16 {
        log.push_str(&qemu.drain_serial(Duration::from_millis(250)));
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
    if !shell_answers(&mut qemu, &mut log) {
        return Err(format!("nothing typed at /bin/console reached a shell:\n{log}"));
    }

    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "locale detect");
        if !serial_until(&mut qemu, &mut log, "Press the key labelled", Duration::from_secs(20)) {
            return Err(format!(
                "the wizard never asked for a key under /bin/console — the console did not \
                 lend it the keyboard\n{log}"
            ));
        }
        for key in SWISS_ANSWERS {
            input.keys(&[(key, true), (key, false)]);
            thread::sleep(Duration::from_millis(120));
        }
    }

    for want in ["That is 'swiss-german'", "Keyboard layout set to 'swiss-german'"] {
        if !serial_until(&mut qemu, &mut log, want, Duration::from_secs(20)) {
            return Err(format!("no {want:?} under /bin/console:\n{log}"));
        }
    }
    // The console acted on the notification. A prefix, not the whole line: the
    // console is shared and not line-atomic, so a kernel line lands inside
    // this one often enough to matter (it did, first time this ran). *Which*
    // layout it re-read is the assertion below, which does not depend on a
    // line surviving intact.
    if !serial_until(&mut qemu, &mut log, "console: keyboard layout", Duration::from_secs(10)) {
        return Err(format!("the console never re-read the config the wizard wrote:\n{log}"));
    }

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
    if !serial_until(&mut qemu, &mut log, "\u{fc}", Duration::from_secs(20)) {
        return Err(format!(
            "typing the `[` key after the wizard did not produce `ü`, so the console is still \
             translating with the layout it booted with\n{log}"
        ));
    }
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
    if !shell_answers(&mut qemu, &mut log) {
        return Err(format!("nothing typed at the terminal window reached a shell:\n{log}"));
    }

    {
        let mut input = qemu::QmpInput::open(qemu.qmp_socket());
        type_line(&mut input, "locale detect");
        if !serial_until(&mut qemu, &mut log, "Press the key labelled", Duration::from_secs(20)) {
            return Err(format!(
                "the wizard never asked for a key inside a terminal — the compositor or the \
                 terminal did not carry the transitions\n{log}"
            ));
        }
        for key in SWISS_ANSWERS {
            input.keys(&[(key, true), (key, false)]);
            thread::sleep(Duration::from_millis(120));
        }
    }

    for want in ["That is 'swiss-german'", "Keyboard layout set to 'swiss-german'"] {
        if !serial_until(&mut qemu, &mut log, want, Duration::from_secs(20)) {
            return Err(format!("no {want:?} inside a terminal:\n{log}"));
        }
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
    if !serial_until(&mut qemu, &mut log, "\u{fc}", Duration::from_secs(20)) {
        return Err(format!(
            "typing the `[` key after the wizard did not produce `ü`, so the compositor's \
             broadcast never reached the terminal's translator\n{log}"
        ));
    }
    eprintln!("  [desktop] the wizard ran three processes below the compositor");
    Ok(())
}

/// The direct regression for the readiness defect: a stimulus that produces
/// bytes and no events must produce no wake. Pause is that stimulus — six
/// bytes, deliberately swallowed.
///
/// It drives the same in-guest reader as [`i8042_keyboard`], and not only for
/// the userland half of the assertion: on a fully idle machine the kernel's
/// log ring flushes one line behind, so the last trace line would never reach
/// the console (filed in known-issues). A guest polling its fd keeps the ring
/// moving.
fn i8042_no_spurious_wake(boot: &mut Boot) -> Result<(), String> {
    let qemu = &mut boot.qemu;
    let result = qemu.run_test_hooked(
        "test_rs_i8042_keyboard",
        Duration::from_secs(20),
        "===I8042_READY===",
        |socket| {
            for _ in 0..2 {
                qemu::qmp_send_keys(socket, &[("pause", true), ("pause", false)]);
                thread::sleep(Duration::from_millis(50));
                qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                thread::sleep(Duration::from_millis(50));
            }
        },
    );
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
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
        return Err(format!(
            "no drain produced zero events — the stimulus never landed:\n{}",
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
        "  [i8042] {zero_event_drains} zero-event drains, none woke; {key_drains} real ones, all did"
    );
    Ok(())
}

/// How far the host may run ahead of the guest while it feeds the framer.
///
/// Ninety-six bytes, against a 256-byte ring in the kernel and QEMU's PS/2
/// buffer above it: neither can fill however little of the host the guest is
/// getting, which is what makes `0 dropped` on the driver's counters a
/// statement about the driver.
const MOUSE_LEAD: usize = 32;

/// The TrackPoint path, and a thousand packets through the framer after it,
/// each sent only once the one before it has come out of the guest.
///
/// The pacing is the design. QEMU's PS/2 buffer silently drops a packet it has
/// no room for, so a host injecting at its own speed measures how fast the
/// guest drains and reads the shortfall as a driver defect. Staying behind the
/// guest's own report leaves no loss to tolerate: every packet injected is a
/// packet that arrived, or the run stalls and says how far it got. It is also
/// what makes the driver's `discarded`/`dropped` counters mean something a
/// slow guest cannot account for.
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
                input.mouse(if burst % 2 == 0 { 1 } else { -1 }, 0, None);
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
            // The driver reports its counters from a scheduler pass, and the
            // client polling its fd is what keeps passes running: the line has
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
        return Err(format!(
            "{err} — {arrived} of the {injected} packets injected came back out, so the host \
             stalled on one the machine never delivered\n{}",
            result.stdout
        ));
    }

    let events = parse_mouse_events(&result.stdout);
    // The host sent none of these until the one before it had arrived, so a
    // shortfall is a packet the machine lost and never a host that outran it.
    if events.len() != injected {
        return Err(format!(
            "{} pointer events reached userland out of {injected} packets injected, each one \
             paced against the arrival of the last",
            events.len()
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
        .filter(|l| l.contains("discarded"))
        .next_back()
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
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline && !boot.console.contains("compositor: at most ") {
        boot.drain(Duration::from_millis(250));
    }
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
    let mut after = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && frames(&after) < 2 {
        after.push_str(&qemu.drain_serial(Duration::from_millis(250)));
    }
    if frames(&after) < 2 {
        return Err(format!(
            "the compositor reported {} frame batches in the 20 s after the last stall:\
             \n{after}",
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
    const CASES: usize = 4;
    if !result
        .stdout
        .contains(&format!("compositor client death: {CASES} deaths survived"))
    {
        return Err(format!(
            "the guest did not report {CASES} survived deaths:\n{}",
            result.stdout
        ));
    }

    // Non-vacuity, and the case that motivated the whole run: at least one of
    // the eight creators has to have been reaped before the compositor served
    // its window, or nothing here exercised the grant that killed the desktop.
    const VANISHED: &str = "the process behind it has exited";
    let vanished = result.stdout.matches(VANISHED).count();
    if vanished == 0 {
        return Err(format!(
            "eight clients asked for a window and died, and the compositor served every one of \
             them before it noticed — this run proves nothing about the grant:\n{}",
            result.stdout
        ));
    }

    // Still painting once every case is behind it, on a capture that starts
    // empty — so this counts frames produced *after* the last case. The
    // compositor's reporting interval is 2 s.
    let frames = |text: &str| text.matches("compositor: frames=").count();
    let mut after = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline && frames(&after) < 2 {
        after.push_str(&qemu.drain_serial(Duration::from_millis(250)));
    }
    if frames(&after) < 2 {
        return Err(format!(
            "the compositor reported {} frame batches in the 20 s after the last client died:\
             \n{after}",
            frames(&after)
        ));
    }

    let console = format!("{}\n{after}", result.serial);
    serial::Serial::named("boot console", console.as_str()).must_be_clean()?;
    eprintln!(
        "  [metal-sim] {CASES} client deaths survived, {vanished} of {VANISHERS} creators \
         vanished before their window, desktop still compositing"
    );
    Ok(())
}

/// How many clients `compositor_client_death` kills mid-request. Restated from
/// the guest so the report can say how many of them the compositor met dead.
const VANISHERS: usize = 8;

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
    // (It is also what `specs/test-cost-audit.md` §3.3's parallel boots would
    // have to fix first.)
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
        "usb_storage_write_error" => usb::usb_storage_write_error(test_config, c_bins, rust_bins),
        "usb_flush_optional" => usb::usb_flush_optional(test_config, c_bins, rust_bins),
        "xhci_deaf_registers" => usb::xhci_deaf_registers(test_config, c_bins, rust_bins),
        "xhci_slow_connect" => usb::xhci_slow_connect(test_config, c_bins, rust_bins),
        "xhci_portsc_rw1c" => usb::xhci_portsc_rw1c(test_config, c_bins, rust_bins),
        "usb_transport_break" => usb::usb_transport_break(test_config, c_bins, rust_bins),
        "xhci_full_speed_device" => {
            usb::xhci_full_speed_device(test_config, c_bins, rust_bins)
        }
        "xhci_hotplug" => usb::xhci_hotplug(test_config, c_bins, rust_bins),
        "xhci_flap" => usb::xhci_flap(test_config, c_bins, rust_bins),
        "xhci_hid_break" => usb::xhci_hid_break(test_config, c_bins, rust_bins),
        // Body in `tests/common/iommu.rs`, same reason.
        "iommu_discovery" => common::iommu::iommu_discovery(test_config, c_bins, rust_bins),
        "iommu_context_absent" => common::iommu::iommu_context_absent(test_config, c_bins, rust_bins),
        "iommu_empty_domain" => common::iommu::iommu_empty_domain(test_config, c_bins, rust_bins),
        // Body in `tests/common/hda.rs`, same reason.
        "hda_probe" => common::hda::hda_probe(test_config, c_bins, rust_bins),
        "double_fault_stack" => faults::double_fault_stack(test_config, c_bins, rust_bins),
        "idle_stack_guard" => faults::idle_stack_guard(test_config, c_bins, rust_bins),
        "diskless_boot" => faults::diskless_boot(test_config, c_bins, rust_bins),
        // Body in `tests/common/audio.rs`, so the hunk here stays one line.
        "metal_sim_null_audio" => audio::null_sink_real_rate(test_config, c_bins, rust_bins),
        "doom_sound_flood" => audio::doom_sound_flood(rust_bins),
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
            let result = qemu.run_test_hooked(
                "test_rs_input_events",
                Duration::from_secs(30),
                "===INPUT_READY===",
                |socket| {
                    let mut input = qemu::QmpInput::open(socket);
                    // Off the origin first: the accumulated position clamps at
                    // 0, so a move up or left from there is invisible.
                    input.mouse(100, 100, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(DX, DY, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(0, 0, Some(("left", true)));
                    thread::sleep(Duration::from_millis(50));
                    input.mouse(0, 0, Some(("left", false)));
                    thread::sleep(Duration::from_millis(100));
                    for key in ["h", "e", "l", "l", "o"] {
                        input.keys(&[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
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
        "serial_vocabulary" => serial::self_check(),
        "suspend_detector" => common::clock::self_check(),
        "suspend_invalidates_a_verdict" => suspend_invalidates_a_verdict(),
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
                kernel_features: &["test-tiny-va"],
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
            // The actuator is `test-heap-ceiling`'s three SYS_DEBUG actions,
            // and the reason it is not an ordinary workload is on the feature
            // in `kernel/Cargo.toml`: routes past the ceiling do still exist,
            // and each of them holds the VFS lock when it dies, so the
            // machine wedges either way and the allocator's recovery cannot
            // be observed on its own.
            let options = BootOptions {
                smp: 1,
                kernel_features: &["test-heap-ceiling"],
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
            serial.must_say("!!! PANIC !!!")?;
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

            let options = BootOptions {
                profile: qemu::Profile::MetalDisk,
                kernel_features: &["test-small-caches"],
                ..Default::default()
            };
            let mut qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();

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
                kernel_features: &["xhci-one-slot"],
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
        "input_merge" => {
            // The check runs in the kernel and panics on mismatch, so a
            // failure arrives as a dead boot; the marker is the only proof it
            // ran at all.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_features: &["test-input-merge"],
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
                    kernel_features: &["i8042-fast-health"],
                    ..Default::default()
                },
            );
            if !qemu.boot_log().contains("i8042: kbd set2+xlat") {
                return Err(format!("the PS/2 keyboard never came up:\n{}", qemu.boot_log()));
            }
            // One key, a silence several periods long, then one more key. The
            // guest program holds the keyboard fd for 5 s and the period is
            // 500 ms, so the quiet stretch is nine periods with nothing to say.
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(30),
                "===I8042_READY===",
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
                "===I8042_READY===",
                |socket| {
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
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
                    kernel_features: &["xhci-descriptor-selftest"],
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
                    kernel_features: &["xhci-xecp-selftest"],
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
                    kernel_features: &["i8042-budget-expired"],
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
                kernel_features: &["i8042-fadt-denial"],
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
                "===I8042_READY===",
                |socket| {
                    for key in ["h", "e", "l", "l", "o"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
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
                kernel_features: &["i8042-kbd-echo"],
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
                "===I8042_READY===",
                |socket| {
                    for key in ["h", "e", "l", "l", "o"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
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
                "===I8042_READY===",
                |socket| {
                    qemu::qmp_send_keys(socket, &[("pause", true), ("pause", false)]);
                    thread::sleep(Duration::from_millis(200));
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }
            let Some(mute) = result.serial.lines().find(|l| l.contains("nothing decoded")) else {
                return Err(format!(
                    "bytes arrived and decoded to nothing and the driver never said so:\n{}",
                    result.serial
                ));
            };
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
            let Some(alive) = result.serial.lines().find(|l| l.contains("the pin asserts")) else {
                return Err(format!(
                    "a letter was typed after the undecoded bytes and the driver never \
                     revised its verdict:\n{}",
                    result.serial
                ));
            };
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
                    kernel_features: &["i8042-fault"],
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
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(30),
                "===I8042_READY===",
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
            // idle loop logs its health every 1000 iterations and halts when
            // there is nothing to do, so a spinning CPU is loud: the first
            // version of this driver left the `irq_ring` record undrained
            // after quarantine and produced 2685 of these lines in 5 s,
            // against 1 on a healthy run.
            let health = result.serial.matches("sched: cpu=").count();
            if health > 50 {
                return Err(format!(
                    "{health} idle-health lines after the quarantine — a CPU is spinning, not halting"
                ));
            }
            eprintln!("  [i8042] {}", line.trim());
            eprintln!("  [i8042] {health} idle-health lines — the CPU still halts");
            Ok(())
        }
        "metal_sim_window_drag" => metal_sim_window_drag(rust_bins),
        "metal_sim_pointer_churn" => {
            // The owner froze his desktop twice by plugging a mouse in and
            // pulling it out again, and the second freeze landed on the fourth
            // cycle's enumeration. The compositor holds the merged pointer's
            // fd across all of it, so every cycle is a source binding and
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
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
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
            let bound = console.matches("merges as source").count();
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
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
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
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && !console.contains("netd: ready, at most ") {
                console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
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
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && !console.contains("netd: ready, at most ") {
                console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
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
            // share one window (known-issues §6), which here is what makes the
            // daemon's side of the story readable at all.
            console.push_str(&result.serial);
            for named in ["netd: dropping pid", "netd: refusing pid"] {
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
                int_ring: usize::from_str_radix(rest.trim().split_whitespace().next()?, 16).ok()?,
            })
        })
        .collect()
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
                source.trim().split_whitespace().next()?.parse().ok()?,
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
            _ => Duration::from_secs(5),
        };
        tests.push(TestDef {
            qemu_name: format!("test_rs_{name}"),
            check: check_for(&name),
            timeout,
            name,
        });
    }

    for name in c_names {
        tests.push(TestDef {
            qemu_name: format!("test_c_{name}"),
            timeout: Duration::from_secs(10),
            check: check_c_result,
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
    let kernel_elf = repo.join("kernel/target/x86_64-unknown-none/debug/kernel");

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
enum Task<'a> {
    /// Every Rust and C test, on the one guest they share.
    Shared(Vec<&'a TestDef>),
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
#[derive(PartialEq, Debug)]
enum Verdict {
    Pass,
    Fail,
    /// The host stopped in the middle of it. Neither a pass nor a fail: the
    /// guest, QEMU's virtual clock and every wall-clock margin the test's
    /// assertion rests on all jumped by however long the lid was closed, so the
    /// run measured something and it was not this tree.
    Invalid,
}

impl Outcome {
    fn verdict(&self) -> Verdict {
        if self.suspended >= common::clock::SUSPENDED_AT_LEAST {
            return Verdict::Invalid;
        }
        match self.reason {
            None => Verdict::Pass,
            Some(_) => Verdict::Fail,
        }
    }
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
    let jitter = common::clock::SUSPENDED_AT_LEAST - Duration::from_millis(1);
    let cases: [(&str, Option<&str>, Duration, Verdict); 6] = [
        ("a pass on a host that stayed up", None, awake, Verdict::Pass),
        ("a fail on a host that stayed up", Some("the guest said no"), awake, Verdict::Fail),
        ("a pass across a suspend", None, slept, Verdict::Invalid),
        ("a fail across a suspend", Some("timed out"), slept, Verdict::Invalid),
        ("a pass across clock jitter", None, jitter, Verdict::Pass),
        ("a fail across clock jitter", Some("the guest said no"), jitter, Verdict::Fail),
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

/// The task that would run `name` again, by itself.
///
/// **Every red from the parallel phase is re-run alone**, and the two possible
/// answers are both findings. Same verdict: the defect is real and the width had
/// nothing to do with it. Green: the test is red only when it shares the host,
/// which makes its [`Sched::Parallel`] wrong — a bug in this file, not in the
/// kernel, and one the suite has no other way to notice.
///
/// **A green retry does not turn the run green.** A rerun-only pass counting as
/// a pass is `specs/test-cost-audit.md` §3.7 by the back door; the failure line
/// says which of the two it was and the run stays red until somebody fixes the
/// classification. That is the whole safety argument for widening the parallel
/// phase: getting a scheduling answer wrong costs a red run, never a quiet one.
///
/// A group member is re-run **as its group**, not on its own, so that the only
/// thing that changed between the two attempts is how many guests the host had.
fn retry_task<'a>(name: &str, all_tests: &[&'a TestDef]) -> Option<Task<'a>> {
    if let Some(def) = all_tests.iter().find(|t| t.name == name) {
        return Some(Task::Shared(vec![def]));
    }
    if let Some((registered, _)) = SCREEN_TESTS.iter().find(|(n, _)| *n == name) {
        return Some(Task::Screen(registered));
    }
    let (registered, _) = MACHINE_TESTS.iter().find(|(n, _)| *n == name)?;
    let names = match group_of(registered) {
        None => vec![*registered],
        Some(group) => MACHINE_TESTS
            .iter()
            .filter(|(n, _)| group_of(n) == Some(group))
            .map(|(n, _)| *n)
            .collect(),
    };
    Some(Task::Machine(names))
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
        Task::Shared(tests) => {
            // The boot itself can fail, and it used to take the run with it.
            // Reporting the block's tests against its reason keeps the count
            // honest and says which one it died on.
            let mut done = 0usize;
            let outcome = catching(|| {
                let mut qemu = QemuInstance::boot(bins.test_config, bins.c_bins, bins.rust_bins);
                for test in &tests {
                    let start = common::clock::mark();
                    let result = qemu.run_test(&test.qemu_name, test.timeout);
                    let reason = (!(test.check)(&result)).then(|| {
                        result
                            .error
                            .clone()
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
            Task::Shared(tests) => tests.iter().map(|t| t.name.as_str()).collect(),
            Task::Machine(names) => names.to_vec(),
            Task::Screen(name) => vec![name],
        }
    }
}

/// Where the last run in this worktree left what each test cost it.
///
/// Under `target/`, so it is per-worktree and never committed: it is a *hint*
/// about how to order a queue and never an input to a verdict. A wrong number
/// costs some idle lane time; a missing one costs nothing at all.
fn durations_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-durations")
}

fn load_durations() -> BTreeMap<String, Duration> {
    let mut out = BTreeMap::new();
    let Ok(text) = fs::read_to_string(durations_path()) else { return out };
    for line in text.lines() {
        if let Some((name, ms)) = line.rsplit_once(' ') {
            if let Ok(ms) = ms.parse::<u64>() {
                out.insert(name.to_string(), Duration::from_millis(ms));
            }
        }
    }
    out
}

/// Merge this run's durations into the recorded profile.
///
/// Merged rather than replaced, because a filtered run knows about four tests
/// and would otherwise throw away what the last full one measured.
fn save_durations(mut known: BTreeMap<String, Duration>, timed: &[(String, Duration)]) {
    for (name, elapsed) in timed {
        known.insert(name.clone(), *elapsed);
    }
    let body: String =
        known.iter().map(|(n, d)| format!("{n} {}\n", d.as_millis())).collect();
    let path = durations_path();
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
    match outcome.verdict() {
        Verdict::Pass => eprintln!("  PASS  {}  ({:.0?})", outcome.name, outcome.elapsed),
        Verdict::Fail => {
            let reason = outcome.reason.as_deref().unwrap_or("check failed");
            eprintln!("FAIL {}: {reason}", outcome.name);
            eprintln!("  FAIL  {}  ({:.0?})", outcome.name, outcome.elapsed);
        }
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

/// Every claim the two registration lists make about themselves, before
/// anything boots.
///
/// A group whose members drifted apart still passes — each one boots its own
/// machine and reads its own console — so nothing downstream would notice, and
/// a group split across the two phases could not share a guest at all.
fn check_registration() {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for (name, _) in MACHINE_TESTS.iter().chain(SCREEN_TESTS) {
        assert!(seen.insert(name, ()).is_none(), "{name} is registered twice");
    }
    for name in AUDIO_TESTS {
        assert!(!seen.contains_key(name), "{name} is registered twice");
    }

    let mut groups: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for (i, (name, _)) in MACHINE_TESTS.iter().enumerate() {
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
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let debug_mode = args.iter().any(|a| a == "--debug");
    let list_mode = args.iter().any(|a| a == "--list");
    let nocapture = args.iter().any(|a| a == "--nocapture" || a == "--show-output");

    // Thorough tier. A flag rather than an env var or a test name: an env var
    // is invisible in the command line and easy to leave set, and a test name
    // would drag ~17 minutes into every plain `cargo test`.
    let mut audio_gate: Option<u32> = None;
    let mut consumed: Vec<usize> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--audio-gate=") {
            consumed.push(i);
            v
        } else if a == "--audio-gate" {
            consumed.push(i);
            consumed.push(i + 1);
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
            consumed.push(i);
            v
        } else if a == "--jobs" || a == "-j" {
            consumed.push(i);
            consumed.push(i + 1);
            args.get(i + 1)
                .map(|s| s.as_str())
                .unwrap_or_else(|| panic!("--jobs needs a width, e.g. --jobs 4"))
        } else {
            continue;
        };
        width = n.parse().unwrap_or_else(|_| panic!("--jobs: {n:?} is not a width"));
        assert!(width >= 1, "--jobs needs at least one worker");
    }

    // How many guests may be up on the *host* at once, across every worktree.
    // `--jobs` is this run's demand; this is what the machine will supply, and
    // zero turns it off.
    let mut host_budget = toyos_build::buildlock::HOST_GUESTS;
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--host-slots=") {
            consumed.push(i);
            v
        } else if a == "--host-slots" {
            consumed.push(i);
            consumed.push(i + 1);
            args.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| {
                panic!("--host-slots needs a budget, e.g. --host-slots 12 (0 turns it off)")
            })
        } else {
            continue;
        };
        host_budget =
            n.parse().unwrap_or_else(|_| panic!("--host-slots: {n:?} is not a budget"));
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

    // Filter: first positional arg that isn't a flag
    let filter: Option<&str> = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && !consumed.contains(i))
        .map(|(_, s)| s.as_str());

    let c_names = discover_c_tests();
    eprintln!("[toyos] Compiling {} C tests...", c_names.len());
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
        for name in AUDIO_TESTS {
            println!("{name}");
        }
        for (name, _) in SCREEN_TESTS {
            println!("{name}");
        }
        for (name, _) in MACHINE_TESTS {
            println!("{name}");
        }
        return;
    }

    if debug_mode {
        run_debug_mode(&c_bins, &rust_bins);
        return;
    }

    if let Some(iterations) = audio_gate {
        let audio_to_run: Vec<&str> = AUDIO_TESTS
            .iter()
            .copied()
            .filter(|n| filter.map_or(true, |f| n.contains(f)))
            .collect();
        assert!(!audio_to_run.is_empty(), "no audio test matches filter {filter:?}");
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
    let tests_to_run: Vec<&TestDef> = match filter {
        Some(f) => all_tests.iter().filter(|t| t.name.contains(f)).collect(),
        None => all_tests.iter().collect(),
    };
    let audio_to_run: Vec<&str> = AUDIO_TESTS
        .iter()
        .copied()
        .filter(|n| filter.map_or(true, |f| n.contains(f)))
        .collect();
    let screen_to_run: Vec<(&str, Sched)> = SCREEN_TESTS
        .iter()
        .copied()
        .filter(|(n, _)| filter.map_or(true, |f| n.contains(f)))
        .collect();
    let machine_to_run: Vec<(&str, Sched)> = MACHINE_TESTS
        .iter()
        .copied()
        .filter(|(n, _)| filter.map_or(true, |f| n.contains(f)))
        .collect();

    if tests_to_run.is_empty()
        && audio_to_run.is_empty()
        && screen_to_run.is_empty()
        && machine_to_run.is_empty()
    {
        eprintln!("No tests match filter {:?}", filter);
        std::process::exit(1);
    }

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let total = tests_to_run.len()
        + audio_to_run.len() * AUDIO_SMP.len()
        + screen_to_run.len()
        + machine_to_run.len();
    eprintln!("\nrunning {total} tests\n");
    let mut passed = 0;
    let mut failed = 0;
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut invalid: Vec<(String, Duration)> = Vec::new();
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
    // its longest job and the durations that would order it are not in the tree
    // — see `specs/test-cost-audit.md` §5.3, which measures the deficit and says
    // what it would take to close it.
    let mut parallel: Vec<Task> = Vec::new();
    let mut serial: Vec<Task> = Vec::new();
    if !tests_to_run.is_empty() {
        eprintln!(
            "[toyos] The shared boot carries {} C + {} Rust binaries",
            c_bins.len(),
            rust_bins.len()
        );
        let task = Task::Shared(tests_to_run.clone());
        match SHARED_BLOCK {
            Sched::Parallel => parallel.push(task),
            Sched::Serial => serial.push(task),
        }
    }
    for (sched, names) in machine_tasks(&machine_to_run) {
        let task = Task::Machine(names);
        match sched {
            Sched::Parallel => parallel.push(task),
            Sched::Serial => serial.push(task),
        }
    }
    for (name, sched) in &screen_to_run {
        let task = Task::Screen(name);
        match sched {
            Sched::Parallel => parallel.push(task),
            Sched::Serial => serial.push(task),
        }
    }

    let mut record = |outcomes: Vec<Outcome>| {
        for outcome in outcomes {
            match outcome.verdict() {
                Verdict::Pass => passed += 1,
                Verdict::Fail => {
                    failed += 1;
                    let reason = outcome.reason.unwrap_or_else(|| "check failed".to_string());
                    let summary = reason.lines().next().unwrap_or("check failed");
                    failures.push((outcome.name, summary.to_string()));
                }
                Verdict::Invalid => invalid.push((outcome.name, outcome.suspended)),
            }
        }
    };

    // Every red the wide phase produced, re-run by itself before anything is
    // believed about it. See [`retry_task`] for why both answers are findings
    // and why neither turns the run green.
    let known = load_durations();
    let mut timed: Vec<(String, Duration)> = Vec::new();
    let mut wide_reds: Vec<String> = Vec::new();
    if !parallel.is_empty() {
        longest_first(&mut parallel, &known);
        eprintln!("  --- parallel, {width} wide ---");
        let started = std::time::Instant::now();
        let outcomes = run_phase(parallel, width, &bins, &slots);
        eprintln!("  --- parallel done in {:.1?} ---", started.elapsed());
        // Reds only. A test the host slept through has no verdict to confirm,
        // and re-running it would put a second guess beside the first.
        wide_reds.extend(
            outcomes
                .iter()
                .filter(|o| o.verdict() == Verdict::Fail)
                .map(|o| o.name.clone()),
        );
        timed.extend(outcomes.iter().map(|o| (o.name.clone(), o.elapsed)));
        record(outcomes);
    }
    if !serial.is_empty() {
        eprintln!("  --- serial ---");
        let started = std::time::Instant::now();
        let outcomes = run_phase(serial, 1, &bins, &slots);
        eprintln!("  --- serial done in {:.1?} ---", started.elapsed());
        timed.extend(outcomes.iter().map(|o| (o.name.clone(), o.elapsed)));
        record(outcomes);
    }
    qemu::set_width(1);
    save_durations(known, &timed);

    if !wide_reds.is_empty() {
        eprintln!("  --- re-running {} wide failure(s) alone ---", wide_reds.len());
        for name in &wide_reds {
            let Some(task) = retry_task(name, &tests_to_run) else {
                eprintln!("  ALONE {name}: no way to run it by itself; verdict stands");
                continue;
            };
            let outcomes = run_phase(vec![task], 1, &bins, &slots);
            match outcomes.iter().find(|o| &o.name == name).map(Outcome::verdict) {
                Some(Verdict::Pass) => eprintln!(
                    "  ALONE {name}: GREEN — it fails only beside other guests, so its \
                     Sched::Parallel is wrong. The run stays red on the classification."
                ),
                Some(Verdict::Fail) => eprintln!("  ALONE {name}: red again — the defect is real."),
                Some(Verdict::Invalid) => {
                    eprintln!("  ALONE {name}: the host was suspended during the retry too")
                }
                None => eprintln!("  ALONE {name}: the lone run reported nothing about it"),
            }
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
                record(vec![outcome]);
            }
        }
    }

    let suite_elapsed = suite_start.elapsed();
    let suite_suspended = suite_start.suspended();

    eprintln!();
    if suite_suspended >= common::clock::SUSPENDED_AT_LEAST {
        // The elapsed figure above is monotonic and therefore already excludes
        // it, which is worth saying: the two numbers do not add up unless a
        // reader knows that.
        eprintln!(
            "note: the host was suspended for {suite_suspended:.0?} during this run. \
             The suite time below excludes it."
        );
    }
    if !failures.is_empty() {
        eprintln!("failures:");
        for (name, reason) in &failures {
            eprintln!("    {name}: {reason}");
        }
        eprintln!();
    }
    if !invalid.is_empty() {
        eprintln!("invalidated by host suspend:");
        for (name, slept) in &invalid {
            eprintln!("    {name}: the host was stopped for {slept:.0?} while it ran");
        }
        eprintln!();
    }

    // Three exit statuses, because there are three things a run can establish.
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
    if !failures.is_empty() {
        eprintln!(
            "test result: FAILED. {passed} passed, {failed} failed, {} invalidated, \
             {total} total ({suite_elapsed:.1?})",
            invalid.len()
        );
        std::process::exit(1);
    }
    if !invalid.is_empty() {
        eprintln!(
            "test result: INVALID. {passed} passed, {} invalidated by a host suspend of \
             {suite_suspended:.0?}, {total} total ({suite_elapsed:.1?})",
            invalid.len()
        );
        eprintln!(
            "This is not a red. The machine stopped mid-run, so those verdicts are of \
             nothing; re-run the suite."
        );
        std::process::exit(2);
    }
    eprintln!("test result: ok. {passed} passed, {total} total ({suite_elapsed:.1?})");
}
