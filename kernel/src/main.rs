#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use kernel::arch::{gdt, idt, paging, syscall};
use kernel::drivers::{acpi, framebuffer, nvme, pci, serial, xhci};
use kernel::{allocator, clock, console, log, process, ramdisk, symbols, vfs, KernelArgs, MemoryMapEntry};
use tyfs::Disk;
use core::fmt::Write;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _ = write!(serial::SerialWriter, "PANIC: {}\n", info);
    loop {}
}

#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    serial::init();
    let _ = write!(serial::SerialWriter, "{:?}\n", kernel_args);

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
    // Initialize allocator first — no allocations before this point
    let reserved = [
        allocator::Region { start: kernel_args.kernel_memory_addr, end: kernel_args.kernel_memory_addr + kernel_args.kernel_memory_size },
        allocator::Region { start: kernel_args.initrd_addr, end: kernel_args.initrd_addr + kernel_args.initrd_size },
        allocator::Region { start: kernel_args.kernel_elf_addr, end: kernel_args.kernel_elf_addr + kernel_args.kernel_elf_size },
    ];
    unsafe { allocator::init(maps, &reserved); }

    // Build our own page tables (identity-mapped, kernel-only).
    // Must happen right after allocator init, before UEFI's page table pages
    // get handed out by the allocator.
    paging::init(maps);

    serial::println("Hello from Kernel!");

    // Mount initrd ramdisk (needed to load font)
    assert!(!initrd.is_empty(), "No initrd provided");
    let _ = writeln!(serial::SerialWriter,
        "Initrd: addr={:#x} size={} bytes",
        initrd.as_ptr() as u64, initrd.len()
    );

    let ramdisk = unsafe { ramdisk::RamDisk::new(initrd.as_ptr() as *mut u8, initrd.len()) };
    let mut initrd_fs = tyfs::SimpleFs::mount(ramdisk).expect("Failed to mount initrd");
    serial::println("TYFS: mounted initrd");
    for (name, size) in initrd_fs.list() {
        let _ = writeln!(serial::SerialWriter, "  {} ({} bytes)", name, size);
    }

    // Initialize framebuffer console
    let _ = writeln!(serial::SerialWriter,
        "GOP: {}x{} stride={} fmt={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height,
        kernel_args.framebuffer_stride, kernel_args.framebuffer_pixel_format
    );
    let fb = unsafe {
        framebuffer::Framebuffer::new(
            kernel_args.framebuffer_addr,
            kernel_args.framebuffer_size,
            kernel_args.framebuffer_width,
            kernel_args.framebuffer_height,
            kernel_args.framebuffer_stride,
            kernel_args.framebuffer_pixel_format,
        )
    };
    let font_data = initrd_fs
        .read_file("font.bin")
        .expect("Failed to load font.bin from rootfs");
    console::init(fb, font_data);

    // From here on, log::println outputs to both serial and framebuffer
    log::println("ToyOS Kernel initialized");
    log!("Framebuffer: {}x{} stride={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height, kernel_args.framebuffer_stride
    );

    // Initialize NVMe for persistent storage
    let ecam_base = acpi::find_ecam_base(kernel_args.rsdp_addr)
        .expect("ACPI: failed to find ECAM base address");
    pci::enumerate(ecam_base);
    let nvme_ctrl = nvme::init(ecam_base).expect("NVMe: no controller found");
    let mut disk = nvme::NvmeDisk::new(nvme_ctrl);
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

    // Initialize USB (xHCI) keyboard (global singleton for sys_read polling)
    let xhci_ctrl = xhci::init(ecam_base).expect("xHCI: no USB controller found");
    xhci::set_global(xhci_ctrl);
    log::println("USB keyboard enabled");

    acpi::init_power(kernel_args.rsdp_addr);

    // Initialize HPET clock
    let hpet_base = acpi::find_hpet_base(kernel_args.rsdp_addr)
        .expect("ACPI: HPET not found");
    paging::map_kernel(hpet_base, 0x1000);
    clock::init(hpet_base);

    // Set up GDT (UEFI's may be in reclaimable memory) and interrupts
    gdt::init();
    idt::init();
    idt::set_kernel_base(kernel_args.kernel_memory_addr);

    // Load kernel symbols for crash diagnostics
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, kernel_args.kernel_memory_addr);
    }

    syscall::init();
    log::println("Ring 3: ready");

    // Build VFS with mount points
    let mut vfs = vfs::Vfs::new();
    vfs.mount("initrd", Box::new(initrd_fs));
    vfs.mount("nvme", Box::new(nvme_fs));

    // Move VFS into global static (accessible from syscall handlers)
    vfs::set_global(vfs);

    // Load keyboard layout from config (if present)
    if let Some(data) = vfs::global().read_file("/nvme/config/keyboard_layout") {
        if let Ok(name) = core::str::from_utf8(&data) {
            let name = name.trim();
            kernel::keyboard::set_layout(name);
        }
    }
    log!("Keyboard layout: {}", kernel::keyboard::layout_name());

    // Run init program (default: "shell")
    let init = if init_path.is_empty() { "shell" } else { init_path };
    let args: Vec<&str> = init.split_whitespace().collect();
    let cmd = args[0];
    let path = if cmd.starts_with('/') {
        String::from(cmd)
    } else {
        format!("/initrd/{}", cmd)
    };
    log!("init: running {}", path);
    let data = vfs::global().read_file(&path)
        .unwrap_or_else(|| panic!("init: {} not found", path));
    let code = process::run(&data, &args);
    kernel::fd::close_all(vfs::global());

    log!("init exited with code {}", code);
    kernel::arch::cpu::halt();
}
