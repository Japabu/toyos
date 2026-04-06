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
mod block;
#[allow(dead_code)]
mod page_cache;
mod file_cache;
mod tmpfs;
mod file_backing;
mod bcachefs_adapter;
#[allow(dead_code)]
mod vfs;
mod elf;
mod symbols;
mod process;
mod loader;
mod scheduler;
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

use alloc::boxed::Box;
use alloc::vec::Vec;
use arch::{apic, cpu, idt, percpu, smp, syscall};
use drivers::{acpi, gop, nvme, pci, serial, virtio_gpu, virtio_net, virtio_sound, xhci};
use toyos_abi::boot::{KernelArgs, MemoryMapEntry};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    unsafe { core::arch::asm!("cli", options(nomem, nostack)); }

    // Early boot: percpu not ready, just halt (single CPU at this point)
    if !log::PERCPU_READY.load(core::sync::atomic::Ordering::Relaxed) {
        log!("!!! EARLY PANIC !!!: {}", info);
        cpu::halt();
    }

    // Per-CPU fault state transition
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

    // If in syscall context: kill the process, rejoin scheduler
    if percpu::syscall_rip() != 0 && percpu::current_tid().is_some() {
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
    syscall::set_screen_size(fb_info.width, fb_info.height);
    crate::device::set_framebuffer_info(fb_info);
    gpu::register(driver, info);
}

unsafe fn kernel_main(kernel_args: &KernelArgs) -> ! {
    // Copy KernelArgs to the kernel stack — the original lives on the UEFI stack
    // which becomes inaccessible after mm::init drops the identity map.
    let kernel_args = *kernel_args;

    serial::init();

    #[cfg(feature = "debug-wait")]
    {
        log!("debug: waiting for debugger — set DEBUG_WAIT=false to continue");
        while DEBUG_WAIT.load(core::sync::atomic::Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }

    log!("{:?}", kernel_args);

    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        DirectMap::from_phys(kernel_args.memory_map_addr).as_ptr::<MemoryMapEntry>(),
        entry_count,
    );
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

    // ── Phase 1: Memory ─────────────────────────────────────────────────
    let reserved = [
        mm::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        mm::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        mm::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
        mm::Region { start: kernel_args.kernel_stack_addr, end: kernel_args.kernel_stack_addr + kernel_args.kernel_stack_size },
        mm::Region { start: 0x8000, end: 0x9000 }, // AP trampoline page
    ];

    // Copy init_programs into heap before mm::init reclaims bootloader memory.
    mm::init(maps, &reserved);
    let init_programs = alloc::string::String::from(init_programs);
    let init_programs: &str = &init_programs;

    // ── Phase 2: CPU — exceptions, LAPIC, clock ─────────────────────────
    // Get exception handlers up ASAP so bugs in later phases produce diagnostics
    // instead of triple-faulting.
    let madt = acpi::parse_madt(kernel_args.rsdp_addr).expect("ACPI: MADT not found");
    apic::init();
    percpu::init_bsp(apic::id());
    idt::init();
    syscall::init();
    symbols::set_kernel_base(kernel_args.kernel_memory_addr);
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, mm::PHYS_OFFSET + kernel_args.kernel_memory_addr);
    }

    // HPET clock — enables profiling for everything from here on
    let hpet_base = acpi::find_hpet_base(kernel_args.rsdp_addr)
        .expect("ACPI: HPET not found");
    clock::init(hpet_base);
    apic::init_timer();

    log!("Boot: CPU ready ({}ms)", clock::nanos_since_boot() / 1_000_000);

    // ── Phase 3: Storage ────────────────────────────────────────────────
    let t_storage = clock::nanos_since_boot();

    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    let ecam = mm::paging::kernel().lock().as_mut().unwrap().map_mmio(ecam_base, 256 * 32 * 8 * 4096);
    pci::enumerate(&ecam);
    let nvme_dev = nvme::init(&ecam).expect("NVMe: no controller found");
    page_cache::init(Box::new(nvme_dev));

    let bcachefs_instance = match bcachefs_adapter::mount() {
        Some(fs) => fs,
        None => bcachefs_adapter::format(),
    };

    log!("Boot: storage ready ({}ms)", (clock::nanos_since_boot() - t_storage) / 1_000_000);

    // ── Phase 4: Peripherals ────────────────────────────────────────────
    let t_periph = clock::nanos_since_boot();

    let xhci_ctrl = xhci::init(&ecam).expect("xHCI: no USB controller found");
    xhci::set_global(xhci_ctrl);
    acpi::init_power(kernel_args.rsdp_addr);

    log!("Boot: peripherals ready ({}ms)", (clock::nanos_since_boot() - t_periph) / 1_000_000);

    // ── Phase 5: Kernel subsystems ──────────────────────────────────────
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

    // Mount NVMe bcachefs at /home for persistent user data
    vfs::lock().mount("home", Box::new(bcachefs_adapter::BcacheFsAdapter::new(bcachefs_instance)));
    vfs::lock().mount("tmp", Box::new(crate::tmpfs::TmpFs::new()));

    // Ensure home directories exist on NVMe
    vfs::lock().create_dir("/home/root");
    vfs::lock().create_dir("/home/root/.config");

    log!("Boot: subsystems ready ({}ms)", (clock::nanos_since_boot() - t_subsys) / 1_000_000);

    // ── Phase 6: Devices ────────────────────────────────────────────────
    let t_devices = clock::nanos_since_boot();

    virtio_net::init(&ecam);

    if let Some((sound, audio_info)) = virtio_sound::init(&ecam) {
        crate::audio::register(sound, audio_info);
    }

    // Initialize GPU: try VirtIO first, fall back to UEFI GOP
    if let Some((gpu_driver, gpu_info)) = virtio_gpu::init(&ecam) {
        log!("GPU: using VirtIO");
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

    log!("Boot: devices ready ({}ms)", (clock::nanos_since_boot() - t_devices) / 1_000_000);

    // ── Phase 7: Userland ───────────────────────────────────────────────
    assert!(!init_programs.is_empty(), "bootloader must provide init_programs");
    for entry in init_programs.split(';') {
        let args: Vec<&str> = entry.split_whitespace().collect();
        assert!(!args.is_empty(), "empty entry in init_programs");
        let pid = process::spawn_kernel(&args);
        log!("spawned {} pid={pid}", args[0]);
    }

    log!("Boot: complete ({}ms total)", clock::nanos_since_boot() / 1_000_000);
    log!("Keyboard layout: {}", crate::keyboard::layout_name());

    smp::set_ready();
    crate::scheduler::schedule_no_return();
}
