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