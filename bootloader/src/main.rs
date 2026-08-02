#![no_main]
#![no_std]

extern crate alloc;

use core::mem;

use alloc::vec;
use alloc::alloc::Layout;
use elf::{abi, endian::AnyEndian, ElfBytes};
use uefi::{
    prelude::*,
    CStr16,
    proto::console::gop::{GraphicsOutput, PixelFormat},
    proto::device_path::{media::{PartitionFormat, PartitionSignature}, DevicePath, DevicePathNode, DeviceType, DeviceSubType},
    proto::loaded_image::LoadedImage,
    proto::media::file::{File, FileInfo, FileMode},
    table::{boot::{MemoryType, PAGE_SIZE}, cfg::ACPI2_GUID},
};
use uefi_services::println;
use toyos_abi::boot::{KernelArgs, MemoryMapEntry};

/// The largest file the bootloader will read off the ESP.
///
/// Nothing here has a caller to return an error to and nothing has run that
/// could recover, so every check in this file ends in a named panic rather
/// than an error path. This one exists so that a corrupt or hostile directory
/// entry is a refusal that says what it refused, instead of a firmware pool
/// request sized by whatever the ESP claimed.
///
/// Policy, and generous: the largest file ToyOS puts on the ESP is the initrd
/// at ~200 MB, so this is five times the real worst case and still far below
/// what a UEFI implementation would serve in one allocation.
const MAX_ESP_FILE: u64 = 1024 * 1024 * 1024;

fn alloc_kernel_memory(size: usize) -> vec::Vec<u8> {
    const KERNEL_ALIGN: usize = 2 * 1024 * 1024; // 2MB
    let layout = Layout::from_size_align(size, KERNEL_ALIGN).expect("invalid layout");
    let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
    assert!(!ptr.is_null(), "kernel allocation failed");
    unsafe { vec::Vec::from_raw_parts(ptr, size, size) }
}

struct LoadedKernel {
    pub memory: vec::Vec<u8>,
    pub entry_offset: usize,
    pub stack_offset: usize,
    pub stack_size: usize,
}

fn load_file_bytes(handle: Handle, system_table: &SystemTable<Boot>, path: &CStr16) -> vec::Vec<u8> {
    let mut fs = system_table
        .boot_services()
        .get_image_file_system(handle)
        .expect("Failed to get file system");

    let mut file = fs
        .open_volume()
        .expect("Failed to open volume")
        .open(path, FileMode::Read, Default::default())
        .expect("Failed to open file")
        .into_regular_file()
        .expect("Failed to convert to regular file");

    let file_info_len = file
        .get_info::<FileInfo>(&mut [])
        .expect_err("Failed to get file info len")
        .data()
        .expect("File info len was None");

    let mut buffer = vec![0; file_info_len];
    let file_info = file
        .get_info::<FileInfo>(&mut buffer)
        .expect("Failed to get file info");

    let declared = file_info.file_size();
    assert!(
        declared <= MAX_ESP_FILE,
        "the ESP reports a {declared}-byte file, past the {MAX_ESP_FILE}-byte bound"
    );
    let size = declared as usize;
    let mut bytes = alloc_uninit(size);
    let read = file.read(&mut bytes).expect("Failed to read file");
    // Every byte handed back must have come from the file: the buffer was never
    // zeroed, so a short read would leave allocator garbage in the tail and the
    // caller would parse it as image content.
    assert_eq!(read, size, "short read: {read} of {size} bytes");

    bytes
}

/// A buffer to be filled by a read, allocated *without* zeroing it first.
/// The caller must check that the read filled the whole buffer.
///
/// Do not simplify to `vec![0; size]`: that memsets the whole initrd
/// immediately before `File::read` overwrites every byte. The chain is not
/// visible at the call site — `vec![0u8; n]` takes `SpecFromElem`'s zero branch
/// to `RawVec::with_capacity_zeroed_in` and so to `alloc_zeroed`, and uefi
/// 0.26's allocator implements only `alloc`/`dealloc`, so it falls through to
/// `GlobalAlloc`'s default of `alloc` plus `write_bytes(ptr, 0, size)`.
fn alloc_uninit(size: usize) -> vec::Vec<u8> {
    let layout = Layout::from_size_align(size, 1).expect("invalid layout");
    let ptr = unsafe { alloc::alloc::alloc(layout) };
    assert!(!ptr.is_null(), "file buffer allocation failed ({size} bytes)");
    unsafe { vec::Vec::from_raw_parts(ptr, size, size) }
}

