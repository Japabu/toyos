#![cfg_attr(not(test), no_std)]
#![feature(allocator_api)]
extern crate alloc;

pub mod log;

#[cfg(not(test))]
pub mod serial;
pub mod acpi;
pub mod pci;
pub mod nvme;
pub mod allocator;

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
}

#[repr(C)]
#[derive(Debug)]
pub struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}
