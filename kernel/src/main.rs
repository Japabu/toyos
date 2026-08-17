#![no_std]
#![no_main]
extern crate alloc;

/// Debugger spin gate. When `--debug` is active, the kernel spins here until
/// LLDB sets this to false: `expr -- *(bool*)&DEBUG_WAIT = false`
#[no_mangle]
#[cfg(feature = "debug-wait")]
static DEBUG_WAIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub use mm::{UserAddr, DirectMap, PHYS_OFFSET};

mod shootdown;
mod sync;
mod id_map;

mod arch;
mod drivers;

#[macro_use]
mod log;
mod actuator;
mod mm;

mod keyboard;
mod mouse;
#[cfg(feature = "boot-actuators")]
mod input_merge_test;
#[cfg(feature = "boot-actuators")]
mod usb_gate;
mod block;
mod gpt;
mod page_cache;
mod file_cache;
mod tmpfs;
mod file_backing;
mod bcachefs_adapter;
mod fat32_adapter;
#[cfg(feature = "boot-actuators")]
mod heartbeat;
mod vfs;
mod elf;
mod symbols;
mod process;
mod loader;
mod scheduler;
mod sched;
mod hw;
mod iommu;
mod preempt;
mod irq_ring;
mod trace;
mod clock;
mod rtc;

mod object;
mod io_uring;
mod pipe;

mod device;
mod net;
mod gpu;
mod user_ptr;
mod vma;

/// Where `screen_late_panic`'s panic comes from, and why it comes from here.
///
/// The renderer wraps rather than clips because the demangled symbol sits at
/// the *end* of a backtrace line, so proving wrap needs a frame whose symbol
/// is wider than the console grid — 256 columns on the 2048-px framebuffer
/// QEMU's stdvga offers, 320 at most anywhere. A generic nested in itself
/// demangles to one: ~25 columns per level, and the head and the tail of the
/// same symbol are then on different display rows. It is a real backtrace
/// frame off a real panic, which a synthetic wide `log!` line was only ever
/// standing in for.
#[cfg(feature = "boot-actuators")]
mod late_panic {
    pub struct Nest<T>(core::marker::PhantomData<T>);

    impl<T> Nest<T> {
        #[inline(never)]
        pub fn on_screen_console_check() -> ! {
            panic!("test-late-panic: on-screen console check");
        }
    }
}

use crate::mm::paging::CachePolicy;
use alloc::boxed::Box;
use arch::{apic, cpu, idt, pat, percpu, smp, syscall};
use drivers::{acpi, gop, i8042, ioapic, nvme, pci, serial, virtio_console, virtio_gpu, virtio_net, virtio_sound, xhci};
use toyos_abi::boot::{KernelArgs, MemoryMapEntry};

/// Per-CPU panic-reentry depth, indexed by x2APIC id (masked). The panic path
/// must not trust GS/percpu: a corrupted percpu block makes `swap_fault_state`
/// itself fault, re-entering the panic handler in an unbounded recursion that
/// smashes the stack down through the heap. CPUID is the only per-CPU
/// discriminator that needs no memory access and no enabled unit at all.
///
/// A single global flag was rejected: it would stay set after a *recovered*
/// panic and silently swallow every later, independent panic report, and a
/// panic on one CPU would mask a concurrent first panic on another. Masking
/// the APIC id to 64 slots only means colliding CPUs share a guard — a
/// concurrent panic on both halts the second, which halt_all_cpus would do
/// moments later anyway.
static PANIC_DEPTH: [core::sync::atomic::AtomicU32; 64] =
    [const { core::sync::atomic::AtomicU32::new(0) }; 64];