/// Which partition this image was loaded from, as firmware knows it.
struct BootPartition {
    guid: [u8; 16],
    start_lba: u64,
    blocks: u64,
}

/// Ask firmware which partition it loaded us from.
///
/// `LoadedImage->DeviceHandle` is the handle the image came off, and the
/// HARDDRIVE node of that handle's device path carries the partition's
/// **unique** GUID — a name for one partition on one disk, which is what the
/// kernel needs and what neither a type GUID nor a disk GUID can give it. It
/// has to be read here, because the device path protocol dies with Boot
/// Services and there is no way to ask afterwards.
///
/// `None` is a machine, not a failure: PXE, an unpartitioned device, and a
/// signature type firmware chose not to fill in all land here, and the kernel
/// is expected to boot on all of them knowing it has no partition of its own.
/// Every early-return below is one of those, so none of them panics — which
/// makes this the one function in this file that does not.
fn boot_partition(handle: Handle, system_table: &SystemTable<Boot>) -> Option<BootPartition> {
    let bs = system_table.boot_services();
    let image = bs.open_protocol_exclusive::<LoadedImage>(handle).ok()?;
    let device = image.device()?;
    let path = bs.open_protocol_exclusive::<DevicePath>(device).ok()?;

    let is_hard_drive = |node: &&DevicePathNode| {
        node.full_type() == (DeviceType::MEDIA, DeviceSubType::MEDIA_HARD_DRIVE)
    };
    // Exactly one, not the last one. A path with two HARDDRIVE nodes describes
    // a partition inside a partition, and picking either is guessing which of
    // the two the kernel's block device will be looking at.
    let mut nodes = path.node_iter().filter(is_hard_drive);
    let node = nodes.next()?;
    if nodes.next().is_some() {
        println!("Boot partition: the device path has more than one HARDDRIVE node — ignoring it");
        return None;
    }

    let hd: &uefi::proto::device_path::media::HardDrive = node.try_into().ok()?;
    if hd.partition_format() != PartitionFormat::GPT {
        println!("Boot partition: firmware says this is not a GPT partition — ignoring it");
        return None;
    }
    let PartitionSignature::Guid(guid) = hd.partition_signature() else {
        println!("Boot partition: firmware named it with no GUID signature — ignoring it");
        return None;
    };
    Some(BootPartition {
        guid: guid.to_bytes(),
        start_lba: hd.partition_start(),
        blocks: hd.partition_size(),
    })
}

/// Name the partition the kernel's log goes on, without reading it.
///
/// Written beside `kernel.elf` and `initrd.img` by `src/image.rs`, which draws
/// the GUID and stamps the same sixteen bytes into the GPT entry. Read here
/// because this is the volume firmware designated and because the kernel has no
/// filesystem yet: the identity is *given* all the way down, and nothing at any
/// level scans for a partition of the right type or format.
///
/// A missing or short file panics, like every other check in this file. The
/// same function writes all three, so a volume with two of them was assembled
/// by something that is not this project — and booting it anyway would mean a
/// kernel that quietly has nowhere to write its log, on the machine that has no
/// other channel.
fn log_partition_guid(handle: Handle, system_table: &SystemTable<Boot>) -> [u8; 16] {
    let bytes = load_file_bytes(handle, system_table, cstr16!("\\toyos\\log.guid"));
    <[u8; 16]>::try_from(bytes.as_slice()).unwrap_or_else(|_| {
        panic!("\\toyos\\log.guid holds {} bytes, wanted 16", bytes.len())
    })
}

/// Kernel virtual base: all physical memory is mapped here in the kernel's address space.
const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

