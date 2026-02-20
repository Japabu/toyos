#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use kernel::*;
use tyfs::Disk;
use core::fmt::Write;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = write!(serial::SerialWriter, "PANIC: {}\n", info);
    loop {}
}

#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    serial::init_serial();
    let _ = write!(serial::SerialWriter, "{:?}\n", kernel_args);

    // Initialize allocator first — no allocations before this point
    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        kernel_args.memory_map_addr as *const MemoryMapEntry,
        entry_count,
    );
    allocator::init(
        maps,
        kernel_args.kernel_memory_addr,
        kernel_args.kernel_memory_size,
        kernel_args.initrd_addr,
        kernel_args.initrd_size,
    );

    serial::println("Hello from Kernel!");

    // Mount initrd ramdisk (needed to load font)
    assert!(kernel_args.initrd_size > 0, "No initrd provided");
    serial::println(&format!(
        "Initrd: addr={:#x} size={} bytes",
        kernel_args.initrd_addr, kernel_args.initrd_size
    ));

    let ramdisk = ramdisk::RamDisk::new(
        kernel_args.initrd_addr as *mut u8,
        kernel_args.initrd_size as usize,
    );
    let mut initrd_fs = tyfs::SimpleFs::mount(ramdisk).expect("Failed to mount initrd");
    serial::println("TYFS: mounted initrd");
    for (name, size) in initrd_fs.list() {
        serial::println(&format!("  {} ({} bytes)", name, size));
    }

    // Initialize framebuffer console
    serial::println(&format!(
        "GOP: {}x{} stride={} fmt={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height,
        kernel_args.framebuffer_stride, kernel_args.framebuffer_pixel_format
    ));
    let fb = framebuffer::Framebuffer::new(
        kernel_args.framebuffer_addr,
        kernel_args.framebuffer_size,
        kernel_args.framebuffer_width,
        kernel_args.framebuffer_height,
        kernel_args.framebuffer_stride,
        kernel_args.framebuffer_pixel_format,
    );
    let font_data = initrd_fs
        .read_file("font.bin")
        .expect("Failed to load font.bin from rootfs");
    console::init(fb, font_data);

    // From here on, log::println outputs to both serial and framebuffer
    log::println("ToyOS Kernel initialized");
    log::println(&format!(
        "Framebuffer: {}x{} stride={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height, kernel_args.framebuffer_stride
    ));

    // Initialize NVMe for persistent storage
    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    pci::enumerate(ecam_base);
    let nvme = nvme::init(ecam_base).expect("NVMe: no controller found");
    let mut disk = nvme::NvmeDisk::new(nvme);
    let total_bytes = disk.total_bytes();
    // Peek at byte 0 to check for existing TYFS magic
    let mut magic = [0u8; 4];
    disk.read(0, &mut magic);
    let nvme_fs = if &magic == b"TYFS" {
        log::println("NVMe: mounted TYFS");
        tyfs::SimpleFs::mount(disk).expect("TYFS header valid but mount failed")
    } else {
        log::println("NVMe: formatting TYFS");
        tyfs::SimpleFs::format(disk, total_bytes)
    };

    // Initialize USB (xHCI) keyboard
    let mut xhci_ctrl = xhci::init(ecam_base).expect("xHCI: no USB controller found");
    log::println("USB keyboard enabled");

    acpi::init_power(kernel_args.rsdp_addr);

    // Set up GDT (UEFI's may be in reclaimable memory) and interrupts
    gdt::init();
    interrupts::init();
    syscall::init();
    paging::set_all_user_accessible();
    log::println("Ring 3: ready");

    // Build VFS with mount points
    let mut vfs = vfs::Vfs::new();
    vfs.mount("initrd", Box::new(initrd_fs));
    vfs.mount("nvme", Box::new(nvme_fs));
    vfs.cd("/nvme");

    // Run init program
    let init_path = unsafe {
        let ptr = kernel_args.init_program_addr as *const u8;
        let len = kernel_args.init_program_len as usize;
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, len))
    };
    if !init_path.is_empty() {
        log::println(&format!("init: running {}", init_path));
        let data = vfs.read_file(init_path)
            .unwrap_or_else(|| panic!("init: {} not found", init_path));
        elf::run(&data);
    }

    // Enter interactive shell
    shell::run(&mut vfs, &mut xhci_ctrl);
}