/// This CPU's APIC id, from CPUID.
///
/// It used to be `rdmsr(IA32_X2APIC_APICID)` under a comment claiming APs
/// enable x2APIC before running any panicking kernel code. They do not:
/// `apic::init_ap` is three calls after `percpu::init_ap` in `ap_entry`, and
/// that MSR is `#GP` until it has run. Every panic an AP took in between —
/// `pat::init`'s assertion, `control_regs`', the FSGSBASE one that has been
/// there all along — faulted *inside the reentry guard*, before the guard was
/// armed, and the machine triple-faulted with the whole boot still unflushed in
/// the log ring: no report, no backtrace, no serial output at all.
///
/// Leaf 0x1F and leaf 0xB give the full x2APIC id and leaf 1 the 8-bit initial
/// one; the slot is masked to 64 either way, so the fallback loses nothing this
/// array was keeping.
fn apic_id() -> u32 {
    let (max_leaf, _, _, _) = cpu::cpuid(0, 0);
    for leaf in [0x1F, 0x0B] {
        if max_leaf >= leaf {
            let (_, ebx, _, edx) = cpu::cpuid(leaf, 0);
            // Reaching the leaf index is not the existence test: SDM Vol. 2A,
            // `CPUID` leaf 0BH, requires `EBX[15:0]` non-zero as well, and a CPU
            // whose maximum leaf covers it without implementing it answers zero
            // in every register. That is not a distinguisher — every CPU would
            // take slot 0 and share one guard — and it is reached by skipping
            // the leaf-1 fallback that is correct for exactly that machine.
            if ebx & 0xFFFF != 0 {
                return edx;
            }
        }
    }
    let (_, ebx, _, _) = cpu::cpuid(1, 0);
    ebx >> 24
}

fn panic_depth_slot() -> &'static core::sync::atomic::AtomicU32 {
    &PANIC_DEPTH[apic_id() as usize & 63]
}