fn load_kernel_elf(kernel_elf_bytes: &[u8]) -> LoadedKernel {
    let elf = ElfBytes::<AnyEndian>::minimal_parse(&kernel_elf_bytes)
        .expect("Failed to parse kernel elf");

    let segments = elf.segments().expect("Failed to get segments");
    let section_headers = elf.section_headers().expect("Failed to get sections");

    let stack_size: usize = 8 * 1024 * 1024; // 8MB

    // The kernel's own `elf::parse_layout` enforces `p_filesz <= p_memsz`
    // because the pair is a (copy length, destination size) pair to it. It is
    // the same pair here, and nothing had checked it: the image is sized from
    // every `p_memsz` and each segment is then copied in at `p_filesz`.
    let mut mem_size: u64 = 0;
    segments.iter().for_each(|segment| {
        if segment.p_type == abi::PT_LOAD {
            assert!(
                segment.p_filesz <= segment.p_memsz,
                "kernel.elf: PT_LOAD p_filesz {} > p_memsz {}",
                segment.p_filesz, segment.p_memsz
            );
            let end = segment
                .p_vaddr
                .checked_add(segment.p_memsz)
                .expect("kernel.elf: PT_LOAD p_vaddr + p_memsz overflows");
            mem_size = mem_size.max(end);
        }
    });

    println!("Kernel stack size: {}", stack_size);
    let mem_size = mem_size
        .checked_add(stack_size as u64)
        .and_then(|n| usize::try_from(n).ok())
        .expect("kernel.elf: image plus stack does not fit an allocation");

    println!("Kernel memory size: {}", mem_size);

    let mut process_mem = alloc_kernel_memory(mem_size);
    println!("Kernel memory located at: {:?}", process_mem.as_ptr());

    segments.iter().for_each(|segment| {
        if segment.p_type == abi::PT_LOAD {
            println!("Loading segment: {:?}", segment);
            let fstart = segment.p_offset as usize;
            let fend = segment
                .p_offset
                .checked_add(segment.p_filesz)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|end| *end <= kernel_elf_bytes.len())
                .expect("kernel.elf: PT_LOAD file extent is past the end of the file");
            let vstart = segment.p_vaddr as usize;
            // In bounds by construction: `mem_size` is at least
            // `p_vaddr + p_memsz` for this segment and `p_filesz <= p_memsz`.
            let vend = vstart + segment.p_filesz as usize;
            process_mem[vstart..vend].copy_from_slice(&kernel_elf_bytes[fstart..fend]);
        }
    });

    if section_headers
        .iter()
        .find(|section| section.sh_type == abi::SHT_REL)
        .is_some()
    {
        panic!("SHT_REL not supported");
    }

    let mut reloc_count = 0u64;
    section_headers
        .iter()
        .filter(|section_header| section_header.sh_type == abi::SHT_RELA)
        .for_each(|section_header| {
            elf.section_data_as_relas(&section_header)
                .expect("Failed to parse SHT_RELA")
                .for_each(|rela| {
                    match rela.r_type {
                        abi::R_X86_64_RELATIVE => {
                            // Both fields index the image and both come out of
                            // the file: `r_offset` is the destination of an
                            // 8-byte store and `r_addend` is the address
                            // stored. Unchecked, the store is an arbitrary
                            // write anywhere in the machine, made before
                            // ExitBootServices with firmware still live.
                            let offset = rela.r_offset;
                            let addend = rela.r_addend;
                            assert!(
                                offset.checked_add(8).is_some_and(|end| end <= mem_size as u64),
                                "kernel.elf: relocation stores 8 bytes at {offset:#x}, outside the {mem_size:#x}-byte image"
                            );
                            assert!(
                                (0..=mem_size as i64).contains(&addend),
                                "kernel.elf: relocation addend {addend:#x} is outside the {mem_size:#x}-byte image"
                            );
                            let value = PHYS_OFFSET + unsafe { process_mem.as_ptr().add(addend as usize) } as u64;
                            unsafe {
                                process_mem
                                    .as_mut_ptr()
                                    .add(offset as usize)
                                    .cast::<u64>()
                                    .write(value);
                            }
                            reloc_count += 1;
                        }
                        _ => panic!("Unsupported relocation type"),
                    }
                });
        });
    println!("Applied {} relocations", reloc_count);

    LoadedKernel {
        memory: process_mem,
        entry_offset: elf.ehdr.e_entry as usize,
        stack_offset: mem_size - stack_size,
        stack_size,
    }
}

static INIT_PROGRAMS: &[u8] = env!("INIT_PROGRAMS").as_bytes();

struct GopInfo {
    framebuffer: u64,
    framebuffer_size: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
}

