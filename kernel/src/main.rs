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

    // Initialize framebuffer console
    let fb = framebuffer::Framebuffer::new(
        kernel_args.framebuffer_addr,
        kernel_args.framebuffer_size,
        kernel_args.framebuffer_width,
        kernel_args.framebuffer_height,
        kernel_args.framebuffer_stride,
        kernel_args.framebuffer_pixel_format,
    );
    let font_data = fs
        .read_file("font8x16.bin")
        .expect("Failed to load font8x16.bin from rootfs");
    console::init(fb, &font_data);

    // From here on, log::println outputs to both serial and framebuffer
    log::println("ToyOS Kernel initialized");
    log::println(&format!(
        "Framebuffer: {}x{} stride={}",
        kernel_args.framebuffer_width, kernel_args.framebuffer_height, kernel_args.framebuffer_stride
    ));

    if let Some(ecam_base) = acpi::find_ecam_base(kernel_args.rsdp_addr) {
        pci::enumerate(ecam_base);
    } else {
        log::println("ACPI: Failed to find ECAM base address");
    }

    if let Some(data) = fs.read_file("hello.txt") {
        if let Ok(text) = core::str::from_utf8(&data) {
            log::println(&format!("hello.txt: {}", text));
        }
    }

    // Set up GDT (UEFI's may be in reclaimable memory) and interrupts
    gdt::init();
    log::println("GDT: loaded");
    interrupts::init();
    log::println("Keyboard IRQ enabled");

    // Shell loop
    console::write_str("> ");

    let mut line_buf = [0u8; 256];
    let mut line_len: usize = 0;

    loop {
        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\n' => {
                    console::putchar(b'\n');
                    serial::println("");

                    if let Ok(cmd) = core::str::from_utf8(&line_buf[..line_len]) {
                        let cmd = cmd.trim();
                        match cmd {
                            "" => {}
                            "help" => log::println("Commands: help, clear"),
                            "clear" => {
                                for _ in 0..50 { console::putchar(b'\n'); }
                            }
                            _ => log::println(&format!("Unknown command: {}", cmd)),
                        }
                    }

                    line_len = 0;
                    console::write_str("> ");
                }
                0x08 => {
                    if line_len > 0 {
                        line_len -= 1;
                        console::backspace();
                    }
                }
                ch => {
                    if line_len < line_buf.len() {
                        line_buf[line_len] = ch;
                        line_len += 1;
                        console::putchar(ch);
                    }
                }
            }
        } else {
            core::hint::spin_loop();
        }
    }
}
