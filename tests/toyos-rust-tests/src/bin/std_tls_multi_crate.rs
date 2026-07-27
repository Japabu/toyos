fn main() {
    let lib = unsafe { libloading::Library::new("/lib/libtls_multi_crate.so") }
        .expect("failed to dlopen tls-multi-crate");

    let push = unsafe { lib.get::<unsafe extern "C" fn(u8) -> u8>(b"mc_push") }.expect("mc_push");
    let pop = unsafe { lib.get::<unsafe extern "C" fn(u8) -> u8>(b"mc_pop") }.expect("mc_pop");
    let lazy_val = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"mc_lazy_value") }.expect("mc_lazy_value");
    let global_count = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"mc_global_count") }.expect("mc_global_count");
    let dep_tls = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"mc_dep_tls") }.expect("mc_dep_tls");
    let dep_tls_set = unsafe { lib.get::<unsafe extern "C" fn(u64)>(b"mc_dep_tls_set") }.expect("mc_dep_tls_set");

    // Test 1: basic push/pop (like cranelift timing tokens)
    unsafe {
        let prev = push(10);
        assert_eq!(prev, 0, "push(10): prev should be 0 (initial)");
        let prev = push(20);
        assert_eq!(prev, 10, "push(20): prev should be 10");
        let cur = pop(10);
        assert_eq!(cur, 20, "pop(10): current should be 20");
        let cur = pop(0);
        assert_eq!(cur, 10, "pop(0): current should be 10");
    }
    println!("PASS: basic push/pop");

    // Test 2: lazy Box<dyn Trait> TLS works
    unsafe {
        assert_eq!(lazy_val(), 42, "lazy TLS should return 42");
    }
    println!("PASS: lazy Box<dyn Trait> TLS");

    // Test 3: dep global counter works (and doesn't corrupt TLS)
    unsafe {
        let count = global_count();
        // push was called twice above, each bumps global
        assert_eq!(count, 2, "global counter should be 2 after 2 push calls");
    }
    println!("PASS: dep global counter");

    // Test 4: dep TLS works
    unsafe {
        assert_eq!(dep_tls(), 0xBEEF, "dep TLS initial value");
        dep_tls_set(0xCAFE);
        assert_eq!(dep_tls(), 0xCAFE, "dep TLS after set");
    }
    println!("PASS: dep TLS");

    // Test 5: push/pop still works after accessing global + lazy + dep TLS
    // (catches corruption from overlapping symbols)
    unsafe {
        let prev = push(30);
        assert_eq!(prev, 0, "push(30) after all accesses: should be 0 (was restored)");
        let cur = pop(0);
        assert_eq!(cur, 30, "pop(0): should be 30");
    }
    println!("PASS: push/pop after mixed accesses");

    // Test 6: interleaved global bumps + TLS access (stress test for corruption)
    unsafe {
        for i in 0u8..50 {
            let prev = push(i + 1);
            assert_eq!(prev, i, "iteration {}: push({}) prev should be {}", i, i+1, i);
            let _ = global_count(); // touch global between TLS accesses
            let _ = lazy_val();     // touch lazy TLS
        }
        let cur = pop(0);
        assert_eq!(cur, 50, "after 50 pushes: current should be 50");
        // Unwind all the nested pushes (we only popped once, so current is now 0)
    }
    println!("PASS: interleaved stress test");

    println!("all multi-crate TLS tests passed");
}
