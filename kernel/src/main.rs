#![no_std]
#![no_main]
#![feature(allocator_api)]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
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

    // Initialize NVMe for persistent storage
    let mut nvme_fs = None;
    if let Some(ecam_base) = acpi::find_ecam_base(kernel_args.rsdp_addr) {
        pci::enumerate(ecam_base);
        if let Some(mut nvme) = nvme::init(ecam_base) {
            let total_bytes = nvme.total_bytes();
            // Peek at sector 0 to check for existing TYFS magic
            let mut magic = [0u8; 4];
            nvme.read(0, &mut magic);
            let disk = tyfs::Disk::new(nvme);
            if &magic == b"TYFS" {
                log::println("NVMe: mounted TYFS");
                nvme_fs = Some(
                    tyfs::SimpleFs::mount(disk).expect("TYFS header valid but mount failed"),
                );
            } else {
                log::println("NVMe: formatting TYFS");
                nvme_fs = Some(tyfs::SimpleFs::format(disk, total_bytes));
            }
        }
    } else {
        log::println("ACPI: Failed to find ECAM base address");
    }
    if nvme_fs.is_none() {
        log::println("Warning: no persistent storage available");
    }

    acpi::init_power(kernel_args.rsdp_addr);

    // Set up GDT (UEFI's may be in reclaimable memory) and interrupts
    gdt::init();
    log::println("GDT: loaded");
    interrupts::init();
    log::println("Keyboard IRQ enabled");

    // VFS: current working directory
    let mut cwd = String::from("/nvme");

    console::write_str(&format!("{}> ", cwd));

    let mut line_buf = [0u8; 256];
    let mut line_len: usize = 0;

    // Text input mode state (for write/edit commands)
    let mut editing_file: Option<(String, String)> = None; // (mount, filename)
    let mut text_buf: Vec<u8> = Vec::new();

    /// Resolve a path argument against cwd into (mount, filename).
    /// Returns ("", "") for root, ("initrd", "") for mount listing,
    /// ("nvme", "hello.txt") for a file, etc.
    fn resolve_path(cwd: &str, arg: &str) -> (String, String) {
        let full = if arg.starts_with('/') {
            String::from(arg)
        } else if cwd == "/" {
            format!("/{}", arg)
        } else {
            format!("{}/{}", cwd, arg)
        };

        // Trim trailing slashes
        let full = full.trim_end_matches('/');
        if full.is_empty() {
            return (String::new(), String::new());
        }

        // Split: /mount/file
        let without_leading = &full[1..]; // skip leading /
        if let Some(pos) = without_leading.find('/') {
            let mount = &without_leading[..pos];
            let file = &without_leading[pos + 1..];
            (String::from(mount), String::from(file))
        } else {
            (String::from(without_leading), String::new())
        }
    }

    loop {
        if let Some(ch) = keyboard::try_read_char() {
            match ch {
                b'\n' => {
                    console::putchar(b'\n');
                    serial::println("");

                    let input = core::str::from_utf8(&line_buf[..line_len])
                        .unwrap_or("")
                        .trim();

                    if let Some((ref mount, ref filename)) = editing_file {
                        if input == "." {
                            // End text input — save file
                            let m = mount.clone();
                            let f = filename.clone();
                            let saved = match m.as_str() {
                                "nvme" => {
                                    if let Some(ref mut nfs) = nvme_fs {
                                        nfs.delete(&f);
                                        nfs.create(&f, &text_buf)
                                    } else {
                                        false
                                    }
                                }
                                "initrd" => {
                                    fs.delete(&f);
                                    fs.create(&f, &text_buf)
                                }
                                _ => false,
                            };
                            if saved {
                                log::println("File saved.");
                            } else {
                                log::println("Error: could not save file.");
                            }
                            editing_file = None;
                            text_buf.clear();
                        } else {
                            text_buf.extend_from_slice(&line_buf[..line_len]);
                            text_buf.push(b'\n');
                        }
                    } else if !input.is_empty() {
                        let (cmd, arg) = match input.find(' ') {
                            Some(pos) => (&input[..pos], input[pos + 1..].trim()),
                            None => (input, ""),
                        };

                        match cmd {
                            "help" => {
                                log::println("Commands:");
                                log::println("  ls [path]       List files");
                                log::println("  cat <file>      Print file contents");
                                log::println("  rm <file>       Delete a file");
                                log::println("  write <file>    Create a file");
                                log::println("  edit <file>     Edit a file");
                                log::println("  cd <path>       Change directory");
                                log::println("  pwd             Print working directory");
                                log::println("  clear           Clear screen");
                                log::println("  shutdown        Power off");
                            }
                            "clear" => console::clear(),
                            "shutdown" => acpi::shutdown(),
                            "pwd" => log::println(&cwd),
                            "cd" => {
                                let target = if arg.is_empty() { "/" } else { arg };
                                let new_cwd = if target == ".." {
                                    String::from("/")
                                } else if target == "/" {
                                    String::from("/")
                                } else {
                                    let (mount, _) = resolve_path(&cwd, target);
                                    match mount.as_str() {
                                        "initrd" | "nvme" => format!("/{}", mount),
                                        _ => {
                                            log::println(&format!(
                                                "cd: {}: no such directory",
                                                target
                                            ));
                                            String::new()
                                        }
                                    }
                                };
                                if !new_cwd.is_empty() {
                                    cwd = new_cwd;
                                }
                            }
                            "ls" => {
                                let (mount, _file) = if arg.is_empty() {
                                    resolve_path(&cwd, "")
                                } else {
                                    resolve_path(&cwd, arg)
                                };
                                match mount.as_str() {
                                    "" => {
                                        // Root listing
                                        log::println("  initrd/");
                                        if nvme_fs.is_some() {
                                            log::println("  nvme/");
                                        }
                                    }
                                    "initrd" => {
                                        let files = fs.list();
                                        if files.is_empty() {
                                            log::println("No files.");
                                        } else {
                                            for (name, size) in &files {
                                                log::println(&format!(
                                                    "  {} ({} bytes)",
                                                    name, size
                                                ));
                                            }
                                        }
                                    }
                                    "nvme" => {
                                        if let Some(ref mut nfs) = nvme_fs {
                                            let files = nfs.list();
                                            if files.is_empty() {
                                                log::println("No files.");
                                            } else {
                                                for (name, size) in &files {
                                                    log::println(&format!(
                                                        "  {} ({} bytes)",
                                                        name, size
                                                    ));
                                                }
                                            }
                                        } else {
                                            log::println("NVMe not available.");
                                        }
                                    }
                                    _ => {
                                        log::println(&format!(
                                            "ls: /{}: no such directory",
                                            mount
                                        ));
                                    }
                                }
                            }
                            "cat" => {
                                if arg.is_empty() {
                                    log::println("Usage: cat <file>");
                                } else {
                                    let (mount, file) = resolve_path(&cwd, arg);
                                    if file.is_empty() {
                                        log::println(&format!("cat: /{}: is a directory", mount));
                                    } else {
                                        let data = match mount.as_str() {
                                            "initrd" => fs.read_file(&file),
                                            "nvme" => nvme_fs.as_mut().and_then(|nfs| nfs.read_file(&file)),
                                            _ => None,
                                        };
                                        if let Some(data) = data {
                                            if let Ok(text) = core::str::from_utf8(&data) {
                                                log::println(text);
                                            } else {
                                                log::println(&format!(
                                                    "{}: {} bytes (binary)", file, data.len()
                                                ));
                                            }
                                        } else {
                                            log::println(&format!("{}: file not found", arg));
                                        }
                                    }
                                }
                            }
                            "rm" => {
                                if arg.is_empty() {
                                    log::println("Usage: rm <file>");
                                } else {
                                    let (mount, file) = resolve_path(&cwd, arg);
                                    if file.is_empty() {
                                        log::println("rm: cannot remove a mount point");
                                    } else {
                                        let ok = match mount.as_str() {
                                            "initrd" => fs.delete(&file),
                                            "nvme" => nvme_fs.as_mut().map_or(false, |nfs| nfs.delete(&file)),
                                            _ => false,
                                        };
                                        if ok {
                                            log::println(&format!("{}: deleted", file));
                                        } else {
                                            log::println(&format!("{}: file not found", arg));
                                        }
                                    }
                                }
                            }
                            "write" => {
                                if arg.is_empty() {
                                    log::println("Usage: write <file>");
                                } else {
                                    let (mount, file) = resolve_path(&cwd, arg);
                                    if file.is_empty() {
                                        log::println("write: need a filename");
                                    } else if mount != "initrd" && mount != "nvme" {
                                        log::println(&format!("write: /{}: no such directory", mount));
                                    } else {
                                        log::println("Enter text (type . on a line by itself to save):");
                                        editing_file = Some((mount, file));
                                        text_buf.clear();
                                    }
                                }
                            }
                            "edit" => {
                                if arg.is_empty() {
                                    log::println("Usage: edit <file>");
                                } else {
                                    let (mount, file) = resolve_path(&cwd, arg);
                                    if file.is_empty() {
                                        log::println("edit: need a filename");
                                    } else if mount != "initrd" && mount != "nvme" {
                                        log::println(&format!("edit: /{}: no such directory", mount));
                                    } else {
                                        let data = match mount.as_str() {
                                            "initrd" => fs.read_file(&file),
                                            "nvme" => nvme_fs.as_mut().and_then(|nfs| nfs.read_file(&file)),
                                            _ => None,
                                        };
                                        if let Some(data) = data {
                                            if let Ok(text) = core::str::from_utf8(&data) {
                                                log::println("Current contents:");
                                                log::println(text);
                                            } else {
                                                log::println(&format!(
                                                    "{}: binary file, cannot edit", file
                                                ));
                                                line_len = 0;
                                                console::write_str(&format!("{}> ", cwd));
                                                continue;
                                            }
                                        } else {
                                            log::println("(new file)");
                                        }
                                        log::println("Enter new text (type . on a line by itself to save):");
                                        editing_file = Some((mount, file));
                                        text_buf.clear();
                                    }
                                }
                            }
                            _ => {
                                log::println(&format!("Unknown command: {}", cmd));
                            }
                        }
                    }

                    line_len = 0;
                    if editing_file.is_some() {
                        console::write_str("| ");
                    } else {
                        console::write_str(&format!("{}> ", cwd));
                    }
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
