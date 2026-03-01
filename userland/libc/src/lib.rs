#![no_std]
#![allow(non_camel_case_types)]

// This is a stub. Actual implementations will come later.
// For now we just need the symbols to exist for linking.

mod panic {
    use core::panic::PanicInfo;
    #[panic_handler]
    fn panic(_info: &PanicInfo) -> ! {
        loop {}
    }
}