/// Fixed-string output for the reentry path: direct UART port I/O — no
/// locks, no log ring, no percpu, nothing that can fault or recurse.
///
/// Gated on the probe like every other UART access: on a machine with no
/// 16550 the LSR reads 0xFF, so the wait falls through immediately and each
/// byte is written to a port nothing answers.
fn panic_raw_uart(msg: &[u8]) {
    if !drivers::serial::uart_present() {
        return;
    }
    for &b in msg {
        // Bounded LSR wait so a wedged UART cannot hang the halt path.
        for _ in 0..100_000 {
            if cpu::inb(0x3FD) & 0x20 != 0 { break; }
        }
        cpu::outb(0x3F8, b);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }

    // Early boot: percpu not ready, just halt (single CPU at this point)
    if !log::PERCPU_READY.load(core::sync::atomic::Ordering::Relaxed) {
        alert!("EARLY PANIC: {}", info);
        // This branch halts directly and never reaches halt_all_cpus, so it
        // owns both halves itself — and inverts halt_all_cpus' order. It runs
        // before idt::init, the one window with no exception handlers at all,
        // where a fault inside the renderer's page walk or its full-screen
        // MMIO blit triple-faults instead of being caught. The flush goes
        // first so that costs the screen and never the serial report; the
        // capture above has already copied the ring, so what render() paints
        // afterwards is byte-identical either way.
        drivers::panic_console::capture();
        unsafe { drivers::serial::panic_flush(); }
        drivers::panic_console::render();
        cpu::halt();
    }

    // Reentry guard — checked before ANY fallible access (percpu fault
    // state, logging, unwinding). A panic inside the panic path halts this
    // CPU immediately with one raw line instead of recursing.
    let depth = panic_depth_slot();
    if depth.fetch_add(1, core::sync::atomic::Ordering::SeqCst) > 0 {
        panic_raw_uart(b"\n!!! PANIC REENTRY: CPU halted !!!\n");
        // The one fatal branch that reached no channel at all on a machine
        // with no UART. render() is safe here by construction: if the reentry
        // came from a fault inside the renderer, PAINTING is already taken and
        // this returns without touching a pixel. No capture() — the outer
        // panic's snapshot is the report worth showing, and re-peeking a ring
        // panic_flush may already have drained would replace it with nothing.
        drivers::panic_console::render();
        cpu::halt();
    }

    let prev = percpu::swap_fault_state(percpu::CpuFaultState::Panic);
    if prev != percpu::CpuFaultState::Normal {
        // Nested: Panic→Panic, Fatal→Panic, PageFault→Panic. Escalate.
        alert!("DOUBLE PANIC");
        apic::halt_all_cpus();
    }

    let rbp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp, options(nomem, nostack)); }

    arch::idt::exceptions::crash_report(
        &arch::idt::exceptions::CrashInfo::Panic { message: info, rbp }
    );

    // Drain the report now, not eventually. `crash_report` only writes into
    // the 64 KiB log ring; the drains are the idle loop and the timer tick,
    // and neither is guaranteed to run again — the recovery path below
    // re-enters a scheduler the panicking thread may have left holding a
    // lock. A wedge after that point loses the one message explaining it.
    // Draining twice is harmless (the second drain finds an empty ring).
    //
    // The on-screen console must read the report before that drain pops it,
    // and must paint only if this panic turns out to be fatal — so the copy
    // happens here and the paint happens in halt_all_cpus. A recovering panic
    // captures and never paints, which is the property, not an accident.
    drivers::panic_console::capture();
    unsafe { drivers::serial::panic_flush(); }

    // If in syscall context: kill the process, rejoin scheduler. This panic
    // is fully handled — reset the reentry guard so a future, independent
    // panic on this CPU still reports.
    //
    // **A kernel thread answers from its own row and never from the two words
    // below**, because for one of them the words do not merely give the wrong
    // answer — they give a *nondeterministic* one. `syscall_rip` is never
    // cleared (`specs/issues/panic-path/syscall-rip-never-cleared.md`), so a
    // kernel task reads whatever user thread last ran on this CPU left behind:
    // the same panic on the same build would recover or halt depending on which
    // CPU work stealing had put the thread on. `sched::kthread` is where the
    // answer is a property of the thread instead.
    let recoverable = sched::kthread::panic_recovers_here()
        .unwrap_or_else(|| percpu::syscall_rip() != 0 && percpu::current_tid().is_some());
    if recoverable {
        depth.store(0, core::sync::atomic::Ordering::SeqCst);
        // The captured report dies with the panic it belongs to. Left set, it
        // outlives a panic the machine survived, and the next fatal path —
        // a #GP an hour later — paints that one as the cause of death.
        drivers::panic_console::discard_capture();
        arch::idt::exceptions::try_recover_from_panic();
    }

    apic::halt_all_cpus();
}

/// Kernel entry point. Called by bootloader with rdi = &KernelArgs.
/// Switches to the kernel's own stack, then falls through to init.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "sysv64" fn _start(_kernel_args: &KernelArgs) -> ! {
    // rdi = &KernelArgs (preserved — not clobbered by stack setup)
    // Stack top = PHYS_OFFSET + kernel_memory_addr + kernel_stack_addr + kernel_stack_size
    core::arch::naked_asm!(
        "mov rax, [rdi + 16]",  // kernel_memory_addr
        "add rax, [rdi + 32]",  // + kernel_stack_addr
        "add rax, [rdi + 40]",  // + kernel_stack_size
        "movabs rbx, {phys_offset}",
        "add rax, rbx",
        "mov rsp, rax",
        "call {kernel_main}",
        phys_offset = const PHYS_OFFSET,
        kernel_main = sym kernel_main,
    );
}

fn register_gpu(driver: Box<dyn gpu::Gpu>, info: gpu::GpuInfo) {
    crate::device::set_framebuffer_info(crate::device::Screen {
        // A description carries handles into whichever process reads it, and
        // that process does not exist yet: `try_claim` mints them.
        info: toyos_abi::FramebufferInfo {
            scanout: [toyos_abi::HANDLE_INVALID; 2],
            cursor: toyos_abi::HANDLE_INVALID,
            width: info.width,
            height: info.height,
            stride: info.stride,
            pixel_format: info.pixel_format,
            flags: info.flags,
        },
        scanout: info.scanout.clone(),
        cursor: info.cursor.clone(),
    });
    gpu::register(driver, info);
}

