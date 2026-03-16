#![no_std]
#![no_main]
extern crate alloc;

mod addr;
pub use addr::{PhysAddr, VirtAddr, UserAddr, PHYS_OFFSET};

mod sync;
mod id_map;

mod arch;
mod drivers;

#[macro_use]
mod log;
mod pmm;
mod allocator;

mod keyboard;
mod mouse;
mod block;
mod page_cache;
mod ramdisk;
mod tmpfs;
mod toyfs;
mod vfs;
mod elf;
mod symbols;
mod process;
mod scheduler;
mod clock;
mod rtc;
mod fd;
mod pipe;
mod message;
mod device;
mod net;
mod gpu;
mod audio;
mod shared_memory;
mod user_ptr;
mod vma;

use alloc::boxed::Box;
use alloc::vec::Vec;
use arch::{apic, cpu, idt, paging, percpu, smp, syscall};
use drivers::{acpi, gop, nvme, pci, serial, virtio_gpu, virtio_net, virtio_sound, xhci};
use toyos_abi::boot::{KernelArgs, MemoryMapEntry};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // Write a fixed marker to serial WITHOUT allocating, in case
    // the panic occurred while the allocator lock was held.
    serial::write_bytes(b"\n[kernel] !!! PANIC !!!\n");
    // Now try the full formatted message (may deadlock if allocator is locked)
    log!("KERNEL PANIC: {}", info);
    cpu::halt()
}

#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    serial::init();
    log!("{:?}", kernel_args);

    // KernelArgs contains physical addresses from the bootloader.
    // Add PHYS_OFFSET to get dereferenceable kernel virtual addresses.
    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        (kernel_args.memory_map_addr + PHYS_OFFSET) as *const MemoryMapEntry,
        entry_count,
    );
    let initrd = core::slice::from_raw_parts(
        (kernel_args.initrd_addr + PHYS_OFFSET) as *const u8,
        kernel_args.initrd_size as usize,
    );
    let kernel_elf = core::slice::from_raw_parts(
        (kernel_args.kernel_elf_addr + PHYS_OFFSET) as *const u8,
        kernel_args.kernel_elf_size as usize,
    );
    let init_bytes = core::slice::from_raw_parts(
        (kernel_args.init_program_addr + PHYS_OFFSET) as *const u8,
        kernel_args.init_program_len as usize,
    );
    let init_programs = core::str::from_utf8(init_bytes).expect("init_programs: invalid UTF-8");

    kernel_main(&kernel_args, maps, initrd, kernel_elf, init_programs);
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

