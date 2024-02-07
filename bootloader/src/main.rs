#![no_main]
#![no_std]

extern crate alloc;

use core::mem;

use alloc::vec;
use elf::{abi, endian::AnyEndian, ElfBytes};
use uefi::{
    prelude::*,
    proto::media::file::{File, FileInfo, FileMode},
    table::boot::{MemoryType, PAGE_SIZE},
};
use uefi_services::println;

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
}

#[repr(C)]
struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}

fn load_kernel_bytes(handle: Handle, system_table: &SystemTable<Boot>) -> vec::Vec<u8> {
    let mut fs = system_table
        .boot_services()
        .get_image_file_system(handle)
        .expect("Failed to get file system");

    let mut kernel_file = fs
        .open_volume()
        .expect("Failed to open volume")
        .open(
            cstr16!("\\toyos\\kernel.elf"),
            FileMode::Read,
            Default::default(),
        )
        .expect("Failed to open kernel file")
        .into_regular_file()
        .expect("Failed to convert kernel file to regular file");

    let kernel_file_info_len = kernel_file
        .get_info::<FileInfo>(&mut [])
        .expect_err("Failed to get file info len")
        .data()
        .expect("File info len was None");

    let mut buffer = vec![0; kernel_file_info_len];
    let kernel_file_info = kernel_file
        .get_info::<FileInfo>(&mut buffer)
        .expect("Failed to get file info");

    println!("Kernel file size: {}", kernel_file_info.file_size());

    println!("Reading kernel...");
    let mut kernel_bytes = vec![0; kernel_file_info.file_size() as usize];
    kernel_file
        .read(&mut kernel_bytes)
        .expect("Failed to read kernel file");

    kernel_bytes
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

    let mut process_mem = vec![0; mem_size as usize];
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
                    println!("Relocation: {:?}", rela);
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

fn start_kernel(kernel: LoadedKernel, system_table: SystemTable<Boot>) -> ! {
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

    let kernel_args = KernelArgs {
        memory_map_addr: memory_map.as_ptr() as u64,
        memory_map_size: memory_map.len() as u64 * mem::size_of::<MemoryMapEntry>() as u64,
        kernel_memory_addr: kernel.memory.as_ptr() as u64,
        kernel_memory_size: kernel.memory.len() as u64,
        kernel_stack_addr: kernel.stack_offset as u64,
        kernel_stack_size: kernel.stack_size as u64,
    };
    let entry_addr = kernel.memory.as_ptr() as usize + kernel.entry_offset;

    mem::forget(memory_map);
    mem::forget(kernel.memory);

    let entry: extern "sysv64" fn(KernelArgs) -> ! = unsafe { mem::transmute(entry_addr) };
    entry(kernel_args);
}

#[entry]
fn main(handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();
    println!("ToyOS Bootloader 1.0");

    println!("Loading kernel...");
    let kernel_bytes = load_kernel_bytes(handle, &system_table);

    println!("Loading kernel elf...");
    let loaded_kernel = load_kernel_elf(&kernel_bytes);

    println!("Starting kernel...");
    start_kernel(loaded_kernel, system_table);
}
