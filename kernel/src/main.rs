#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::format;
use kernel::*;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    serial::println("PANIC!");
    serial::println(&format!("{}", info));
    loop {}
}

#[no_mangle]
pub unsafe extern "sysv64" fn _start(kernel_args: KernelArgs) -> ! {
    serial::init_serial();

    // Initialize allocator first — no allocations before this point
    let entry_count = kernel_args.memory_map_size as usize / core::mem::size_of::<MemoryMapEntry>();
    let maps = core::slice::from_raw_parts(
        kernel_args.memory_map_addr as *const MemoryMapEntry,
        entry_count,
    );
    allocator::init(maps, kernel_args.kernel_memory_addr, kernel_args.kernel_memory_size);

    serial::println("Hello from Kernel!");

    if let Some(ecam_base) = acpi::find_ecam_base(kernel_args.rsdp_addr) {
        pci::enumerate(ecam_base);
    } else {
        serial::println("ACPI: Failed to find ECAM base address");
    }

    // Mount initrd ramdisk
    assert!(kernel_args.initrd_size > 0, "No initrd provided");
    serial::println(&format!(
        "Initrd: addr={:#x} size={} bytes",
        kernel_args.initrd_addr, kernel_args.initrd_size
    ));

    let ramdisk = tyfs::SliceDisk::new(
        kernel_args.initrd_addr as *mut u8,
        kernel_args.initrd_size as usize,
        512,
    );
    let disk = tyfs::Disk::new(ramdisk);
    let mut fs = tyfs::SimpleFs::mount(disk).expect("Failed to mount initrd");
    serial::println("TYFS: mounted initrd");
    for (name, size) in fs.list() {
        serial::println(&format!("  {} ({} bytes)", name, size));
    }
    if let Some(data) = fs.read_file("hello.txt") {
        if let Ok(text) = core::str::from_utf8(&data) {
            serial::println(&format!("hello.txt: {}", text));
        }
    }

    loop {}
}