/// Say where this boot's log can be read, on the last surface that still shows
/// it.
///
/// The final boot checkpoint paints the tail of the records, so a line logged
/// immediately before it is on the panel until userland claims the screen. On
/// the machine this exists for that panel is the only thing a person can be
/// told anything on — and what they most need to be told is that there will be
/// nothing to read afterwards.
///
/// **The four-way table survives L6 and its second axis changed, which is not
/// the same as losing it** (§5.6). It was `(console, the file logd opened)`, and
/// the kernel does not open a file any more — it does not name one and cannot
/// say whether logd got anywhere. What it *does* still know is whether the log
/// **volume mounted**, which is the fact this line exists to carry: a machine
/// with no `/log` partition leaves no account of itself once userland owns the
/// screen, and that is the sentence the owner needs on the panel.
///
/// **It has to be the kernel's, because the panel is the kernel's.**
/// `panic_console` paints records, so a userland line reaches a console and
/// never the screen — and this line's whole audience is somebody looking at a
/// T14 with no serial port. `/bin/logd` says the half only it knows, on its own
/// console handle: which file, or that it could not open one.
///
/// `alert!` is what says the row is red, and it is used for the two states in
/// which this boot leaves no readable account of itself anywhere. Nothing in
/// the text says so: the panel reads `Level` off the record, so a refusal wears
/// the colour without having to spell it.
///
/// ASCII throughout, unlike the rest of the kernel's prose: the panel's font is
/// codepoints 0x20..=0x7E and `draw_glyph` renders everything else as a dot, so
/// an em dash reaches the one reader this line has as three of them.
fn report_log_destination() {
    let has_log = vfs::lock().has_mount(fat32_adapter::Role::Log.mount());
    match (drivers::serial::has_console(), has_log) {
        (true, true) => log!("log: this boot is on the console and on /log"),
        (false, true) => log!("log: no serial console - this boot is on /log and on the screen"),
        (true, false) => {
            alert!("log: no /log - this boot is on the console only, and nothing outlives the power")
        }
        (false, false) => {
            alert!("log: no serial console and no /log - this boot is on this screen and nowhere else")
        }
    }
}