fn query_gop(system_table: &SystemTable<Boot>) -> Option<GopInfo> {
    let bs = system_table.boot_services();
    let gop_handle = bs.get_handle_for_protocol::<GraphicsOutput>().ok()?;
    let mut gop = bs.open_protocol_exclusive::<GraphicsOutput>(gop_handle).ok()?;

    // Find the highest-resolution mode with a supported pixel format
    let mut best_mode = None;
    let mut best_pixels = 0usize;
    for mode in gop.modes(bs) {
        let info = mode.info();
        match info.pixel_format() {
            PixelFormat::Rgb | PixelFormat::Bgr => {}
            _ => continue,
        }
        let (w, h) = info.resolution();
        if w * h > best_pixels {
            best_pixels = w * h;
            best_mode = Some(mode);
        }
    }

    if let Some(target) = best_mode {
        println!("GOP: selecting best mode ({}x{})", target.info().resolution().0, target.info().resolution().1);
        gop.set_mode(&target).expect("failed to set GOP mode");
    }

    let mode = gop.current_mode_info();
    let (width, height) = mode.resolution();
    let stride = mode.stride();
    let pixel_format = match mode.pixel_format() {
        PixelFormat::Rgb => 0,
        PixelFormat::Bgr => 1,
        _ => return None,
    };

    let mut fb = gop.frame_buffer();
    let framebuffer = fb.as_mut_ptr() as u64;
    let framebuffer_size = fb.size() as u64;

    println!("GOP: {}x{} stride={} format={} fb={:#x} size={}",
        width, height, stride, pixel_format, framebuffer, framebuffer_size);

    Some(GopInfo {
        framebuffer,
        framebuffer_size,
        width: width as u32,
        height: height as u32,
        stride: stride as u32,
        pixel_format,
    })
}

/// Build minimal boot page tables for kernel transition to high half.
/// `pt_mem` is a pointer to PT_PAGES * 4096 bytes of zeroed memory.
/// Returns the physical address of the PML4.
///
/// Maps first `size` bytes of physical memory at both identity (PML4[0]) and
/// high-half (PML4[256] = PHYS_OFFSET). Uses 2MB large pages.
unsafe fn build_boot_page_tables(pt_mem: *mut u8, size: u64) -> u64 {
    const PAGE_PRESENT: u64 = 1 << 0;
    const PAGE_WRITE: u64 = 1 << 1;
    const PAGE_SIZE_BIT: u64 = 1 << 7;
    const PAGE_2M: u64 = 2 * 1024 * 1024;
    const GB: u64 = 1 << 30;

    let mut next_page = 0usize;
    let mut alloc_page = |pt_mem: *mut u8| -> *mut u64 {
        let p = pt_mem.add(next_page * 4096) as *mut u64;
        next_page += 1;
        p
    };

    let pml4 = alloc_page(pt_mem);
    let identity_pdpt = alloc_page(pt_mem);
    let high_pdpt = alloc_page(pt_mem);

    let num_gb = ((size + GB - 1) / GB) as usize;
    for gi in 0..num_gb {
        let pd = alloc_page(pt_mem);
        for pdi in 0..512u64 {
            let phys = gi as u64 * GB + pdi * PAGE_2M;
            if phys < size {
                *pd.add(pdi as usize) = phys | PAGE_PRESENT | PAGE_WRITE | PAGE_SIZE_BIT;
            }
        }
        let pd_phys = pd as u64;
        *identity_pdpt.add(gi) = pd_phys | PAGE_PRESENT | PAGE_WRITE;
        *high_pdpt.add(gi) = pd_phys | PAGE_PRESENT | PAGE_WRITE;
    }

    // PML4[0] = identity, PML4[256] = high-half (PHYS_OFFSET >> 39 = 256)
    *pml4.add(0) = identity_pdpt as u64 | PAGE_PRESENT | PAGE_WRITE;
    *pml4.add(256) = high_pdpt as u64 | PAGE_PRESENT | PAGE_WRITE;

    pml4 as u64
}

