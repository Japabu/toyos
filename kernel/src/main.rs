#![no_std]
#![no_main]
#![allow(dead_code)]
extern crate alloc;

/// Debugger spin gate. When `--debug` is active, the kernel spins here until
/// LLDB sets this to false: `expr -- *(bool*)&DEBUG_WAIT = false`
#[no_mangle]
#[cfg(feature = "debug-wait")]
static DEBUG_WAIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub use mm::{UserAddr, DirectMap, PHYS_OFFSET};

mod sync;
mod id_map;

mod arch;
mod drivers;

#[macro_use]
mod log;
mod mm;

mod keyboard;
mod mouse;
#[cfg(feature = "test-input-merge")]
mod input_merge_test;
#[cfg(feature = "usb-storage-gate")]
mod usb_gate;
mod block;
mod gpt;
mod page_cache;
mod file_cache;
mod tmpfs;
mod file_backing;
mod bcachefs_adapter;
mod fat32_adapter;
#[allow(dead_code)]
mod vfs;
mod elf;
mod symbols;
mod process;
mod loader;
mod scheduler;
mod sched;
mod hw;
mod preempt;
mod irq_ring;
mod trace;
mod clock;
mod rtc;
mod fd;
mod io_uring;
mod pipe;
mod listener;
mod device;
mod net;
mod gpu;
mod audio;
mod shared_memory;
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
#[cfg(feature = "test-late-panic")]
mod late_panic {
    pub struct Nest<T>(core::marker::PhantomData<T>);

    impl<T> Nest<T> {
        #[inline(never)]
        pub fn on_screen_console_check() -> ! {
            panic!("test-late-panic: on-screen console check");
        }
    }
}

use alloc::boxed::Box;
use alloc::vec::Vec;
use arch::{apic, cpu, idt, percpu, smp, syscall};
use drivers::{acpi, gop, i8042, ioapic, nvme, pci, serial, virtio_console, virtio_gpu, virtio_net, virtio_sound, xhci};
use toyos_abi::boot::{KernelArgs, MemoryMapEntry};

/// Per-CPU panic-reentry depth, indexed by x2APIC id (masked). The panic path
/// must not trust GS/percpu: a corrupted percpu block makes `swap_fault_state`
/// itself fault, re-entering the panic handler in an unbounded recursion that
/// smashes the stack down through the heap. `rdmsr(IA32_X2APIC_APICID)` is the
/// only per-CPU discriminator that needs no memory access at all.
///
/// A single global flag was rejected: it would stay set after a *recovered*
/// panic and silently swallow every later, independent panic report, and a
/// panic on one CPU would mask a concurrent first panic on another. Masking
/// the APIC id to 64 slots only means colliding CPUs share a guard — a
/// concurrent panic on both halts the second, which halt_all_cpus would do
/// moments later anyway.
static PANIC_DEPTH: [core::sync::atomic::AtomicU32; 64] =
    [const { core::sync::atomic::AtomicU32::new(0) }; 64];

const IA32_X2APIC_APICID: u32 = 0x802;

