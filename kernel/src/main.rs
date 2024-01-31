#![no_std]
#![no_main]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // Custom panic handling code goes here
    loop {}
}

#[no_mangle]
pub fn main() {
    loop {}
}