unsafe fn kernel_main(kernel_args: &KernelArgs) -> ! {
    // Copy KernelArgs to the kernel stack — the original lives on the UEFI stack
    // which becomes inaccessible after mm::init drops the identity map.
    let kernel_args = *kernel_args;

    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.memory_map_addr).as_ptr::<MemoryMapEntry>(),
        entry_count,
    );

    // Before serial::init, because serial::init is itself a place the kernel
    // can die on unfamiliar hardware and the screen may be the only channel.
    // Nothing is mapped yet beyond the bootloader's identity+high map, which
    // is exactly what a sub-4 GiB firmware framebuffer needs.
    drivers::panic_console::arm(&kernel_args, maps);

    serial::init();

    // After the two channels a refusal can be read on, and before every
    // actuator site — the earliest of them is a dozen lines below.
    //
    // The length is asked first because an empty `Vec` in the bootloader has no
    // allocation behind it to point at, and every image anyone ships carries an
    // empty one.
    actuator::init(if kernel_args.cmdline_len == 0 {
        ""
    } else {
        core::str::from_utf8(core::slice::from_raw_parts(
            DirectMap::from_phys(kernel_args.cmdline_addr).as_ptr::<u8>(),
            kernel_args.cmdline_len as usize,
        ))
        .expect("the boot parameter is not UTF-8")
    });

    // Before `pat::init`, which restores the CR0 it found around its own
    // no-fill window and would carry a firmware CD straight through it.
    arch::control_regs::init_cr0(0);

    // Before `panic_console::remap` and `mm::init`, which are the first things
    // to map a page selecting the entry it writes.
    pat::init();
    log!("PAT: IA32_PAT={:#018x}, entry {} = {}",
        pat::msr(), pat::WC_ENTRY, pat::entry_name(pat::WC_ENTRY));

    // The window this exists to cover: percpu is not up, no allocator, no
    // paging of our own, so the early-panic branch is the whole reporting
    // mechanism.
    if actuator::test_early_panic() {
        panic!("test-early-panic: on-screen console check");
    }

    #[cfg(feature = "debug-wait")]
    {
        log!("debug: waiting for debugger — set DEBUG_WAIT=false to continue");
        while DEBUG_WAIT.load(core::sync::atomic::Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }

    // **Six records rather than one `{:?}`.** The derived debug of `KernelArgs`
    // is the one call site in the tree whose message exceeds the record bound —
    // everything above 200 characters in the measured corpus is this line, 18
    // of 12,497 — and unlike a demangled symbol it is a producer the kernel can
    // split. So it is split, grouped by the question each field answers, rather
    // than truncated with a count of what was lost
    // (`specs/log-architecture-spec.md` §2.1).
    log!(
        "boot: memory map {:#x}+{:#x}, kernel {:#x}+{:#x}, stack {:#x}+{:#x}",
        kernel_args.memory_map_addr, kernel_args.memory_map_size,
        kernel_args.kernel_memory_addr, kernel_args.kernel_memory_size,
        kernel_args.kernel_stack_addr, kernel_args.kernel_stack_size
    );
    log!(
        "boot: initrd {:#x}+{:#x}, kernel elf {:#x}+{:#x}, rsdp {:#x}, boot pml4 {:#x}",
        kernel_args.initrd_addr, kernel_args.initrd_size,
        kernel_args.kernel_elf_addr, kernel_args.kernel_elf_size,
        kernel_args.rsdp_addr, kernel_args.boot_pml4_addr
    );
    log!(
        "boot: gop {:#x}+{:#x} {}x{} stride {} format {}",
        kernel_args.gop_framebuffer, kernel_args.gop_framebuffer_size,
        kernel_args.gop_width, kernel_args.gop_height,
        kernel_args.gop_stride, kernel_args.gop_pixel_format
    );
    log!(
        "boot: boot partition present={} lba {} +{} blocks guid {:02x?}",
        kernel_args.boot_partition_present, kernel_args.boot_partition_start_lba,
        kernel_args.boot_partition_blocks, kernel_args.boot_partition_guid
    );
    log!("boot: log partition guid {:02x?}", kernel_args.log_partition_guid);
    log!(
        "boot: rtc utc offset {} minutes (known={}), cmdline {:#x}+{}",
        kernel_args.rtc_utc_offset_minutes, kernel_args.rtc_utc_offset_known,
        kernel_args.cmdline_addr, kernel_args.cmdline_len
    );

    let initrd = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.initrd_addr).as_ptr::<u8>(),
        kernel_args.initrd_size as usize,
    );
    let kernel_elf = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.kernel_elf_addr).as_ptr::<u8>(),
        kernel_args.kernel_elf_size as usize,
    );
    let kernel_args = &kernel_args;

    // Phase 1: Memory
    let reserved = [
        mm::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        mm::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        mm::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
        mm::Region { start: kernel_args.kernel_stack_addr, end: kernel_args.kernel_stack_addr + kernel_args.kernel_stack_size },
        mm::Region { start: 0x8000, end: 0x9000 }, // AP trampoline page
    ];

    mm::init(maps, &reserved);
    drivers::panic_console::remap();

    // Phase 2: CPU — exceptions, LAPIC, clock
    // Get exception handlers up ASAP so bugs in later phases produce diagnostics
    // instead of triple-faulting.
    let madt = acpi::parse_madt(kernel_args.rsdp_addr).expect("ACPI: MADT not found");
    apic::init();
    percpu::init_bsp(apic::id());
    idt::init();
    ioapic::init(&madt);
    idt::enable_interrupts();
    syscall::init();
    symbols::set_kernel_base(kernel_args.kernel_memory_addr);
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, mm::PHYS_OFFSET + kernel_args.kernel_memory_addr);
    }

    // HPET clock — enables profiling for everything from here on
    let hpet_base = acpi::find_hpet_base(kernel_args.rsdp_addr)
        .expect("ACPI: HPET not found");
    clock::init(hpet_base);
    // Straight after the monotonic clock it is anchored to, and before anything
    // that stamps a file or serves a clock syscall. Two questions the RTC's own
    // registers cannot answer come from elsewhere: the FADT says where the
    // century digit is, and firmware said what zone the thing keeps.
    let century_reg = match acpi::rtc_century_register(kernel_args.rsdp_addr) {
        Ok(reg) => reg,
        Err(e) => {
            log!("ACPI: the FADT is unreadable ({e:?}), so where the RTC keeps its century is unknown too");
            None
        }
    };
    clock::init_wall(century_reg, kernel_args.rtc_utc_offset());
    trace::enable();
    apic::init_timer();

    boot_phase!("CPU ready", 0);

    // Phase 3: Storage
    let t_storage = clock::nanos_since_boot();

    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    let ecam = mm::paging::map_mmio(ecam_base, 256 * 32 * 8 * 4096, CachePolicy::DeferToMtrr);
    let pci_devices = pci::enumerate(&ecam);
    // After ACPI is readable and PCI is enumerable, before any driver `init`:
    // the unit has to be programmed before the first device is told to do DMA
    // (`specs/iommu-spec.md` §2.1), and every function the walk above returned
    // has to have a context entry before translation comes on. Refuses nothing
    // — a machine with no usable unit boots exactly as it does without one.
    iommu::init(kernel_args.rsdp_addr, &pci_devices);
    file_cache::init();
    gpt::init(kernel_args);

    // No controller is a configuration, not a failure — the same call this
    // kernel already makes for a missing xHCI, a missing NIC and a missing
    // audio device. The bootloader reads the whole initrd through UEFI before
    // ExitBootServices, so a machine can boot off a USB stick with no NVMe at
    // all, and one where the controller sits behind a firmware setting we have
    // not touched looks identical. `.expect` here killed both, at 0.08 s, on a
    // machine whose only output channel is a screen that says nothing useful
    // yet.
    //
    // `None` from `open_home` is the other half and means something different:
    // there *is* a disk and it is not ours to write to. Both land on a tmpfs.
    let home_volume = match nvme::init(&pci_devices) {
        Some(mut nvme_dev) => {
            // Before the page cache takes the device: this is the one place
            // that has it in the device's own logical blocks, and asking a
            // disk where our boot partition is has to happen whether or not
            // anything on it turns out to be ours.
            let sector_size = nvme_dev.sector_size();
            gpt::probe(&mut nvme_dev, sector_size);
            page_cache::init(Box::new(nvme_dev));
            bcachefs_adapter::open_home()
        }
        None => {
            log!("NVMe: no controller on this machine, storage unavailable");
            None
        }
    };

    boot_phase!("storage ready", t_storage);

    // **Four phases before the idle loop, which is where the log used to become
    // sayable.** Under `Drain::Inline` every record above is already on the wire
    // when this runs, so what the gate reads is the whole boot and then silence
    // — where before this branch it was silence and nothing else.
    #[cfg(feature = "boot-actuators")]
    if actuator::pre_idle_wedge() {
        pre_idle_wedge();
    }

    // Phase 4: Peripherals
    let t_periph = clock::nanos_since_boot();

    xhci::init(&pci_devices);
    #[cfg(feature = "boot-actuators")]
    if actuator::usb_storage_gate() {
        usb_gate::run();
    }
    // Here rather than beside the NVMe probe: this machine boots off a USB
    // stick, so the disk carrying the boot partition does not exist until the
    // controller above has bound it.
    fat32_adapter::probe_boot_disks();
    i8042::init(kernel_args.rsdp_addr);
    acpi::init_power(kernel_args.rsdp_addr);

    boot_phase!("peripherals ready", t_periph);

    // Phase 5: Kernel subsystems
    let t_subsys = clock::nanos_since_boot();

    smp::boot_aps(&madt, kernel_args.boot_pml4_addr);
    vfs::init();
    process::init();
    scheduler::init();
    pipe::init();
    io_uring::init();


    // Mount initrd as read-only root filesystem (bcachefs, no extraction)
    assert!(!initrd.is_empty(), "No initrd provided");
    let initrd_base = initrd.as_ptr();
    let initrd_fs = bcachefs_adapter::mount_initrd(initrd_base, initrd.len());
    vfs::lock().set_root(Box::new(bcachefs_adapter::ReadOnlyBcacheFsAdapter::new(initrd_fs, initrd_base)));

    // NVMe bcachefs at /home when the device is ours, a tmpfs when it is not,
    // so a machine we may not write to still boots to a working system. The
    // difference is persistence and nothing else, which is what keeps the
    // refusal from turning into a second failure mode further up.
    use vfs::UserAccess;
    match home_volume {
        Some(fs) => vfs::lock().mount("home", Box::new(bcachefs_adapter::BcacheFsAdapter::new(fs)), UserAccess::ReadWrite),
        None => {
            log!("storage: /home is a tmpfs — it will not survive a reboot");
            vfs::lock().mount("home", Box::new(crate::tmpfs::TmpFs::new()), UserAccess::ReadWrite)
        }
    }
    vfs::lock().mount("tmp", Box::new(crate::tmpfs::TmpFs::new()), UserAccess::ReadWrite);

    // The two partitions the handoff named, each under the name of its role
    // rather than of its type: `/esp` would say what the format is, and
    // selecting a volume by what it looks like is the mistake `gpt` exists to
    // make unrepresentable — both of these are FAT32 and neither is chosen for
    // being FAT32. A machine that cannot identify one of them simply does not
    // have that mount, and boots exactly as it did before.
    use fat32_adapter::Role;
    //
    // The boot volume is `KernelOnly`: firmware and the bootloader read the
    // machine out of it, so a process that can write it can make the machine
    // unbootable. The log volume is not — it is a diagnostic partition whose
    // worst loss is the diagnostic, and `toybox` writes to it. That the log
    // file itself is unprotected is the residual; see
    // `specs/issues/boot-media/log-is-userland-writable.md`.
    match fat32_adapter::mount(Role::Boot) {
        Some(fs) => vfs::lock().mount(Role::Boot.mount(), Box::new(fs), UserAccess::KernelOnly),
        None => log!("boot-volume: not mounted; the kernel has no /boot this boot"),
    }
    match fat32_adapter::mount(Role::Log) {
        Some(fs) => {
            vfs::lock().mount(Role::Log.mount(), Box::new(fs), UserAccess::ReadWrite);
        }
        // A refusal `gpt:` has already named the missing GUID for, and never a
        // fallback onto `/boot`: a stick with no log partition keeps its log in
        // the shards, where the screen and the console can still reach it.
        None => log!("log-volume: not mounted; this boot's kernel log stays in memory"),
    }

    // Kernel string literals, not untrusted input: these are orders of
    // magnitude under `MAX_PATH`, so a refusal here is a kernel bug and gets
    // fail-fast rather than the error return `sys_mkdir` hands userland.
    vfs::lock().create_dir("/home/root").expect("boot: /home/root exceeds MAX_PATH");
    vfs::lock().create_dir("/home/root/.config").expect("boot: /home/root/.config exceeds MAX_PATH");

    boot_phase!("subsystems ready", t_subsys);

    // Phase 6: Devices
    let t_devices = clock::nanos_since_boot();

    // Once for the machine, before any device is brought up: it touches no
    // device and reads no register, so a run per virtio driver would say the
    // same thing four times.
    #[cfg(feature = "boot-actuators")]
    if actuator::virtio_used_selftest() {
        drivers::virtio::used_selftest();
    }

    virtio_console::init(&pci_devices);
    virtio_net::init(&pci_devices);

    virtio_sound::init(&pci_devices);
    drivers::hda::init(&pci_devices);

    if let Some((gpu_driver, gpu_info)) = virtio_gpu::init(&pci_devices) {
        log!("GPU: using VirtIO");
        // virtio's scanout is only reachable through a virtqueue round trip
        // behind GPU.lock(), which the panic path may not take.
        drivers::panic_console::disable();
        register_gpu(gpu_driver, gpu_info);
    } else if kernel_args.gop_framebuffer != 0 {
        log!("GPU: using UEFI GOP");
        let (gpu_driver, gpu_info) = gop::init(
            kernel_args.gop_framebuffer,
            kernel_args.gop_framebuffer_size,
            kernel_args.gop_width,
            kernel_args.gop_height,
            kernel_args.gop_stride,
            kernel_args.gop_pixel_format,
        );
        register_gpu(gpu_driver, gpu_info);
    } else {
        log!("GPU: none found, running headless");
    };

    boot_phase!("devices ready", t_devices);

    // Before userland, so nothing else is reading the input queues.
    #[cfg(feature = "boot-actuators")]
    if actuator::test_input_merge() {
        input_merge_test::run();
    }

    // Phase 7: Userland. One program, and it is not a choice the boot config
    // makes any more: init reads `/etc/system.manifest` and starts what that
    // says. What the bootloader used to carry — a `;`-joined argv blob baked
    // into its own binary — made the `.efi` a function of the boot config, and
    // a concurrent build could hand an image another config's init string.
    let pid = process::spawn_init();
    log!("spawned {} pid={pid}", process::INIT_PATH);

    report_log_destination();
    boot_phase!("complete", 0);

    // The panic no userland process can produce, by design: nothing is
    // current here, so the handler's recovery predicate fails and it runs the
    // ordinary fatal path — crash_report, capture, drain, halt, paint.
    //
    // It used to say the drain empties the ring before the paint, "which makes
    // this the one test that fails if the capture stops happening". That is no
    // longer true and was measured false: a drain no longer erases what the
    // console reads, so this test passes with `capture` stubbed out. See the
    // note on `panic_console::capture` for what still justifies it.
    #[cfg(feature = "boot-actuators")]
    if actuator::test_late_panic() {
        late_panic::Nest::<late_panic::Nest<late_panic::Nest<late_panic::Nest<
            late_panic::Nest<late_panic::Nest<late_panic::Nest<late_panic::Nest<
            late_panic::Nest<late_panic::Nest<()>>>>>>>>>>::on_screen_console_check();
    }

    // The last thing before the machine hands itself to the scheduler, because
    // that is the first moment anything can run: the APs spin on `SMP_READY`
    // below and the BSP reaches no pass before `enter_idle_loop`. A `klogd`
    // spawned earlier would sit in a run queue through phases 5, 6 and 7 —
    // which is the window a machine with no console wedges in — while the boot
    // believed it had a drainer. `specs/log-architecture-spec.md` §4.2.
    log::console::start();

    smp::set_ready();
    crate::scheduler::enter_idle_loop();
}

/// Stop this machine where nothing can report it, and say so first.
///
/// **Interrupts off and then a spin, which is a wedge rather than a machine
/// that is merely idle.** No timer tick, no scheduler pass, no idle loop: the
/// two things that used to drain the byte ring are both unreachable from here,
/// and so is `klogd`, which is not spawned for another four phases. Everything
/// the boot has said is therefore already on the wire or it never will be —
/// which is the whole of what `pre_idle_wedge_speaks` reads.
#[cfg(feature = "boot-actuators")]
fn pre_idle_wedge() -> ! {
    log!("pre-idle-wedge: the boot stops here, and this line is the last thing this machine says");
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    loop {
        core::hint::spin_loop();
    }
}