fn kernel_main(
    kernel_args: &KernelArgs,
    maps: &[MemoryMapEntry],
    initrd: &[u8],
    kernel_elf: &[u8],
    init_programs: &str,
) -> ! {
    // ── Phase 1: Memory ─────────────────────────────────────────────────
    let reserved = [
        allocator::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        allocator::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        allocator::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
        allocator::Region { start: kernel_args.kernel_stack_addr, end: kernel_args.kernel_stack_addr + kernel_args.kernel_stack_size },
        allocator::Region { start: 0x8000, end: 0x9000 }, // AP trampoline page
    ];
    unsafe { allocator::init(maps, &reserved); }

    // Copy init_programs into heap before init_buddy reclaims bootloader memory.
    // The original pointer is into the bootloader's .rodata, which the buddy
    // allocator treats as free usable memory.
    let init_programs = alloc::string::String::from(init_programs);
    let init_programs: &str = &init_programs;

    paging::init(maps);
    unsafe { allocator::init_buddy(maps, &reserved); }

    // ── Phase 2: CPU — exceptions, LAPIC, clock ─────────────────────────
    // Get exception handlers up ASAP so bugs in later phases produce diagnostics
    // instead of triple-faulting.
    let madt = acpi::parse_madt(kernel_args.rsdp_addr).expect("ACPI: MADT not found");
    apic::init(madt.local_apic_addr);
    percpu::init_bsp(apic::id() as u32);
    idt::init();
    syscall::init();
    symbols::set_kernel_base(kernel_args.kernel_memory_addr);
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, kernel_args.kernel_memory_addr);
    }

    // HPET clock — enables profiling for everything from here on
    let hpet_base = acpi::find_hpet_base(kernel_args.rsdp_addr)
        .expect("ACPI: HPET not found");
    paging::map_kernel(PhysAddr::new(hpet_base), 0x1000);
    clock::init(hpet_base);
    apic::init_timer();

    log!("Boot: CPU ready ({}ms)", clock::nanos_since_boot() / 1_000_000);

    // ── Phase 3: Storage ────────────────────────────────────────────────
    let t_storage = clock::nanos_since_boot();

    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    pci::enumerate(ecam_base);
    let nvme_dev = nvme::init(ecam_base).expect("NVMe: no controller found");
    page_cache::init(Box::new(nvme_dev));

    let toyfs_instance = {
        let mut guard = page_cache::lock();
        let (cache, dev) = guard.cache_and_dev();
        match toyfs::ToyFs::mount(cache, dev) {
            Some(fs) => fs,
            None => toyfs::ToyFs::format(cache, dev),
        }
    };

    log!("Boot: storage ready ({}ms)", (clock::nanos_since_boot() - t_storage) / 1_000_000);

    // ── Phase 4: Peripherals ────────────────────────────────────────────
    let t_periph = clock::nanos_since_boot();

    let xhci_ctrl = xhci::init(ecam_base).expect("xHCI: no USB controller found");
    xhci::set_global(xhci_ctrl);
    acpi::init_power(kernel_args.rsdp_addr);

    log!("Boot: peripherals ready ({}ms)", (clock::nanos_since_boot() - t_periph) / 1_000_000);

    // ── Phase 5: Kernel subsystems ──────────────────────────────────────
    let t_subsys = clock::nanos_since_boot();

    smp::boot_aps(&madt);
    vfs::init();
    process::init();
    pipe::init();
    shared_memory::init();

    // Mount initrd ramdisk
    assert!(!initrd.is_empty(), "No initrd provided");
    let initrd_disk = unsafe { ramdisk::InitrdDisk::new(initrd.as_ptr(), initrd.len()) };
    let mut initrd_fs = tyfs::SimpleFs::mount(initrd_disk).expect("Failed to mount initrd");

    // Mount root filesystem (ToyFs on NVMe) and tmpfs
    vfs::lock().set_root(Box::new(toyfs::ToyFsAdapter::new(toyfs_instance)));
    vfs::lock().mount("tmp", Box::new(crate::tmpfs::TmpFs::new()));

    // Extract initrd into root filesystem (fresh binaries every boot)
    {
        let t0 = clock::nanos_since_boot();

        // Nuke old system directories and reclaim space
        {
            let mut v = vfs::lock();
            let r = v.root_mut();
            r.delete_prefix("bin/");
            r.delete_prefix("lib/");
            r.delete_prefix("share/");
        }

        let t1 = clock::nanos_since_boot();

        let files = initrd_fs.list();
        let mut total_bytes = 0u64;
        for (name, _size) in &files {
            if let Some(target) = initrd_fs.read_link(name) {
                vfs::lock().root_mut().create_symlink(name, &target)
                    .unwrap_or_else(|e| panic!("symlink {} -> {}: {}", name, target, e));
            } else {
                let data = initrd_fs.read_file(name)
                    .unwrap_or_else(|e| panic!("read initrd {}: {:?}", name, e));
                let mtime = initrd_fs.file_mtime(name).unwrap_or(0);
                total_bytes += data.len() as u64;
                vfs::lock().root_mut().create(name, &data, mtime)
                    .unwrap_or_else(|e| panic!("extract {} ({} bytes): {}", name, data.len(), e));
            }
        }

        let t2 = clock::nanos_since_boot();

        vfs::lock().root_mut().sync();

        let t3 = clock::nanos_since_boot();
        log!("Boot: initrd {} files, {} MB extracted in {}ms (delete {}ms, write {}ms, sync {}ms)",
            files.len(), total_bytes / (1024 * 1024),
            (t3 - t0) / 1_000_000,
            (t1 - t0) / 1_000_000,
            (t2 - t1) / 1_000_000,
            (t3 - t2) / 1_000_000);
    }
    // initrd_fs dropped

    // Ensure home directory exists
    vfs::lock().create_dir("/home");
    vfs::lock().create_dir("/home/root");
    vfs::lock().create_dir("/home/root/.config");

    log!("Boot: subsystems ready ({}ms)", (clock::nanos_since_boot() - t_subsys) / 1_000_000);

    // ── Phase 6: Devices ────────────────────────────────────────────────
    let t_devices = clock::nanos_since_boot();

    virtio_net::init(ecam_base);

    if let Some((sound, audio_info)) = virtio_sound::init(ecam_base) {
        crate::audio::register(sound, audio_info);
    }

    // Initialize GPU: try VirtIO first, fall back to UEFI GOP
    if let Some((gpu_driver, gpu_info)) = virtio_gpu::init(ecam_base) {
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

    #[cfg(feature = "debug-wait")]
    {
        use core::sync::atomic::AtomicBool;
        static DEBUG_WAIT: AtomicBool = AtomicBool::new(true);
        log!("debug: waiting for debugger — set DEBUG_WAIT=false to continue");
        while DEBUG_WAIT.load(core::sync::atomic::Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }

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