fn panic_depth_slot() -> &'static core::sync::atomic::AtomicU32 {
    // Safe once PERCPU_READY: apic::init (x2APIC enable) precedes it on the
    // BSP, and APs enable x2APIC before running any panicking kernel code.
    let id = cpu::rdmsr(IA32_X2APIC_APICID) as usize;
    &PANIC_DEPTH[id & 63]
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
        log!("!!! EARLY PANIC !!!: {}", info);
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
        log!("!!! DOUBLE PANIC !!!");
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
    if percpu::syscall_rip() != 0 && percpu::current_tid().is_some() {
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
    let fb_info = fd::FramebufferInfo {
        token: [info.tokens[0].raw(), info.tokens[1].raw()],
        cursor_token: info.cursor_token.raw(),
        width: info.width,
        height: info.height,
        stride: info.stride,
        pixel_format: info.pixel_format,
        flags: info.flags,
    };
    crate::device::set_framebuffer_info(fb_info);
    gpu::register(driver, info);
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

    // The window this exists to cover: percpu is not up, no allocator, no
    // paging of our own, so the early-panic branch is the whole reporting
    // mechanism. black_box keeps the rest of kernel_main reachable to the
    // compiler; a bare `panic!` would make every later line dead code.
    #[cfg(feature = "test-early-panic")]
    if core::hint::black_box(true) {
        panic!("test-early-panic: on-screen console check");
    }

    #[cfg(feature = "debug-wait")]
    {
        log!("debug: waiting for debugger — set DEBUG_WAIT=false to continue");
        while DEBUG_WAIT.load(core::sync::atomic::Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }

    log!("{:?}", kernel_args);

    let initrd = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.initrd_addr).as_ptr::<u8>(),
        kernel_args.initrd_size as usize,
    );
    let kernel_elf = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.kernel_elf_addr).as_ptr::<u8>(),
        kernel_args.kernel_elf_size as usize,
    );
    let init_bytes = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.init_program_addr).as_ptr::<u8>(),
        kernel_args.init_program_len as usize,
    );
    let init_programs = core::str::from_utf8(init_bytes).expect("init_programs: invalid UTF-8");
    let kernel_args = &kernel_args;

    // Phase 1: Memory
    let reserved = [
        mm::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        mm::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        mm::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
        mm::Region { start: kernel_args.kernel_stack_addr, end: kernel_args.kernel_stack_addr + kernel_args.kernel_stack_size },
        mm::Region { start: 0x8000, end: 0x9000 }, // AP trampoline page
    ];

    // Copy init_programs into heap before mm::init reclaims bootloader memory.
    mm::init(maps, &reserved);
    drivers::panic_console::remap();
    let init_programs = alloc::string::String::from(init_programs);
    let init_programs: &str = &init_programs;

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
    trace::enable();
    apic::init_timer();

    boot_phase!("CPU ready", 0);

    // Phase 3: Storage
    let t_storage = clock::nanos_since_boot();

    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    let ecam = mm::paging::kernel().lock().as_mut().unwrap().map_mmio(ecam_base, 256 * 32 * 8 * 4096);
    pci::enumerate(&ecam);
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
    let home_volume = match nvme::init(&ecam) {
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

    // Phase 4: Peripherals
    let t_periph = clock::nanos_since_boot();

    match xhci::init(&ecam) {
        Some(ctrl) => xhci::set_global(ctrl),
        None => log!("xHCI: no controller on this machine, USB input unavailable"),
    }
    #[cfg(feature = "usb-storage-gate")]
    usb_gate::run();
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
    listener::init();
    shared_memory::init();

    // Mount initrd as read-only root filesystem (bcachefs, no extraction)
    assert!(!initrd.is_empty(), "No initrd provided");
    let initrd_base = initrd.as_ptr();
    let initrd_fs = bcachefs_adapter::mount_initrd(initrd_base, initrd.len());
    vfs::lock().set_root(Box::new(bcachefs_adapter::ReadOnlyBcacheFsAdapter::new(initrd_fs, initrd_base)));

    // NVMe bcachefs at /home when the device is ours, a tmpfs when it is not,
    // so a machine we may not write to still boots to a working system. The
    // difference is persistence and nothing else, which is what keeps the
    // refusal from turning into a second failure mode further up.
    match home_volume {
        Some(fs) => vfs::lock().mount("home", Box::new(bcachefs_adapter::BcacheFsAdapter::new(fs))),
        None => {
            log!("storage: /home is a tmpfs — it will not survive a reboot");
            vfs::lock().mount("home", Box::new(crate::tmpfs::TmpFs::new()))
        }
    }
    vfs::lock().mount("tmp", Box::new(crate::tmpfs::TmpFs::new()));

    // The partition firmware loaded us from, under the name of its role rather
    // than of its type: `/esp` would say what the format is, and selecting a
    // volume by what it looks like is the mistake `gpt` exists to make
    // unrepresentable. A machine that cannot identify its boot partition has
    // no `/boot` and boots exactly as it did before.
    match fat32_adapter::mount_boot() {
        Some(fs) => vfs::lock().mount("boot", Box::new(fs)),
        None => log!("esp: no boot volume — the kernel has no /boot this boot"),
    }

    // Kernel string literals, not untrusted input: these are orders of
    // magnitude under `MAX_PATH`, so a refusal here is a kernel bug and gets
    // fail-fast rather than the error return `sys_mkdir` hands userland.
    vfs::lock().create_dir("/home/root").expect("boot: /home/root exceeds MAX_PATH");
    vfs::lock().create_dir("/home/root/.config").expect("boot: /home/root/.config exceeds MAX_PATH");

    boot_phase!("subsystems ready", t_subsys);

    // Phase 6: Devices
    let t_devices = clock::nanos_since_boot();

    virtio_console::init(&ecam);
    virtio_net::init(&ecam);

    if let Some((sound, audio_info)) = virtio_sound::init(&ecam) {
        crate::audio::register(sound, audio_info);
    }

    if let Some((gpu_driver, gpu_info)) = virtio_gpu::init(&ecam) {
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
    #[cfg(feature = "test-input-merge")]
    input_merge_test::run();

    // Phase 7: Userland
    assert!(!init_programs.is_empty(), "bootloader must provide init_programs");
    for entry in init_programs.split(';') {
        let args: Vec<&str> = entry.split_whitespace().collect();
        assert!(!args.is_empty(), "empty entry in init_programs");
        let pid = process::spawn_kernel(&args);
        log!("spawned {} pid={pid}", args[0]);
    }

    boot_phase!("complete", 0);
    log!("Keyboard layout: {}", crate::keyboard::layout_name());

    // The panic no userland process can produce, by design: nothing is
    // current here, so the handler's recovery predicate fails and it runs the
    // ordinary fatal path — crash_report, capture, drain, halt, paint.
    //
    // It used to say the drain empties the ring before the paint, "which makes
    // this the one test that fails if the capture stops happening". That is no
    // longer true and was measured false: a drain no longer erases what the
    // console reads, so this test passes with `capture` stubbed out. See the
    // note on `panic_console::capture` for what still justifies it.
    #[cfg(feature = "test-late-panic")]
    if core::hint::black_box(true) {
        late_panic::Nest::<late_panic::Nest<late_panic::Nest<late_panic::Nest<
            late_panic::Nest<late_panic::Nest<late_panic::Nest<late_panic::Nest<
            late_panic::Nest<late_panic::Nest<()>>>>>>>>>>::on_screen_console_check();
    }

    smp::set_ready();
    crate::scheduler::enter_idle_loop();
}
