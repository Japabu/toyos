//! What this kernel has been told to break, and nothing a boot decides.
//!
//! Every actuator here used to be a cargo feature, so every one of them was a
//! kernel build: 45 of them in a full `cargo test`, each the shipping kernel
//! plus one live path changed. Now there are two kernels — the one an image
//! ships, which carries none of this, and the one the suite boots, which
//! carries all of it and arms whichever the boot parameter names.
//!
//! **The shipping kernel is not this kernel with the switches off.** Without
//! `boot-actuators` every accessor below is `const fn … { false }`, so a call
//! site folds to the shipped branch and a constant an actuator moves stays a
//! constant. There is no arm-set, no name, and no parser: `init` refuses a
//! parameter rather than ignoring one, because a shipping kernel handed a test
//! parameter must not boot pretending it was given nothing.
//!
//! The parameter arrives in [`toyos_abi::boot::KernelArgs`], written to
//! `\toyos\cmdline` on the ESP by `src/image.rs` and passed on by the
//! bootloader. That is what makes it readable before the first actuator fires:
//! `test-early-panic` panics between arming the on-screen console and
//! `mm::init`, and `no-ap-control-regs` acts at AP bring-up, so nothing the
//! kernel could mount or ask for later is early enough.
//!
//! It is our own bootloader's string and crosses no trust boundary, so an
//! unknown token is a bug in this build system and panics by name.

#[cfg(feature = "boot-actuators")]
use core::sync::atomic::{AtomicU64, Ordering};

