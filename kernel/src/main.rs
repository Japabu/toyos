#![no_std]
#![no_main]

mod serial;

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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Custom panic handling code goes here
    loop {}
}

#[no_mangle]
pub extern "sysv64" fn _start(_kernel_args: KernelArgs) -> ! {
    let x = "Hello from Kernel!";

    serial::init_serial();
    serial::println(x);

    loop {}
}
