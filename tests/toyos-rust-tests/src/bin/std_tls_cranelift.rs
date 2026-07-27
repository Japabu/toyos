fn main() {
    let lib = unsafe { libloading::Library::new("/lib/libtls_cranelift.so") }
        .expect("failed to dlopen tls-cranelift");

    let compile = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"cl_compile_trivial") }
        .expect("cl_compile_trivial");

    println!("Compiling trivial function with cranelift...");
    let result = unsafe { compile() };
    assert_eq!(result, 0, "cranelift compilation failed with code {}", result);
    println!("PASS: cranelift compiled trivial function");

    // Compile again to test repeated timing token usage
    let result = unsafe { compile() };
    assert_eq!(result, 0, "second cranelift compilation failed");
    println!("PASS: second cranelift compilation");

    // Test 3: basic thread spawn from .so
    let thread_test = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"cl_thread_test") }
        .expect("cl_thread_test");
    println!("Testing thread spawn from .so...");
    let result = unsafe { thread_test() };
    assert_eq!(result, 42, "thread spawn from .so failed: {}", result);
    println!("PASS: thread spawn from .so");

    // Test 4: compile with profiler swap (mimics rustc pattern)
    println!("Starting profiler swap test...");
    let compile_swap = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"cl_compile_with_profiler_swap") }
        .expect("cl_compile_with_profiler_swap");
    println!("Calling cl_compile_with_profiler_swap...");
    let result = unsafe { compile_swap() };
    println!("cl_compile_with_profiler_swap returned: {}", result);
    assert_eq!(result, 0, "compile with profiler swap failed with code {}", result);
    println!("PASS: compile with profiler swap");

    println!("all cranelift TLS tests passed");
}