/// Every declared actuator: the accessor the kernel calls, the name a boot
/// parameter uses, and what nothing else can reach.
///
/// **The comment on each is the claim that earns it a place** — why the state
/// under it cannot be staged from the host side, which is what separates an
/// actuator from a harness that could have injected the same thing from
/// outside — and it lives here rather than in `kernel/Cargo.toml` because this
/// is the file that has to stay true when one changes.
macro_rules! actuators {
    ($( $(#[$doc:meta])* $name:ident = $wire:literal; )*) => {
        /// In bit order: an actuator's bit is its index here.
        #[cfg(feature = "boot-actuators")]
        const NAMES: &[&str] = &[$($wire),*];

        $(
            $(#[$doc])*
            #[cfg(feature = "boot-actuators")]
            #[inline(always)]
            pub fn $name() -> bool {
                const AT: (usize, u64) = bit_of($wire);
                ARMED[AT.0].load(Ordering::Relaxed) & AT.1 != 0
            }

            $(#[$doc])*
            #[cfg(not(feature = "boot-actuators"))]
            // **The one allow this file needs, and it is the shipping arm.**
            // Nineteen of these accessors have their only call site inside a
            // module that is itself `#[cfg(feature = "boot-actuators")]` —
            // `input_merge_test`, `usb_gate`, `heartbeat`, the xHCI break
            // arms — so in a shipping kernel the constant is compiled and the
            // caller is not. Deleting them is not the answer: the whole claim
            // of this file is that the shipping kernel folds every actuator to
            // a constant, and an accessor that exists only in the test kernel
            // makes that claim per-actuator instead of whole.
            #[allow(dead_code)]
            #[inline(always)]
            pub const fn $name() -> bool {
                false
            }
        )*
    };
}

actuators! {
    /// Panic between arming the on-screen console and `mm::init`, so a test can
    /// assert the console covers the window where nothing else can report.
    test_early_panic = "test-early-panic";

    /// One log line per i8042 drain: bytes seen, events queued, whether the
    /// queue was woken. The only way a test can assert the wake guard from
    /// outside. Implies `i8042-fast-health` and `i8042-edge-race`: a kernel
    /// built to be watched has no use for a counter line every 10 s, and the
    /// trace group is the only boot that counts interrupts against bytes.
    i8042_trace = "i8042-trace";

    /// Hold a scheduler pass between reading the source's `irq_ring` record and
    /// reading its byte ring, so an interrupt lands in between and the pass sees
    /// bytes it has been told nothing about. Unwidened that window is a handful
    /// of instructions on one CPU: no injection the harness can time and no load
    /// it can stage reaches it.
    i8042_edge_race = "i8042-edge-race";

    /// Make the ISR's output-buffer check lie, so its 16-byte bound trips and
    /// the quarantine path runs. A controller that chatters forever is the one
    /// case the bound alone still lets livelock a CPU, and nothing else can
    /// stage it.
    i8042_fault = "i8042-fault";

    /// Set the i8042 probe's whole init budget to zero, so its expiry paths run
    /// on a controller that is answering perfectly. QEMU answers every step in
    /// microseconds — the whole probe measures 0.099 s to 0.100 s on a
    /// metal-sim boot — and no real EC has ever been timed, so nothing else can
    /// reach them.
    i8042_budget_expired = "i8042-budget-expired";

    /// Hand the i8042 probe the T14's own FADT answer — revision 6,
    /// `iapc_boot_arch=0x0011`, the 8042 bit clear — on a machine that has a
    /// working controller. QEMU cannot stage that disagreement:
    /// `-machine q35,i8042=off` clears the bit by removing the device and
    /// `-device i8042` puts the device back into the QOM tree the bit is read
    /// from, so there the claim and the hardware always agree.
    i8042_fadt_denial = "i8042-fadt-denial";

    /// Answer the i8042 probe's `0xF0 0x00` scancode-set query with `0xEE` —
    /// ECHO's own reply, and what the T14's EC answered on the laptop's own
    /// screen — on a keyboard that is otherwise working. QEMU's PS/2 keyboard
    /// implements `0xF0` to the letter (`hw/input/ps2.c`, `KBD_CMD_SCANCODE`)
    /// and no device or machine property makes it stop. Only the verdict is
    /// replaced; both bytes still go out.
    i8042_kbd_echo = "i8042-kbd-echo";

    /// Shorten the i8042's post-verdict counter report from 10 s to 500 ms. The
    /// report is what turns "the log went silent" into a readable fact, and its
    /// whole claim is about what happens over tens of seconds: one line after
    /// the pin asserts, none while it is quiet, another when it asserts again.
    /// No guest test program lives that long, and stretching every input test to
    /// watch a log cadence is the wrong trade. Only the period moves.
    i8042_fast_health = "i8042-fast-health";

    /// Shorten the idle loop's own health/PMM snapshot cadence
    /// (`scheduler.rs`'s `snapshot_interval_ns`) from 10 s to 200 ms. The line
    /// carries a per-CPU idle-trip counter that is not itself rate-limited, but
    /// the *print* is — so telling a spinning CPU from a halting one needs at
    /// least two prints to compare, and no guest test program this suite runs
    /// lives past the shipped 10 s once, let alone twice. Only the period
    /// moves: the counter, the fields and the halt/spin behaviour underneath
    /// are the shipped ones. See `i8042_quarantine`'s idle-trip check,
    /// `issues/kernel/i8042-quarantine-health-line-count-is-vacuous.md`.
    sched_fast_health = "sched-fast-health";

    /// Script the input core directly at end of boot. QEMU activates one input
    /// handler per device class, so two keyboards or two pointers can never both
    /// be live in a guest — the merge has no end-to-end test and this is its
    /// only one.
    test_input_merge = "test-input-merge";

    /// Clamp the xHCI driver to one device block, so a test can drive the path
    /// where the controller hands back a slot the DMA pool has no room for.
    /// QEMU's `nec-usb-xhci,slots=N` does not reach HCSPARAMS1 and its Enable
    /// Slot ignores the MaxSlotsEn a driver writes.
    xhci_one_slot = "xhci-one-slot";

    /// Run the xHCI extended-capability walk over eight malformed lists at init.
    /// The list comes from firmware and QEMU's controller publishes none at all,
    /// so without this the bounds in `xhci/legacy.rs` would ship never having
    /// executed.
    xhci_xecp_selftest = "xhci-xecp-selftest";

    /// Read and write every USB disk that carries the gate's stamp in block 0,
    /// at the end of the peripheral phase. A raw block device has no path to
    /// userland, so the kernel is the only in-guest actor that can drive one —
    /// and the stamp, not the parameter, is what decides which disk gets
    /// written: the boot stick is on the same bus and must come back untouched.
    /// See `kernel/src/usb_gate.rs`.
    usb_storage_gate = "usb-storage-gate";

    /// Ask the NVMe disk for a block with the caller's operation budget already
    /// spent, once, as soon as the page cache has the device — under both of
    /// its locks, which is where every real caller asks from.
    ///
    /// `block::OPERATION` bounds a controller that answers every command and
    /// takes too long over the composition above them, and nothing on the host
    /// side reaches that state: QEMU's NVMe answers in microseconds and
    /// `rerror`/`werror` fail a command rather than delaying one, so a disk slow
    /// enough to reach two seconds cannot be staged and an injection that faked
    /// one would have to spend the two seconds. An operation that is already
    /// over is established instead. Armed by `cache_eviction`, which is the
    /// registered name that already boots this kernel on a real namespace.
    /// See `kernel/src/nvme_gate.rs`.
    nvme_spent_budget = "nvme-spent-budget";

    /// Establish three nested `scheduler::Operation`s with known deadlines and
    /// report what every level observed and what each drop restored — once from
    /// a boot phase, which has no task, and once from `iod`'s body, which is
    /// one.
    ///
    /// **The law it reads has no host-side reader and cannot get one.**
    /// `Operation::begin` stores `outer.min(until)`, so an inner establishment
    /// may only narrow; the type reaches `percpu::cpu_id` and
    /// `driver::current_handle`, and `kernel/` is excluded from the host
    /// workspace, so nothing off a booted machine can construct one. The
    /// establishments the other gates drive are incidental to what those gates
    /// are about — `nvme-spent-budget` proves a narrowing happened by the
    /// refusal it produces and reads none of the values — and every one of them
    /// is the task-less kind.
    ///
    /// It measures and stages nothing: no driver behaviour changes with it on,
    /// no device is touched, and every guard is dropped before the function
    /// returns. See `kernel/src/sched_gate.rs`.
    sched_operation_nesting = "sched-operation-nesting";

    /// Answer SYNCHRONIZE CACHE with the ILLEGAL REQUEST / INVALID COMMAND
    /// OPERATION CODE a stick with no write cache gives. QEMU's `scsi-disk`
    /// implements 0x35 for every front end that reaches it, so the difference
    /// between a device that will not flush and one that cannot is unreachable
    /// from the host side. The command is still issued; only its verdict is
    /// replaced. See `xhci/wait/msc.rs`'s `flush_sense`.
    usb_flush_unimplemented = "usb-flush-unimplemented";

    /// The same, with a HARDWARE ERROR: a device that cannot flush rather than
    /// one that will not.
    usb_flush_fails = "usb-flush-fails";

    /// Abandon the data phase of the first WRITE(10) of the boot without waiting
    /// for it. QEMU's `usb-storage` answers every CBW, data phase and CSW it is
    /// handed, in microseconds and in order; `rerror`/`werror` fail the whole
    /// drive instead of leaving a transfer in flight. Nothing is faked: the TRB
    /// is really on the ring, the endpoint is really left Running, and the
    /// controller really completes the transfer afterwards — only the wait is
    /// skipped, which is the state a transfer that ran out `USB_TIMEOUT_NS`
    /// leaves behind. See `xhci/wait/msc.rs`'s `transport_break`.
    usb_transport_break = "usb-transport-break";

    /// Under-deliver one READ(10) data phase where the gate asks for it, so that
    /// the controller's count of the bytes it moved and the device's own CSW
    /// residue disagree. QEMU derives both from one transfer, so they can never
    /// contradict each other there; on real hardware a device that ends its data
    /// phase early and reports `dCSWDataResidue` as zero is a firmware bug that
    /// ships. The bytes left in the tail of the window are the previous
    /// transfer's, read off that window rather than invented. Implies
    /// `usb-storage-gate`, which is the only caller that asks for it.
    /// See `xhci/wait/msc.rs`'s `short_read`.
    usb_short_read = "usb-short-read";

    /// Hold every mass-storage bulk completion back for 2 ms before the driver
    /// may see it, which is what a USB flash stick's erase block does to a 4 KiB
    /// write and what the T14's audio pops are made of. QEMU's `usb-storage`
    /// answers in microseconds and has no property that delays a transfer, so
    /// this is the only way the suite can ask what the machine does while a
    /// device is slow. The event is the controller's own and the bytes really
    /// moved; what is replaced is when the driver is allowed to see it.
    /// See `xhci/mod.rs`'s `SLOW_TRANSFER_NS`.
    usb_slow_device = "usb-slow-device";

    /// Report the preempt depth at the deepest point of a disk transfer, with
    /// the backtrace that got there. The one number that says how much of the
    /// kernel is holding a spinlock while a device is being waited for, and one
    /// no static read of the call graph produces: it is 5 on an ordinary boot.
    /// The instrument the work in
    /// `issues/kernel/every-wait-in-this-kernel-is-a-spin.md` is judged on: it
    /// has to fall. It measures and stages nothing — no driver behaviour
    /// changes with it on.
    io_depth_probe = "io-depth-probe";

    /// Starve the four xHCI bring-up register waits in `init_one` on a
    /// controller that is otherwise answering. QEMU's xHC halts, resets, clears
    /// CNR and starts in microseconds; no device or machine property makes a
    /// register bit not settle. See `xhci/mod.rs`'s `settles`.
    xhci_deaf_controller = "xhci-deaf-controller";

    /// The same for the port-reset wait in `init_device`, which QEMU finishes
    /// synchronously and where unplugging between the port scan and the reset is
    /// not expressible either.
    xhci_deaf_port = "xhci-deaf-port";

    /// Make one CPU ignore a kick — the one machine state the blocked-task dump
    /// exists to describe, and the one nothing on the host can stage: QEMU
    /// delivers every IPI it is given. The victim really clears IF and really
    /// spins, for longer than the dump's kick budget and then no longer, so the
    /// kick is genuinely unanswered, the NMI is genuinely what reaches it, and
    /// the CPU genuinely comes back. See `sched/dump.rs`'s `deaf_window`.
    dump_deaf_cpu = "dump-deaf-cpu";

    /// Storm the CPU that is spinning on `syscall` from Ring 3 with NMIs, and
    /// count how many arrive at CPL 0 with a user `rsp` — the window
    /// `arch::idt`'s IST2 row exists for.
    ///
    /// **Where an asynchronous interrupt lands is decided inside the guest.**
    /// QEMU delivers every IPI it is given and has no way to aim one at an
    /// instruction, so there is no device, machine property or monitor command
    /// that puts an NMI in a three-instruction window; the alternative — a
    /// kernel that pretends one arrived — would certify nothing about the stack
    /// the CPU actually pushes on. Nothing here is faked: another CPU sends it,
    /// the victim takes it where it is, and the classification is read off the
    /// CPU's own frame.
    ///
    /// **The storm arms on the victim's own syscall count and never on a
    /// clock**, so it cannot fire at a machine where nothing is spinning yet —
    /// what that cost while it was a wall-clock instant is written at
    /// `nmi_gate::SPINNING_SYSCALLS`. Implies `diag-tick`, because the sending
    /// CPU looks from its idle loop and a quiet CPU would otherwise sleep
    /// through the run. See `kernel/src/nmi_gate.rs`.
    syscall_window_nmi = "syscall-window-nmi";

    /// Take the IST index off vector 2's gate, leaving the handler, the ring and
    /// everything else exactly as it ships.
    ///
    /// **The kernel this tree had until 2026-08-22, and the negative control on
    /// the row above.** With it the CPU builds the NMI frame at whatever `rsp`
    /// holds, so an NMI in the syscall window writes to a user page from CPL 0,
    /// SMAP refuses, and the machine takes the `#DF` this whole change is about.
    /// The IDT is the guest's own memory and no QEMU property edits it, so the
    /// state is unreachable from the host side; and it replaces the *behaviour*
    /// rather than a verdict, which is what keeps the gate above non-vacuous.
    nmi_without_ist = "nmi-without-ist";

    /// Return from inside the NMI handler through an `iretq` with a second NMI
    /// already pending, which is the one way a second NMI can enter on the
    /// stack the first is standing on.
    ///
    /// **The re-entrancy the design argues cannot happen, staged rather than
    /// assumed.** The architecture blocks NMI delivery until the handler's
    /// `iretq`, so nesting needs an early one — a handler that faults gets it
    /// for free, and Linux's nested-NMI machinery exists for exactly that. No
    /// host-side stimulus can produce it: it is one CPU, inside a handler,
    /// between two instructions. What it drives is whether `nmi_entry`'s check
    /// fires and the machine says so, or whether the outer frame is silently
    /// overwritten. See `kernel/src/nmi_gate.rs`'s `stage_nested_if_armed`.
    nmi_nested = "nmi-nested";

    /// Report an empty root hub for the first 300 ms of the boot, so the port
    /// scan runs against a controller that has not finished detecting its
    /// devices yet. That is what every physical root hub does after HCRST and
    /// what QEMU's cannot: it answers PORTSC from the QOM tree, so an attached
    /// device reads CCS the instant the register is touched. The register is
    /// replaced, not a verdict. See `xhci/mod.rs`'s `SLOW_CONNECT_NS`.
    xhci_slow_connect = "xhci-slow-connect";

    /// Report the *first* root-hub port empty for the same window, leaving every
    /// other port on the machine reading normally. A different machine from the
    /// one above and not a weaker version of it: `await_connect_settle` ends as
    /// soon as the connect set has held still and is non-empty, so a bus whose
    /// other devices have settled settles on them — which is the T14, with four
    /// internal USB devices beside the stick it boots from. Hiding the whole bus
    /// can never reach it. See `xhci/mod.rs`'s `SLOW_STORAGE_PORT`.
    xhci_slow_storage_connect = "xhci-slow-storage-connect";

    /// Give PORTSC's PED bit (bit 1) the RW1CS meaning xHCI 1.2 §5.4.8 gives it:
    /// a port software writes a '1' to goes from Enabled to Disabled
    /// (§4.19.1.1.6) and reads PED clear until it is reset again. QEMU's
    /// `xhci_port_write` clears only CSC|PEC|WRC|OCC|PRC|PLC|CEC on a written
    /// '1' and PED is in neither that set nor its read/write set. The register
    /// is replaced, not a verdict: the port reads exactly what the T14's five
    /// ports read. See `XhciController::software_disabled`.
    xhci_portsc_rw1c = "xhci-portsc-rw1c";

    /// Take a bound HID device's very first completion away and hand the driver
    /// a stall in its place, which is the shape a Logitech mouse hot-plugged
    /// into the T14 showed. QEMU's `usb-hid` completes every interrupt TRB it is
    /// given — `usb_hid_handle_data` answers an IN token on endpoint 1 with a
    /// report or with NAK and has no path to `USB_RET_STALL` for it. What is
    /// replaced is the code *and the report that transfer delivered*: without
    /// taking the bytes away a driver that dispatched it anyway would publish a
    /// delta it never earned. See `xhci/hid.rs`'s `stage_break`.
    xhci_hid_break_first = "xhci-hid-break-first";

    /// The same at its fourth, where a device that has been delivering stops.
    xhci_hid_break_late = "xhci-hid-break-late";

    /// Run `parse_config` over nine crafted configuration descriptors at init.
    /// The bytes come from a device and every device QEMU can attach describes
    /// itself correctly, so without this the parser's refusals — an endpoint
    /// address naming endpoint 0 above all — would ship never having executed.
    /// See `xhci/device.rs`'s `selftest`.
    xhci_descriptor_selftest = "xhci-descriptor-selftest";

    /// Run `Virtqueue::poll_used` over eleven crafted used-ring elements at
    /// init. Both fields of a used-ring element are written by the device, and
    /// every virtio device QEMU implements writes correct ones — no device
    /// property, machine property or backend makes one report a head
    /// descriptor it was never given or a length past the buffer it was
    /// posted. So without this the parse's refusals would ship never having
    /// executed. The queue and its DMA page are real and the shipped
    /// `poll_used` is what runs; only the writer of the ring is the kernel
    /// instead of a device. See `drivers/virtio.rs`'s `used_selftest`.
    virtio_used_selftest = "virtio-used-selftest";

    /// Leave every AP holding the CR0 and CR4 that INIT left it, which is what
    /// every boot before `arch/control_regs.rs` was: caching disabled, WP clear,
    /// NE clear. `control_regs_negative` boots it and holds the verdict against
    /// what comes out. The per-CPU line and both assertions are the shipped ones
    /// and only the two register writes are absent, so the CPU it names is a
    /// real divergent CPU.
    ///
    /// Nothing else can stage it. A control register is written by the guest and
    /// read by nobody outside it, so there is no QEMU device, machine property
    /// or `-cpu` flag that leaves one wrong.
    no_ap_control_regs = "no-ap-control-regs";

    /// Time the same read loop on every CPU, either side of the `mov cr0` that
    /// turns its caching on. See `arch/control_regs.rs`'s `bench` for why
    /// nothing outside the kernel can ask this and why the answer is not on this
    /// host: TCG models no cache at all, so `CR0.CD` there costs nothing and the
    /// number is metal's.
    control_regs_bench = "control-regs-bench";

    /// Shrink both disk caches to 64 entries each. The honest ceilings are tens
    /// of megabytes, so a test that reached them by doing real I/O would spend
    /// minutes proving what 256 KiB proves in a second. The eviction code under
    /// test is the shipped code; only the bound moves.
    test_small_caches = "test-small-caches";

    /// Shrink each process's VA arena from ~1015 GB to 256 MiB, so that
    /// exhausting it is a test rather than a physics problem. Every region costs
    /// at worst twice its size in physical memory, so the shipped arena needs
    /// upwards of 500 GB of RAM to fill and the PMM refuses long first —
    /// `find_gap` cannot be made to fail by any workload this harness can
    /// express. See `vma::alloc_floor`.
    test_tiny_va = "test-tiny-va";

    /// Panic once the boot phases are done, with no thread current, so a test
    /// can drive the ordinary fatal panic — the one path where the report
    /// reaches the screen only because the panic handler captured it before
    /// draining the ring.
    test_late_panic = "test-late-panic";

    /// Take a Ring 0 `#UD` once the boot phases are done, with no thread
    /// current, so `fatal_exception` runs its `Blame::Kernel` arm.
    ///
    /// **Nothing outside the kernel can make the kernel fault.** Every
    /// exception this suite stages is a *program*'s — a segfault, an illegal
    /// instruction, a bad syscall pointer — and all of them end at
    /// `recover_or_halt`'s process arm. There is no QEMU device, machine
    /// property or guest program that puts a Ring 0 frame on a bad instruction,
    /// so before this the kernel arm of the fault path, and the `DOUBLE PANIC`
    /// branch reachable only through it, had never been executed by a test at
    /// all.
    test_kernel_fault = "test-kernel-fault";

    /// Panic inside the crash report, before it has said anything: at the head
    /// of `crash_report_panic` and at the head of `fatal_exception`, which are
    /// the two reports this kernel writes.
    ///
    /// **The panic path panicking is not stageable from the host in any other
    /// way.** It is a second failure *inside* the handler for the first, on one
    /// CPU, between two statements — no injection reaches there, and the
    /// sighting that asked for it took two worktrees' suites running at once to
    /// produce once
    /// (`issues/panic-path/a-double-panic-at-boots-edge-says-nothing-but-its-name.md`).
    /// Armed alone it changes nothing: a boot that reports no crash reaches
    /// neither site. What it drives is which words a machine that is two bugs
    /// deep leaves behind — the reentry guard's when the first crash was a
    /// panic, and `DOUBLE PANIC`'s when it was a fault.
    panic_in_report = "panic-in-report";

    /// Panic a few seconds after a *compositor* has claimed the framebuffer,
    /// from an idle CPU, so the panic handler's recovery branch is not taken and
    /// the report reaches `halt_all_cpus`.
    ///
    /// What nothing else reaches: whether a fatal report lands on the panel of
    /// the owner's T14 while his desktop owns the scanout.
    /// `screen_fatal_halt_composited` certifies the software half and cannot
    /// certify more — QEMU's framebuffer is host RAM, while the T14's is a
    /// write-combining MMIO mapping the compositor is also writing, and a full
    /// paint measures ~460 ms there. Three investigations into the machine
    /// freeze have assumed a fatal panic would have been seen if one had
    /// happened; this is the boot that turns that assumption into an
    /// observation. `test-late-panic` cannot serve: it fires during the boot
    /// phases, before any userland process exists.
    ///
    /// Implies `diag-tick`, because the probe's deadline is read from the idle
    /// loop; without it the probe fires on the first interrupt after the
    /// deadline rather than at the deadline, which on the owner's T14 was 99
    /// seconds late and looked like a probe that never fired.
    metal_panic_probe = "metal-panic-probe";

    /// Cap how long a CPU with nothing to run may sleep, so the idle loop keeps
    /// running on a machine that has gone quiet.
    ///
    /// The shipping kernel is right not to: a CPU whose pass found no work and
    /// no deadline stops its LAPIC timer and halts until an interrupt arrives,
    /// which is correct and is good power management. But everything the kernel
    /// does *for the person watching it* lives in the idle loop — the heartbeat,
    /// the probe's deadline, the log sink's flush — so on a machine that has
    /// gone quiet none of it runs. Eight boots of the owner's T14 produced two
    /// heartbeats each, at 1.15 s and 1.78 s, then nothing for as long as 102 s
    /// until a keypress.
    ///
    /// It costs the wakes and the flushes they carry — with `heartbeat` armed,
    /// four `sync_mount`s a second on whatever `/log` sits on — which also means
    /// the instrument is no longer a passive observer of the boot it is
    /// watching. A shipping build cannot carry it, and now cannot: the accessor
    /// is `false` there and the idle loop has no branch at all.
    diag_tick = "diag-tick";

    /// One log line every 250 ms carrying the monotonic time and which CPUs are
    /// still alive.
    ///
    /// What nothing else reaches: *was this boot alive at time T*. Ten logs off
    /// the owner's stick are byte-identical between the boots that froze and the
    /// boots that did not — a live idle desktop and a dead machine have the same
    /// last thing to say — so the log records the freeze's existence and never
    /// its time. With this, a frozen boot's log ends at the last heartbeat
    /// before death, and the mask says whether the machine stopped all at once
    /// or a CPU at a time.
    ///
    /// Implies `diag-tick`, which is what makes the mask mean that: without it a
    /// clear bit means "this CPU had nothing to do", which every idle CPU on a
    /// quiet machine has.
    heartbeat = "heartbeat";

    /// Every CPU emitting patterned log records at once, from kernel threads the
    /// boot's first `SYS_LOG_READ` spawns.
    ///
    /// **Nothing else can reach it.** A real workload's record rate is set by
    /// what the kernel happens to log — a handful of lines a second — and
    /// cannot be made to saturate a shard however long a test waits, while the
    /// property under test is exactly what happens when producers outrun the
    /// one reader and the ring begins to drop. Nothing is faked: the records go
    /// through the shipped `emit`, the shipped reservation and the shipped
    /// publication, and only their number and their text belong to the test.
    /// See `kernel/src/log/storm.rs`.
    log_storm = "log-storm";

    /// Remove §2.3a's IF/TF bracket from shard selection through final
    /// publication, leaving `LogCommitGuard` a guard that masks nothing.
    ///
    /// **This is the correctness claim the whole design rests on, and the only
    /// thing that can make it fail on purpose.** Its reader is
    /// `log_reserve_window_negative` at `--smp 8`, which boots it beside
    /// `log-nested-reserve` and holds the log gate's verdict against what comes
    /// out: with the bracket gone the handler's records take the sequence
    /// numbers the interrupted producer had already stamped a timestamp for, so
    /// the shard's sequence order stops being its timestamp order and the
    /// reader refuses the descent. Nothing on the host can stage it — there is
    /// no injection that interrupts a kernel between two instructions.
    ///
    /// **The other half of the claim is unreachable on this kernel, and saying
    /// so is the honest state.** `arch::LogCommitGuard::close` also argues that
    /// the bracket stops a producer *migrating* between its shard-pointer read
    /// and its `xadd`, which would put two CPUs on one `head`. Preemption here
    /// is deferred: `need_resched` is polled by `preempt::enable` and by the
    /// Ring 3 exit check, `arch::idt`'s `common_entry` returns to a Ring 0
    /// frame without polling either, and the bracket contains no preemption
    /// point — so no Ring 0 producer can be switched out inside that window at
    /// any rate, and only a task that is Ready ever migrates.
    /// `log::storm`'s header carries the same finding from the other end,
    /// measured: 0 of 8 and 0 of 16 producers with records on a second shard.
    /// `issues/kernel/a-ring-0-loop-is-never-preempted.md`
    /// is the entry. See `arch::LogCommitGuard::close`.
    log_unbracketed_reserve = "log-unbracketed-reserve";

    /// Send this CPU its own IPI from halfway through a log record's body copy,
    /// and have the handler emit exactly one shard generation of patterned
    /// records — an interrupt that logs, inside another `emit`, on one CPU.
    ///
    /// **The case loom cannot express.** Loom models threads, not CPU flags and
    /// not strict LIFO reentrancy on one CPU, so §2.4's fourth property has no
    /// model; and nothing on the host can interrupt a kernel between two
    /// instructions of one function. With §2.3a's bracket the IPI is pending
    /// across the whole publication and lands the instant the guard drops, so
    /// the burst laps the shard and the outer record goes by the ring's own
    /// drop-oldest policy. Without it the same IPI lands inside the copy.
    /// See `kernel/src/log/nested.rs`.
    log_nested_emit = "log-nested-emit";

    /// The same IPI, sent from **between the shard-pointer read and the
    /// unlocked `xadd`** instead — the first window §2.3a's bracket names.
    ///
    /// **The row above stages a corruption no reader can see, and this one
    /// stages the corruption they can.** A writer lapped mid-body republishes
    /// the previous generation's sequence number, which is exactly what an
    /// unpublished slot looks like and is already below
    /// `Shard::oldest_readable` — so what it costs is one record, and one
    /// record is what the ring is allowed to drop. Here the damage is to the
    /// *order*: `emit` stamps `at_ns` before it reserves, so a handler that
    /// reserves from inside this window takes the lower sequence numbers under
    /// the later timestamps and the interrupted producer's own record lands
    /// above them carrying a timestamp from before all of them.
    /// `read.rs`'s `Descent::advance` is written against exactly that not
    /// happening — an early stop that would drop a mid-`emit` CPU's whole
    /// answer to Ctrl+Alt+D — and `test-runner`'s log gate refuses a shard
    /// whose `at_ns` descends. Nothing on the host reaches it, for the row
    /// above's reason. See `kernel/src/log/nested.rs`'s `reserve_window`.
    log_nested_reserve = "log-nested-reserve";

    /// Turn the reservation's one unlocked `xadd` into a load, an open
    /// interrupt window and a store — the shape §2.3a says is not
    /// interrupt-atomic.
    ///
    /// **The window is what makes it deterministic rather than a race.** The
    /// defect being staged is precisely "something came between the load and
    /// the store", and the only thing that can be made to come between them on
    /// one CPU is an interrupt this kernel sent itself: `log-nested-emit`'s
    /// one-shot is consumed here instead of mid-body, so the handler's first
    /// record takes the sequence number the interrupted writer had already
    /// read. Nothing else changes; every other reservation is the same two
    /// instructions with nothing admitted between them.
    /// See `arch::percpu_fetch_add`.
    log_shared_reservation = "log-shared-reservation";

    /// Let a handle close cancel every poll in the machine on `Source::Log` —
    /// which is every `SysCap`'s.
    ///
    /// **A real prior behaviour and the defect `/bin/logd` would have lived
    /// under.** `ops::close` cancelled by source across every ring, which is
    /// right for a pipe and wrong for a stream that outlives every handle: any
    /// process closing any capability posted `-NotFound` into logd's parked
    /// poll. It cannot be staged from the host — which process closes which
    /// handle is decided inside the guest, and the two processes involved need
    /// not know about each other at all, which is the whole shape of the bug.
    ///
    /// It used to cover the keyboard too, because the question was asked of the
    /// object and one switch answered for both of the objects that got it
    /// wrong. The question is the *source*'s now
    /// (`Source::ended_by_its_last_handle`), so the keyboard has its own name
    /// below and this one restores exactly the log half.
    log_close_cancels_any_syscap = "log-close-cancels-any-syscap";

    /// Let a handle close cancel every poll in the machine on
    /// `Source::Keyboard` — the keyboard claim's, and every `Console`'s.
    ///
    /// **The keyboard half of the row above, and a live cross-cancellation
    /// rather than an invented one.** While `object::ops` asked the question of
    /// the object, `Device(_)` answered "this ends its sources" unconditionally,
    /// so the one process holding the keyboard claim closing its handle posted
    /// `-NotFound` into every pending `POLL_ADD` on stdin in the machine —
    /// libc's terminal read is what arms them, so the blast radius was every
    /// program waiting for a keystroke, none of which holds a device or was
    /// consulted. It cannot be staged from the host: which process closes which
    /// handle is decided inside the guest, and the claim's holder and the poll's
    /// owner need not know about each other at all, which is the whole shape of
    /// the bug. `Source::ended_by_its_last_handle`, in `kernel/src/inbox.rs`.
    keyboard_close_cancels_every_console = "keyboard-close-cancels-every-console";

    /// Bypass `ConsoleObject`'s line buffer: every userland `write` reaches the
    /// backend as it arrives.
    ///
    /// **A real prior build rather than an invented defect** — it is exactly
    /// what this tree shipped between L3 and L5, and before that the byte ring
    /// made the unit of interleaving a `write` syscall in a lossier way still.
    /// The host cannot stage it: `println!` is `LineWriter`, whose two syscalls
    /// per line are decided inside the guest's own `std`, and no host-side
    /// stimulus can make the kernel forget a buffer it holds. What it produces
    /// is a line one process began and another finished —
    /// `console_line_atomicity` counts them and `Serial::interleaved` names the
    /// kernel-into-userland half.
    console_unbuffered = "console-unbuffered";

    /// Panic inside `klogd`, the kernel thread, on its first instruction.
    ///
    /// **Nothing outside the kernel can make a kernel thread panic**, and the
    /// verdict is not the panic but which *branch* the panic handler takes: a
    /// kernel task has a tid, so the recovery predicate's second clause holds,
    /// and its first reads a `syscall_rip` that is never cleared. Without
    /// `sched::kthread`'s row the outcome is decided by which CPU work stealing
    /// last put a user thread on, so no host-side stimulus could exist even in
    /// principle — there is no process to kill and no syscall to make.
    klogd_panic = "klogd-panic";

    /// Panic inside `usbd`, the second kernel thread, on its first instruction.
    ///
    /// **`klogd-panic`'s other half, and it is the half nothing has ever run.**
    /// The two threads carry opposite rows in `sched::kthread`: `klogd`'s panic
    /// halts the machine, `usbd`'s kills the thread and the machine carries on.
    /// Until this actuator existed only the halting branch had ever been taken
    /// by a kernel thread, so "recoverable" was a value in a table rather than a
    /// path anything had walked — and the recovery it names runs through
    /// `poison_tid`, the idle loop's `reap_poisoned` and `zombify_poisoned`,
    /// none of which had ever seen a task with no address space of its own. The
    /// host has no stimulus for it even in principle: there is no process to
    /// kill and no syscall to make.
    usbd_panic = "usbd-panic";

    /// Stop the boot dead in phase 3, with interrupts off, before anything that
    /// could ever have drained a log.
    ///
    /// **A machine that wedges cannot be staged from the host at all** — there
    /// is no injection that stops a kernel between two statements, and a QEMU
    /// pause stops the guest without leaving it in the state under test, which
    /// is a CPU that will never reach a scheduler pass. What the gate reads is
    /// the *console*: before `Drain::Inline` a boot that stopped here produced
    /// nothing whatsoever, including everything it had logged
    /// (`issues/diagnostics/pre-idle-wedge-says-nothing.md`), because the
    /// only two drains in the machine were the timer tick and the idle loop.
    pre_idle_wedge = "pre-idle-wedge";

    /// Fail every re-read of a page of a file on either FAT mount through
    /// `FatBacking`, with the mount and the filesystem underneath it working.
    /// Both partitions are on the disk the guest is running from, so no
    /// QEMU-side way of failing its reads leaves the machine booted and the
    /// volumes mounted — `readonly=on` is writes only and `rerror` takes the
    /// whole drive. The read is still issued and only its verdict is replaced.
    /// What it drives is the partial write an appender makes into an evicted
    /// page — `log_file`'s until L6 and `/bin/logd`'s since, which is the same
    /// path through the page cache and a *more* reachable one, because a
    /// userland writer's tail page is ordinary evictable cache.
    /// See `fat32_adapter.rs`'s `fat_backing_reads`.
    fat_backing_read_fails = "fat-backing-read-fails";

    /// Fail every *filesystem* read of the boot volume once it is mounted, with
    /// the mount and the rest of the machine working. Its sibling above injects
    /// one layer higher, at `FatBacking::read_page`, which is the page-fault path
    /// and reaches no directory entry — so with that one armed an `open`, a
    /// `read_dir` and an mtime of the same volume all still succeed. This one is
    /// under `Fat32` itself, where a directory entry, a FAT chain and an extent
    /// list are read. `Role::Boot` because nothing in the kernel reads that
    /// volume after the mount, so the log, the shell and the serial console all
    /// survive and a process can be sent to ask it a question.
    /// See `fat32_adapter.rs`'s `boot_volume_reads`.
    fat_boot_reads_fail = "fat-boot-reads-fail";

    /// Leave the NVMe controller out of the IOMMU's root table, so it reaches
    /// translation with no context entry. A root table, a context entry and a
    /// page table are all the kernel's own memory, so no QEMU device or machine
    /// property can reach that state while leaving the rest of the machine
    /// correct. Nothing is faked: the transaction really happens, the unit
    /// really blocks it, and the fault record the handler reads is the
    /// hardware's own. See `iommu/vtd/mod.rs`'s `enable`.
    iommu_context_absent = "iommu-context-absent";

    /// Give it a present context entry naming a second-level table with no
    /// mappings in it. The second of the pair, and not a weaker first: together
    /// they are the negative control on the whole of stage I2 — a unit that were
    /// bypassing or never enabled would let both through, and this one alone
    /// separates a real second-level walk from a context entry naming
    /// passthrough, which would fault identically for the first.
    iommu_empty_domain = "iommu-empty-domain";

    /// Run the HDA register allow-list over every arm of it at bind time, and
    /// report each verdict by name. Nothing else can be the caller: the check is
    /// gated on holding the device claim, soundd takes that claim for the life of
    /// the boot, and the claim is exclusive by construction — so a guest test can
    /// only reach the allow-list on a machine where nothing is driving the sound
    /// card, which is a machine with no audio. The shipped path runs; only the
    /// caller is staged. See `kernel/src/drivers/hda.rs`'s `allowlist_selftest`.
    hda_allowlist_selftest = "hda-allowlist-selftest";

    /// A wall clock whose update flag never clears. QEMU has no switch that
    /// removes or wedges the mc146818 and its RTC always presents the guest a
    /// coherent register set. What is replaced is what the *hardware* answers;
    /// the decoder and everything downstream of it are shipped code.
    /// See `kernel/src/rtc.rs`.
    rtc_dead = "rtc-dead";

    /// One whose registers never settle: no two of four reads agree.
    rtc_unstable = "rtc-unstable";

    /// Firmware that names no century register. The FADT a guest reads is
    /// generated by QEMU and always names it at 0x32.
    /// See `drivers/acpi.rs`'s `rtc_century_register`.
    rtc_no_century = "rtc-no-century";

    /// A century register reading 0x21. `-rtc base=` sets every digit of the
    /// date except the century — measured, a guest booted at 2101 reads century
    /// 20 and year 01.
    rtc_century_next = "rtc-century-next";

    /// A machine whose firmware names its zone. OVMF ships
    /// `EFI_UNSPECIFIED_TIMEZONE` and nothing in QEMU sets the UEFI variable that
    /// would change it, so without this every emulated boot assumes UTC and the
    /// arithmetic between local time and UTC never runs. See `clock::init_wall`.
    rtc_zone_east = "rtc-zone-east";
}

/// What arming one arms with it, exactly as `kernel/Cargo.toml`'s feature
/// implications used to.
#[cfg(feature = "boot-actuators")]
const IMPLIES: &[(&str, &[&str])] = &[
    ("i8042-trace", &["i8042-fast-health", "i8042-edge-race"]),
    ("usb-short-read", &["usb-storage-gate"]),
    ("metal-panic-probe", &["diag-tick"]),
    ("heartbeat", &["diag-tick"]),
    ("syscall-window-nmi", &["diag-tick"]),
];

/// Words the arm set takes, from how many names there are.
///
/// **One was exactly enough until 2026-08-22 and then it was not.** The 64th
/// actuator filled it and the 65th made `1 << i` overflow, which the `const`
/// block at the foot of this file refused by name — so the wall was a compile
/// error and never a boot that quietly read somebody else's bit. Derived rather
/// than written down, so the next name past a multiple of 64 costs nothing and
/// no second number has to be kept agreeing with `NAMES`.
#[cfg(feature = "boot-actuators")]
const ARM_WORDS: usize = NAMES.len().div_ceil(u64::BITS as usize);

#[cfg(feature = "boot-actuators")]
static ARMED: [AtomicU64; ARM_WORDS] = [const { AtomicU64::new(0) }; ARM_WORDS];

/// Arm what the boot parameter names, before anything can read it.
///
/// Called from `kernel_main` before the earliest actuator site and before any
/// AP exists, so every later read is a plain relaxed load of a word this CPU
/// wrote or a word a CPU that did not yet exist will find already written.
///
/// **The set is built whole and published word by word, and that is sound for
/// the same reason** — there is no reader yet, so a torn publication is not a
/// state anything can observe.
///
/// A token this kernel does not declare panics by name. It came from our own
/// image builder through our own bootloader, so it is a bug in this build
/// system and not input — there is no trust boundary anywhere on that path.
#[cfg(feature = "boot-actuators")]
pub fn init(cmdline: &str) {
    let mut armed = [0u64; ARM_WORDS];
    for token in cmdline.split(',').filter(|t| !t.is_empty()) {
        arm(&mut armed, token);
    }
    for (name, implied) in IMPLIES {
        if is_armed(&armed, name) {
            for one in *implied {
                arm(&mut armed, one);
            }
        }
    }
    for (word, value) in ARMED.iter().zip(armed) {
        word.store(value, Ordering::Relaxed);
    }
    if armed.iter().any(|&word| word != 0) {
        log!("actuators: {cmdline}");
    }
}

/// Set `name`'s bit in a set being built.
#[cfg(feature = "boot-actuators")]
fn arm(armed: &mut [u64; ARM_WORDS], name: &str) {
    let (word, bit) = at(index_of(name));
    armed[word] |= bit;
}

/// Is `name`'s bit set in a set being built? [`IMPLIES`] asks; nothing else
/// does, because every other reader is an accessor with its bit resolved at
/// compile time.
#[cfg(feature = "boot-actuators")]
fn is_armed(armed: &[u64; ARM_WORDS], name: &str) -> bool {
    let (word, bit) = at(index_of(name));
    armed[word] & bit != 0
}

/// An index in [`NAMES`] as the word it lives in and the bit inside that word.
/// One expression, so the accessor's `const` and the two runtime callers cannot
/// disagree about where a name's bit is.
#[cfg(feature = "boot-actuators")]
const fn at(index: usize) -> (usize, u64) {
    (index / u64::BITS as usize, 1 << (index % u64::BITS as usize))
}

/// A kernel with no actuators in it refuses a parameter rather than ignoring
/// one: a boot that was asked for a machine this binary cannot be must not come
/// up looking like the machine that was asked for.
#[cfg(not(feature = "boot-actuators"))]
pub fn init(cmdline: &str) {
    assert!(
        cmdline.is_empty(),
        "this kernel carries no actuators and was handed the boot parameter {cmdline:?}"
    );
}

#[cfg(feature = "boot-actuators")]
fn index_of(name: &str) -> usize {
    let mut i = 0;
    while i < NAMES.len() {
        if NAMES[i] == name {
            return i;
        }
        i += 1;
    }
    panic!("boot parameter {name:?}: this kernel declares no such actuator");
}

/// The word and bit `name` is armed in, resolved where a typo is a compile error
/// rather than a boot that quietly reads somebody else's actuator.
#[cfg(feature = "boot-actuators")]
const fn bit_of(name: &str) -> (usize, u64) {
    let mut i = 0;
    while i < NAMES.len() {
        if str_eq(NAMES[i], name) {
            return at(i);
        }
        i += 1;
    }
    panic!("undeclared actuator");
}

#[cfg(feature = "boot-actuators")]
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(feature = "boot-actuators")]
const _: () = {
    assert!(
        ARM_WORDS * u64::BITS as usize >= NAMES.len(),
        "the arm set has fewer bits than there are actuators"
    );
    let mut i = 0;
    while i < NAMES.len() {
        let mut j = i + 1;
        while j < NAMES.len() {
            assert!(!str_eq(NAMES[i], NAMES[j]), "two actuators share a name");
            j += 1;
        }
        i += 1;
    }
    // Every side of [`IMPLIES`] too, so a name it misspells is this build
    // failing rather than the one boot that asks for it panicking.
    let mut i = 0;
    while i < IMPLIES.len() {
        let (name, implied) = IMPLIES[i];
        let _ = bit_of(name);
        let mut j = 0;
        while j < implied.len() {
            let _ = bit_of(implied[j]);
            j += 1;
        }
        i += 1;
    }
};
