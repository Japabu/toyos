use std::cell::Cell;
use std::sync::{Arc, Barrier};

thread_local! {
    static EXE_VALUE: Cell<u64> = const { Cell::new(42) };
}

fn main() {
    // Test 1: exe TLS works before dlopen
    EXE_VALUE.with(|v| {
        assert_eq!(v.get(), 42, "exe TLS initial value");
        v.set(100);
    });
    println!("PASS: exe TLS before dlopen");

    // Test 2: dlopen a library with TLS
    let lib = unsafe { libloading::Library::new("/lib/libtls_dlopen_lib.so") }
        .expect("failed to dlopen tls-dlopen-lib");

    let get_a = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_get_a") }.expect("dl_tls_get_a");
    let get_b = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_get_b") }.expect("dl_tls_get_b");
    let inc_a = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_increment_a") }.expect("dl_tls_increment_a");
    let inc_b = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_increment_b") }.expect("dl_tls_increment_b");
    let get_label = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_get_label") }.expect("dl_tls_get_label");
    let set_label = unsafe { lib.get::<unsafe extern "C" fn(u64)>(b"dl_tls_set_label") }.expect("dl_tls_set_label");
    let check_buffer = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_check_buffer") }.expect("dl_tls_check_buffer");
    let scope_push = unsafe { lib.get::<unsafe extern "C" fn(u32) -> u32>(b"dl_tls_scope_push") }.expect("scope_push");
    let scope_pop = unsafe { lib.get::<unsafe extern "C" fn(u32) -> u32>(b"dl_tls_scope_pop") }.expect("scope_pop");

    // Test 3: initial values correct
    unsafe {
        assert_eq!(get_a(), 0, "dlopen TLS counter_a initial");
        assert_eq!(get_b(), 0, "dlopen TLS counter_b initial");
        assert_eq!(get_label(), 0xDEAD_BEEF, "dlopen TLS label initial");
        assert_eq!(check_buffer(), 1, "dlopen TLS buffer initial (all 0xAB)");
    }
    println!("PASS: dlopen TLS initial values");

    // Test 4: counters are independent (not aliased)
    unsafe {
        assert_eq!(inc_a(), 1);
        assert_eq!(inc_a(), 2);
        assert_eq!(inc_a(), 3);
        assert_eq!(get_b(), 0, "counter_b not affected by counter_a");
        assert_eq!(inc_b(), 1);
        assert_eq!(get_a(), 3, "counter_a not affected by counter_b");
    }
    println!("PASS: dlopen TLS counters independent");

    // Test 5: label independent from counters
    unsafe {
        set_label(0xCAFE_BABE);
        assert_eq!(get_label(), 0xCAFE_BABE);
    }
    println!("PASS: dlopen TLS label independent");

    // Test 6: exe TLS not corrupted
    EXE_VALUE.with(|v| {
        assert_eq!(v.get(), 100, "exe TLS not corrupted after dlopen TLS ops");
    });
    println!("PASS: exe TLS not corrupted");

    // Test 7: nested RAII scoping
    unsafe {
        let prev0 = scope_push(1);
        assert_eq!(prev0, 0, "scope_push(1): prev should be 0");
        let prev1 = scope_push(2);
        assert_eq!(prev1, 1, "scope_push(2): prev should be 1");
        let cur = scope_pop(1);
        assert_eq!(cur, 2, "scope_pop(1): current should be 2");
        let cur = scope_pop(0);
        assert_eq!(cur, 1, "scope_pop(0): current should be 1");
    }
    println!("PASS: nested RAII scoping");

    // Test 8: cross-thread TLS isolation — sequential
    let inc_a_ptr = *inc_a as usize;
    let get_a_ptr = *get_a as usize;
    let get_b_ptr = *get_b as usize;

    let thread = std::thread::spawn(move || {
        let inc_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(inc_a_ptr) };
        let get_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(get_a_ptr) };
        let get_b: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(get_b_ptr) };
        unsafe {
            assert_eq!(get_a(), 0, "child: counter_a should start at 0");
            assert_eq!(get_b(), 0, "child: counter_b should start at 0");
            assert_eq!(inc_a(), 1, "child: first inc_a");
            assert_eq!(inc_a(), 2, "child: second inc_a");
        }
    });
    thread.join().unwrap();
    unsafe {
        assert_eq!(get_a(), 3, "main: counter_a unchanged after child");
        assert_eq!(get_b(), 1, "main: counter_b unchanged after child");
    }
    println!("PASS: cross-thread TLS isolation (sequential)");

    // Test 9: concurrent cross-thread TLS isolation
    let barrier = Arc::new(Barrier::new(2));
    let inc_a_ptr = *inc_a as usize;
    let get_a_ptr = *get_a as usize;
    let b = barrier.clone();

    let thread = std::thread::spawn(move || {
        let inc_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(inc_a_ptr) };
        let get_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(get_a_ptr) };
        b.wait();
        unsafe {
            for _ in 0..1000 { inc_a(); }
            get_a()
        }
    });
    barrier.wait();
    unsafe { for _ in 0..1000 { inc_a(); } }
    let child_final = thread.join().unwrap();
    let main_final = unsafe { get_a() };
    assert_eq!(main_final, 1003, "main: counter_a should be 1003 (3 + 1000)");
    assert_eq!(child_final, 1000, "child: counter_a should be 1000 (fresh TLS)");
    println!("PASS: concurrent cross-thread TLS isolation");

    // Test 10: thread spawned BEFORE dlopen accesses dlopen'd TLS
    // This is how rustc works: rayon pool threads exist before cranelift is dlopen'd.
    // Drop the current library first so we can test the full sequence.
    drop(lib);

    let barrier2 = Arc::new(Barrier::new(2));
    let b2 = barrier2.clone();

    // Spawn thread BEFORE dlopen
    let thread = std::thread::spawn(move || {
        // Wait for main thread to dlopen
        b2.wait();
        // Now access TLS from a library that was dlopen'd after this thread was created
        let inc_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(inc_a_ptr) };
        let get_a: unsafe extern "C" fn() -> u64 = unsafe { core::mem::transmute(get_a_ptr) };
        unsafe {
            assert_eq!(get_a(), 0, "pre-spawn thread: counter_a should be 0");
            assert_eq!(inc_a(), 1);
            assert_eq!(inc_a(), 2);
            get_a()
        }
    });

    // Dlopen again (thread is waiting at barrier)
    let lib2 = unsafe { libloading::Library::new("/lib/libtls_dlopen_lib.so") }
        .expect("failed to dlopen tls-dlopen-lib (second time)");
    let get_a2 = unsafe { lib2.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_get_a") }.expect("dl_tls_get_a");
    let inc_a2 = unsafe { lib2.get::<unsafe extern "C" fn() -> u64>(b"dl_tls_increment_a") }.expect("dl_tls_increment_a");

    // Signal thread to go
    barrier2.wait();

    // Main thread also uses TLS from the re-opened library (fresh TLS for this handle)
    let main_val = unsafe { get_a2() };
    println!("  main counter_a after re-dlopen: {}", main_val);
    unsafe {
        for _ in 0..100 { inc_a2(); }
    }
    let main_final2 = unsafe { get_a2() };
    let child_final2 = thread.join().unwrap();

    // Child should have independent TLS
    assert_eq!(child_final2, 2, "pre-spawn thread: counter_a final = 2");
    println!("  main counter_a final: {}, child final: {}", main_final2, child_final2);
    println!("PASS: thread spawned BEFORE dlopen accesses dlopen'd TLS");

    println!("all dlopen TLS tests passed");
}
