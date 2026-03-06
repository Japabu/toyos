#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use kernel::arch::{apic, idt, paging, percpu, smp, syscall};
use kernel::drivers::{acpi, gop, nvme, pci, serial, virtio_gpu, virtio_net, virtio_sound, xhci};
use kernel::{allocator, clock, fd, gpu, log, pipe, process, ramdisk, shared_memory, symbols, vfs, KernelArgs, MemoryMapEntry};
use tyfs::Disk;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("KERNEL PANIC: {}", info);
    loop {}
}

#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    serial::init();
    log!("{:?}", kernel_args);

    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        kernel_args.memory_map_addr as *const MemoryMapEntry,
        entry_count,
    );
    let initrd = core::slice::from_raw_parts(
        kernel_args.initrd_addr as *const u8,
        kernel_args.initrd_size as usize,
    );
    let kernel_elf = core::slice::from_raw_parts(
        kernel_args.kernel_elf_addr as *const u8,
        kernel_args.kernel_elf_size as usize,
    );
    let init_bytes = core::slice::from_raw_parts(
        kernel_args.init_program_addr as *const u8,
        kernel_args.init_program_len as usize,
    );
    let init_path = core::str::from_utf8(init_bytes).expect("init_program: invalid UTF-8");

    kernel_main(&kernel_args, maps, initrd, kernel_elf, init_path);
}

fn kernel_main(
    kernel_args: &KernelArgs,
    maps: &[MemoryMapEntry],
    initrd: &[u8],
    kernel_elf: &[u8],
    init_path: &str,
) -> ! {
    // Initialize allocator first
    let reserved = [
        allocator::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        allocator::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        allocator::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
        allocator::Region { start: 0x8000, end: 0x9000 }, // AP trampoline page
    ];
    unsafe { allocator::init(maps, &reserved); }

    paging::init(maps);

    log!("Hello from Kernel!");

    // Mount initrd ramdisk
    assert!(!initrd.is_empty(), "No initrd provided");
    log!("Initrd: addr={:#x} size={} bytes", initrd.as_ptr() as u64, initrd.len());

    let ramdisk = unsafe { ramdisk::RamDisk::new(initrd.as_ptr(), initrd.len()) };
    let mut initrd_fs = tyfs::SimpleFs::mount(ramdisk).expect("Failed to mount initrd");
    log!("TYFS: mounted initrd");
    for (name, size) in initrd_fs.list() {
        log!("  {} ({} bytes)", name, size);
    }

    log!("ToyOS Kernel initialized");

    // Initialize NVMe
    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    pci::enumerate(ecam_base);
    let nvme_ctrl = nvme::init(ecam_base).expect("NVMe: no controller found");
    let mut disk = nvme::NvmeDisk::new(nvme_ctrl);
    let total_bytes = disk.total_bytes();
    let mut magic = [0u8; 4];
    disk.read(0, &mut magic);
    let nvme_fs = if &magic == b"TYFS" {
        log!("NVMe: mounted TYFS");
        tyfs::SimpleFs::mount(disk).expect("TYFS header valid but mount failed")
    } else {
        log!("NVMe: formatting TYFS");
        tyfs::SimpleFs::format(disk, total_bytes)
    };

    // Initialize USB keyboard
    let xhci_ctrl = xhci::init(ecam_base).expect("xHCI: no USB controller found");
    xhci::set_global(xhci_ctrl);
    log!("USB keyboard enabled");

    acpi::init_power(kernel_args.rsdp_addr);

    // Initialize HPET clock
    let hpet_base = acpi::find_hpet_base(kernel_args.rsdp_addr)
        .expect("ACPI: HPET not found");
    paging::map_kernel(hpet_base, 0x1000);
    clock::init(hpet_base);

    // Parse MADT and init LAPIC (needed for per-CPU setup)
    let madt = acpi::parse_madt(kernel_args.rsdp_addr).expect("ACPI: MADT not found");
    apic::init(madt.local_apic_addr);

    // Per-CPU data + GDT for BSP
    percpu::init_bsp(apic::id() as u32);

    // IDT and syscall MSRs
    idt::init();
    symbols::set_kernel_base(kernel_args.kernel_memory_addr);

    // Load kernel symbols for crash diagnostics
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, kernel_args.kernel_memory_addr);
    }

    syscall::init();
    apic::init_timer();
    log!("Ring 3: ready");

    // Boot secondary CPUs
    smp::boot_aps(&madt);

    // Initialize subsystems
    vfs::init();
    process::init();
    pipe::init();
    shared_memory::init();

    // Mount filesystems
    vfs::lock().mount("initrd", Box::new(initrd_fs));
    vfs::lock().mount("nvme", Box::new(nvme_fs));

    // Load keyboard layout from config
    if let Ok(data) = vfs::lock().read_file("/nvme/config/keyboard_layout") {
        if let Ok(name) = core::str::from_utf8(&data) {
            let name = name.trim();
            kernel::keyboard::set_layout(name);
        }
    }
    log!("Keyboard layout: {}", kernel::keyboard::layout_name());

    // Initialize VirtIO networking
    virtio_net::init(ecam_base);

    // Initialize VirtIO sound
    if let Some(sound) = virtio_sound::init(ecam_base) {
        kernel::audio::register(sound);
    }

    // Initialize GPU: try VirtIO first, fall back to UEFI GOP
    if let Some((gpu_driver, gpu_info)) = virtio_gpu::init(ecam_base) {
        log!("GPU: using VirtIO");
        let fb_info = fd::FramebufferInfo {
            token: gpu_info.tokens,
            cursor_token: gpu_info.cursor_token,
            width: gpu_info.width,
            height: gpu_info.height,
            stride: gpu_info.stride,
            pixel_format: gpu_info.pixel_format,
            flags: gpu_info.flags,
        };
        syscall::set_screen_size(fb_info.width, fb_info.height);
        kernel::device::set_framebuffer_info(fb_info);
        gpu::register(gpu_driver, gpu_info);
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
        let fb_info = fd::FramebufferInfo {
            token: gpu_info.tokens,
            cursor_token: gpu_info.cursor_token,
            width: gpu_info.width,
            height: gpu_info.height,
            stride: gpu_info.stride,
            pixel_format: gpu_info.pixel_format,
            flags: gpu_info.flags,
        };
        syscall::set_screen_size(fb_info.width, fb_info.height);
        kernel::device::set_framebuffer_info(fb_info);
        gpu::register(gpu_driver, gpu_info);
    } else {
        log!("GPU: none found, running headless");
    };

    #[cfg(feature = "debug-wait")]
    {
        use core::sync::atomic::AtomicBool;
        static DEBUG_WAIT: AtomicBool = AtomicBool::new(true);
        log!("debug: waiting for debugger — set DEBUG_WAIT=false to continue");
        while DEBUG_WAIT.load(core::sync::atomic::Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    }

    // Spawn initial userland processes
    assert!(!init_path.is_empty(), "bootloader must provide init_program");
    let args: Vec<&str> = init_path.split_whitespace().collect();
    process::spawn_kernel(&args);

    // Optional services — skip if binary not present in initrd
    if let Some(pid) = process::spawn_optional(&["/initrd/netd"]) {
        log!("spawned netd pid={pid}");
    }
    if let Some(pid) = process::spawn_optional(&["/initrd/sshd"]) {
        log!("spawned sshd pid={pid}");
    }

    // Signal APs and enter the scheduler idle loop (never returns)
    smp::set_ready();
    kernel::scheduler::schedule_no_return();
}
