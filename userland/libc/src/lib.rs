#![no_std]
#![feature(c_variadic)]

extern crate alloc;

mod ctype;
mod math;
mod memory;
mod misc;
mod posix_io;
mod printf;
mod pthread;
mod socket;
mod stdio;
mod string;
mod time;

// C runtime: _start entry point, panic handler, and global allocator.
// Only for pure C programs (no Rust std). When linked into a Rust program
// with std, std provides these.
#[cfg(not(feature = "std-runtime"))]
mod runtime {
    use core::panic::PanicInfo;

    // Entry point for C programs. The kernel pushes argc and argv onto the stack.
    #[unsafe(no_mangle)]
    #[unsafe(naked)]
    unsafe extern "C" fn _start() -> ! {
        // Stack layout at entry (set up by kernel):
        //   [RSP]   = argc
        //   [RSP+8] = argv[0], argv[1], ..., NULL
        core::arch::naked_asm!(
            "mov rdi, [rsp]",      // argc
            "lea rsi, [rsp + 8]",  // argv
            "call {start_c}",
            "ud2",
            start_c = sym start_c,
        );
    }

    extern "C" fn start_c(argc: i32, argv: *const *const u8) -> ! {
        unsafe extern "C" {
            fn main(argc: i32, argv: *const *const u8) -> i32;
        }
        let code = unsafe { main(argc, argv) };
        toyos_abi::syscall::exit(code)
    }

    struct Stderr;

    impl core::fmt::Write for Stderr {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let _ = toyos_abi::syscall::write(toyos_abi::syscall::Fd(2), s.as_bytes());
            Ok(())
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo) -> ! {
        use core::fmt::Write;
        let _ = write!(Stderr, "libc panic: {info}\n");
        toyos_abi::syscall::exit(134) // SIGABRT-like
    }

    struct LibcAllocator;

    #[global_allocator]
    static ALLOCATOR: LibcAllocator = LibcAllocator;

    unsafe impl core::alloc::GlobalAlloc for LibcAllocator {
        unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
            // SAFETY: layout constraints (non-zero size, valid alignment) upheld by GlobalAlloc contract
            unsafe { toyos_abi::syscall::alloc(layout.size(), layout.align()) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
            // SAFETY: ptr was returned by alloc with the same layout
            unsafe { toyos_abi::syscall::free(ptr, layout.size(), layout.align()) };
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: core::alloc::Layout, new_size: usize) -> *mut u8 {
            // SAFETY: ptr was returned by alloc with the same layout
            unsafe { toyos_abi::syscall::realloc(ptr, layout.size(), layout.align(), new_size) }
        }
    }
}
