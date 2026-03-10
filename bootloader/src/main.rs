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
    proto::media::file::{File, FileInfo, FileMode},
    table::{boot::{MemoryType, PAGE_SIZE}, cfg::ACPI2_GUID},
};
use uefi_services::println;

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

#[repr(C)]
pub struct KernelArgs {
    pub memory_map_addr: u64,
    pub memory_map_size: u64,
    pub kernel_memory_addr: u64,
    pub kernel_memory_size: u64,
    pub kernel_stack_addr: u64,
    pub kernel_stack_size: u64,
    pub rsdp_addr: u64,
    pub initrd_addr: u64,
    pub initrd_size: u64,
    pub init_program_addr: u64,
    pub init_program_len: u64,
    pub kernel_elf_addr: u64,
    pub kernel_elf_size: u64,
    pub gop_framebuffer: u64,
    pub gop_framebuffer_size: u64,
    pub gop_width: u32,
    pub gop_height: u32,
    pub gop_stride: u32,
    pub gop_pixel_format: u32,
}

#[repr(C)]
struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
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

    let size = file_info.file_size() as usize;
    let mut bytes = vec![0; size];
    file.read(&mut bytes).expect("Failed to read file");

    bytes
}

fn load_kernel_elf(kernel_elf_bytes: &[u8]) -> LoadedKernel {
    let elf = ElfBytes::<AnyEndian>::minimal_parse(&kernel_elf_bytes)
        .expect("Failed to parse kernel elf");

    let segments = elf.segments().expect("Failed to get segments");
    let section_headers = elf.section_headers().expect("Failed to get sections");

    // calculate process memory size
    let stack_size: usize = 8 * 1024 * 1024; // 8MB

    let mut mem_size: usize = 0;
    segments.iter().for_each(|segment| {
        if segment.p_type == abi::PT_LOAD {
            mem_size = mem_size.max((segment.p_vaddr + segment.p_memsz) as usize);
        }
    });

    // reserve space for stack at the end of the memory
    println!("Kernel stack size: {}", stack_size);
    mem_size += stack_size;

    println!("Kernel memory size: {}", mem_size);

    let mut process_mem = alloc_kernel_memory(mem_size);
    println!("Kernel memory located at: {:?}", process_mem.as_ptr());

    // handle load segments
    segments.iter().for_each(|segment| {
        if segment.p_type == abi::PT_LOAD {
            println!("Loading segment: {:?}", segment);
            let fstart = segment.p_offset as usize;
            let fend = fstart + segment.p_filesz as usize;
            let vstart = segment.p_vaddr as usize;
            let vend = vstart + segment.p_filesz as usize;
            process_mem[vstart..vend].copy_from_slice(&kernel_elf_bytes[fstart..fend]);
        }
    });

    // handle relocations
    if section_headers
        .iter()
        .find(|section| section.sh_type == abi::SHT_REL)
        .is_some()
    {
        panic!("SHT_REL not supported");
    }

    section_headers
        .iter()
        .filter(|section_header| section_header.sh_type == abi::SHT_RELA)
        .for_each(|section_header| {
            elf.section_data_as_relas(&section_header)
                .expect("Failed to parse SHT_RELA")
                .for_each(|rela| {
                    match rela.r_type {
                        abi::R_X86_64_RELATIVE => {
                            let offset = rela.r_offset as isize;
                            let addend = rela.r_addend as isize;
                            unsafe {
                                process_mem
                                    .as_mut_ptr()
                                    .byte_offset(offset)
                                    .cast::<u64>()
                                    .write(process_mem.as_ptr().byte_offset(addend) as u64);
                            }
                        }
                        _ => panic!("Unsupported relocation type"),
                    }
                });
        });

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

    // Switch to best mode
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

fn start_kernel(kernel: LoadedKernel, kernel_elf_bytes: vec::Vec<u8>, initrd: vec::Vec<u8>, rsdp_addr: u64, gop: Option<GopInfo>, system_table: SystemTable<Boot>) -> ! {
    // Estimate memory map size
    let mms = system_table.boot_services().memory_map_size();
    let memory_map_entry_count = mms.map_size / mms.entry_size + 8;
    let mut memory_map = vec::Vec::<MemoryMapEntry>::with_capacity(memory_map_entry_count);

    let (_system_table, uefi_memory_map) = system_table.exit_boot_services(MemoryType::LOADER_DATA);

    // Convert memory map to a format that the kernel can understand
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

    let kernel_args = KernelArgs {
        memory_map_addr: memory_map.as_ptr() as u64,
        memory_map_size: memory_map.len() as u64 * mem::size_of::<MemoryMapEntry>() as u64,
        kernel_memory_addr: kernel.memory.as_ptr() as u64,
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
    };
    let entry_addr = kernel.memory.as_ptr() as usize + kernel.entry_offset;

    mem::forget(memory_map);
    mem::forget(kernel.memory);
    mem::forget(kernel_elf_bytes);
    mem::forget(initrd);

    let entry: extern "sysv64" fn(KernelArgs) -> ! = unsafe { mem::transmute(entry_addr) };
    entry(kernel_args);
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

    println!("Loading kernel...");
    let kernel_bytes = load_file_bytes(handle, &system_table, cstr16!("\\toyos\\kernel.elf"));
    println!("Kernel: {} bytes", kernel_bytes.len());

    println!("Loading initrd...");
    let initrd = load_file_bytes(handle, &system_table, cstr16!("\\toyos\\initrd.img"));
    println!("Initrd: {} bytes", initrd.len());

    println!("Loading kernel elf...");
    let loaded_kernel = load_kernel_elf(&kernel_bytes);

    // Query UEFI GOP before exiting boot services
    let gop = query_gop(&system_table);

    println!("Starting kernel...");
    start_kernel(loaded_kernel, kernel_bytes, initrd, rsdp_addr, gop, system_table);
}
