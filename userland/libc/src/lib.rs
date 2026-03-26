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
            let _ = toyos_abi::syscall::write(toyos_abi::Fd(2), s.as_bytes());
            Ok(())
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo) -> ! {
        use core::fmt::Write;
        let _ = write!(Stderr, "libc panic: {info}\n");
        toyos_abi::syscall::exit(134) // SIGABRT-like
    }

    // Unwinding stubs — core references these symbols via .eh_frame, but with
    // panic=abort the unwinding path is never taken. Provide no-op/abort stubs
    // to satisfy the linker.
    #[unsafe(no_mangle)]
    extern "C" fn rust_eh_personality() {}

    #[unsafe(no_mangle)]
    extern "C" fn _Unwind_Resume() -> ! {
        toyos_abi::syscall::exit(134)
    }

    struct MmapAllocator;

    unsafe impl dlmalloc::Allocator for MmapAllocator {
        fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
            use toyos_abi::syscall::{MmapProt, MmapFlags};
            let ptr = unsafe {
                toyos_abi::syscall::mmap(
                    core::ptr::null_mut(),
                    size,
                    MmapProt::READ | MmapProt::WRITE,
                    MmapFlags::ANONYMOUS,
                )
            };
            if ptr.is_null() { (core::ptr::null_mut(), 0, 0) } else { (ptr, size, 0) }
        }

        fn remap(&self, _ptr: *mut u8, _old: usize, _new: usize, _can_move: bool) -> *mut u8 {
            core::ptr::null_mut()
        }

        fn free_part(&self, _ptr: *mut u8, _old: usize, _new: usize) -> bool {
            false
        }

        fn free(&self, ptr: *mut u8, size: usize) -> bool {
            unsafe { toyos_abi::syscall::munmap(ptr, size).is_ok() }
        }

        fn can_release_part(&self, _flags: u32) -> bool {
            false
        }

        fn allocates_zeros(&self) -> bool {
            true
        }

        fn page_size(&self) -> usize {
            0x1000
        }
    }

    struct SyncDlmalloc(core::cell::UnsafeCell<dlmalloc::Dlmalloc<MmapAllocator>>);
    unsafe impl Sync for SyncDlmalloc {}

    static DLMALLOC: SyncDlmalloc =
        SyncDlmalloc(core::cell::UnsafeCell::new(dlmalloc::Dlmalloc::new_with_allocator(MmapAllocator)));
    static LOCKED: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

    fn lock() -> DropLock {
        while LOCKED.swap(1, core::sync::atomic::Ordering::Acquire) != 0 {
            core::hint::spin_loop();
        }
        DropLock
    }

    struct DropLock;
    impl Drop for DropLock {
        fn drop(&mut self) {
            LOCKED.store(0, core::sync::atomic::Ordering::Release);
        }
    }

    struct LibcAllocator;

    #[global_allocator]
    static ALLOCATOR: LibcAllocator = LibcAllocator;

    unsafe impl core::alloc::GlobalAlloc for LibcAllocator {
        unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
            let _lock = lock();
            unsafe { (*DLMALLOC.0.get()).malloc(layout.size(), layout.align()) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
            let _lock = lock();
            unsafe { (*DLMALLOC.0.get()).free(ptr, layout.size(), layout.align()) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: core::alloc::Layout, new_size: usize) -> *mut u8 {
            let _lock = lock();
            unsafe { (*DLMALLOC.0.get()).realloc(ptr, layout.size(), layout.align(), new_size) }
        }
    }
}
