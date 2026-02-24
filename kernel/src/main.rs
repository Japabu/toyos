#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::alloc::{alloc_zeroed, Layout};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use kernel::arch::{apic, gdt, idt, paging, smp, syscall};
use kernel::drivers::{acpi, nvme, pci, serial, xhci};
use kernel::{allocator, clock, fd, log, pipe, process, ramdisk, symbols, vfs, KernelArgs, MemoryMapEntry};
use tyfs::Disk;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    log!("PANIC: {}", info);
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

const USER_STACK_SIZE: usize = 64 * 1024;

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

    let ramdisk = unsafe { ramdisk::RamDisk::new(initrd.as_ptr() as *mut u8, initrd.len()) };
    let mut initrd_fs = tyfs::SimpleFs::mount(ramdisk).expect("Failed to mount initrd");
    log!("TYFS: mounted initrd");
    for (name, size) in initrd_fs.list() {
        log!("  {} ({} bytes)", name, size);
    }

    // Map framebuffer for userland access and store screen dimensions
    log!("GOP: {}x{} stride={} fmt={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height,
        kernel_args.framebuffer_stride, kernel_args.framebuffer_pixel_format
    );
    paging::map_user(kernel_args.framebuffer_addr, kernel_args.framebuffer_size);
    syscall::set_screen_size(kernel_args.framebuffer_width, kernel_args.framebuffer_height);

    log!("ToyOS Kernel initialized");
    log!("Framebuffer: {}x{} stride={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height, kernel_args.framebuffer_stride
    );

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

    // Set up GDT and interrupts
    gdt::init();
    idt::init();
    idt::set_kernel_base(kernel_args.kernel_memory_addr);

    // Load kernel symbols for crash diagnostics
    if !kernel_elf.is_empty() {
        symbols::load_kernel(kernel_elf, kernel_args.kernel_memory_addr);
    }

    syscall::init();
    log!("Ring 3: ready");

    // Boot secondary CPUs
    let madt = acpi::parse_madt(kernel_args.rsdp_addr).expect("ACPI: MADT not found");
    apic::init(madt.local_apic_addr);
    smp::boot_aps(&madt);

    // Initialize subsystems
    vfs::init();
    process::init();
    pipe::init();

    // Mount filesystems
    vfs::global_mut().mount("initrd", Box::new(initrd_fs));
    vfs::global_mut().mount("nvme", Box::new(nvme_fs));

    // Load keyboard layout from config
    if let Some(data) = vfs::global_mut().read_file("/nvme/config/keyboard_layout") {
        if let Ok(name) = core::str::from_utf8(&data) {
            let name = name.trim();
            kernel::keyboard::set_layout(name);
        }
    }
    log!("Keyboard layout: {}", kernel::keyboard::layout_name());

    // Load and prepare the init program
    let init = if init_path.is_empty() { "shell" } else { init_path };
    let args: Vec<&str> = init.split_whitespace().collect();
    let cmd = args[0];
    let path = if cmd.starts_with('/') {
        String::from(cmd)
    } else {
        format!("/initrd/{}", cmd)
    };
    log!("init: running {}", path);
    let data = vfs::global_mut().read_file(&path)
        .unwrap_or_else(|| panic!("init: {} not found", path));

    // Load ELF, map user pages, set up stack
    let loaded = kernel::elf::load(&data).expect("init: ELF load failed");
    paging::map_user(loaded.base_ptr as u64, loaded.load_size as u64);

    let stack_layout = Layout::from_size_align(USER_STACK_SIZE, 4096).unwrap();
    let stack_base = unsafe { alloc_zeroed(stack_layout) };
    assert!(!stack_base.is_null(), "init: stack alloc failed");
    let stack_top = stack_base as u64 + USER_STACK_SIZE as u64;
    let elf_layout = Layout::from_size_align(loaded.load_size, 4096).unwrap();
    paging::map_user(stack_base as u64, USER_STACK_SIZE as u64);

    symbols::load_process(&data, loaded.base);
    symbols::set_process_ranges(
        loaded.base_ptr as u64,
        loaded.base_ptr as u64 + loaded.load_size as u64,
        stack_base as u64,
        stack_top,
    );

    let sp = process::write_argv_to_stack(stack_top, &args);

    log!("init: entry={:#x}, stack={:#x}, argc={}", loaded.entry, sp, args.len());

    // Store framebuffer info for open_device claims
    kernel::device::set_framebuffer_info(fd::FramebufferInfo {
        addr: kernel_args.framebuffer_addr,
        width: kernel_args.framebuffer_width,
        height: kernel_args.framebuffer_height,
        stride: kernel_args.framebuffer_stride,
        pixel_format: kernel_args.framebuffer_pixel_format,
    });

    // Create process 0 and start the scheduler (never returns)
    process::init_process0(loaded.entry, sp, loaded.base_ptr, elf_layout, stack_base, stack_layout);
    unreachable!();
}
