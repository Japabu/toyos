#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]
extern crate alloc;

pub mod sync;
pub mod id_map;

#[cfg(not(test))]
pub mod arch;
pub mod drivers;

pub mod log;
pub mod allocator;

#[cfg(not(test))]
pub mod keyboard;
#[cfg(not(test))]
pub mod mouse;
#[cfg(not(test))]
pub mod ramdisk;
#[cfg(not(test))]
pub mod vfs;
#[cfg(not(test))]
pub mod elf;
#[cfg(not(test))]
pub mod symbols;
#[cfg(not(test))]
pub mod process;
#[cfg(not(test))]
pub mod scheduler;
#[cfg(not(test))]
pub mod clock;
#[cfg(not(test))]
pub mod fd;
#[cfg(not(test))]
pub mod pipe;
#[cfg(not(test))]
pub mod message;
#[cfg(not(test))]
pub mod device;
#[cfg(not(test))]
pub mod user_heap;

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
}

#[repr(C)]
#[derive(Debug)]
pub struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}