fn start_kernel(kernel: LoadedKernel, kernel_elf_bytes: vec::Vec<u8>, initrd: vec::Vec<u8>, rsdp_addr: u64, gop: Option<GopInfo>, boot_part: Option<BootPartition>, log_partition_guid: [u8; 16], system_table: SystemTable<Boot>) -> ! {
    let mms = system_table.boot_services().memory_map_size();
    let memory_map_entry_count = mms.map_size / mms.entry_size + 8;
    let mut memory_map = vec::Vec::<MemoryMapEntry>::with_capacity(memory_map_entry_count);

    // Pre-allocate page table pages before exiting boot services.
    // We need: 1 PML4 + 2 PDPTs + up to 8 PDs (for 8GB) = ~11 pages max.
    // Allocate as a flat array and split into 512-entry pages.
    const PT_PAGES: usize = 12;
    let pt_layout = Layout::from_size_align(PT_PAGES * 4096, 4096).unwrap();
    let pt_mem = unsafe { alloc::alloc::alloc_zeroed(pt_layout) };
    assert!(!pt_mem.is_null(), "page table allocation failed");

    let (_system_table, uefi_memory_map) = system_table.exit_boot_services(MemoryType::LOADER_DATA);

    uefi_memory_map.entries().for_each(|entry| {
        memory_map.push(MemoryMapEntry {
            uefi_type: entry.ty.0,
            start: entry.phys_start as u64,
            end: entry.phys_start + entry.page_count * PAGE_SIZE as u64,
        });
    });

    let (gop_framebuffer, gop_framebuffer_size, gop_width, gop_height, gop_stride, gop_pixel_format) =
        match &gop {
            Some(g) => (g.framebuffer, g.framebuffer_size, g.width, g.height, g.stride, g.pixel_format),
            None => (0, 0, 0, 0, 0, 0),
        };

    let (boot_partition_guid, boot_partition_start_lba, boot_partition_blocks, boot_partition_present) =
        match &boot_part {
            Some(p) => (p.guid, p.start_lba, p.blocks, 1),
            None => ([0u8; 16], 0, 0, 0),
        };

    // KernelArgs: all addresses are PHYSICAL (kernel translates to virtual)
    let kernel_phys = kernel.memory.as_ptr() as u64;
    let mut kernel_args = KernelArgs {
        memory_map_addr: memory_map.as_ptr() as u64,
        memory_map_size: memory_map.len() as u64 * mem::size_of::<MemoryMapEntry>() as u64,
        kernel_memory_addr: kernel_phys,
        kernel_memory_size: kernel.memory.len() as u64,
        kernel_stack_addr: kernel.stack_offset as u64,
        kernel_stack_size: kernel.stack_size as u64,
        rsdp_addr,
        initrd_addr: initrd.as_ptr() as u64,
        initrd_size: initrd.len() as u64,
        init_program_addr: INIT_PROGRAMS.as_ptr() as u64,
        init_program_len: INIT_PROGRAMS.len() as u64,
        kernel_elf_addr: kernel_elf_bytes.as_ptr() as u64,
        kernel_elf_size: kernel_elf_bytes.len() as u64,
        gop_framebuffer,
        gop_framebuffer_size,
        gop_width,
        gop_height,
        gop_stride,
        gop_pixel_format,
        boot_pml4_addr: 0, // set below after page tables are built
        boot_partition_start_lba,
        boot_partition_blocks,
        boot_partition_guid,
        boot_partition_present,
        log_partition_guid,
    };

    // Build boot page tables: identity map + high-half map for first 4GB.
    let pml4_phys = unsafe { build_boot_page_tables(pt_mem, 4 * 1024 * 1024 * 1024) };
    kernel_args.boot_pml4_addr = pml4_phys;

    // Switch to new page tables (identity map keeps us alive)
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) pml4_phys, options(nostack)) };

    let entry_virt = PHYS_OFFSET + kernel_phys + kernel.entry_offset as u64;

    mem::forget(memory_map);
    mem::forget(kernel.memory);
    mem::forget(kernel_elf_bytes);
    mem::forget(initrd);

    let entry: extern "sysv64" fn(&KernelArgs) -> ! = unsafe { mem::transmute(entry_virt) };
    entry(&kernel_args);
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();
    println!("ToyOS Bootloader 1.0");

    // Find ACPI 2.0 RSDP from UEFI configuration table
    let rsdp_addr = system_table
        .config_table()
        .iter()
        .find(|entry| entry.guid == ACPI2_GUID)
        .map(|entry| entry.address as u64)
        .expect("ACPI 2.0 RSDP not found in UEFI config table");
    println!("RSDP address: {:#x}", rsdp_addr);

    let boot_part = boot_partition(handle, &system_table);
    match &boot_part {
        Some(p) => println!(
            "Boot partition: LBA {}+{} signature {:02x?}",
            p.start_lba, p.blocks, p.guid
        ),
        None => println!("Boot partition: this machine has none"),
    }

    println!("Loading kernel...");
    let kernel_bytes = load_file_bytes(handle, &system_table, cstr16!("\\toyos\\kernel.elf"));
    println!("Kernel: {} bytes", kernel_bytes.len());

    println!("Loading initrd...");
    let initrd = load_file_bytes(handle, &system_table, cstr16!("\\toyos\\initrd.img"));
    println!("Initrd: {} bytes", initrd.len());

    let log_guid = log_partition_guid(handle, &system_table);
    println!("Log partition: signature {:02x?}", log_guid);

    println!("Loading kernel elf...");
    let loaded_kernel = load_kernel_elf(&kernel_bytes);

    // Query UEFI GOP before exiting boot services
    let gop = query_gop(&system_table);

    println!("Starting kernel...");
    start_kernel(loaded_kernel, kernel_bytes, initrd, rsdp_addr, gop, boot_part, log_guid, system_table);
}
