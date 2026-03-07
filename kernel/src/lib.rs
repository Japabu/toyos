#![no_std]
#![feature(allocator_api)]
extern crate alloc;

pub mod sync;
pub mod id_map;

pub mod arch;
pub mod drivers;

pub mod log;
pub mod allocator;

pub mod keyboard;
pub mod mouse;
pub mod ramdisk;
pub mod tmpfs;
pub mod vfs;
pub mod elf;
pub mod symbols;
pub mod process;
pub mod scheduler;
pub mod clock;
pub mod rtc;
pub mod fd;
pub mod pipe;
pub mod message;
pub mod device;
pub mod net;
pub mod gpu;
pub mod audio;
pub mod user_heap;
pub mod shared_memory;
pub mod user_ptr;

#[repr(C)]
#[derive(Debug)]
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
#[derive(Debug)]
pub struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}
