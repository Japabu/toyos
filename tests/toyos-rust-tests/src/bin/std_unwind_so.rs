fn main() {
    let lib = unsafe { libloading::Library::new("/lib/libtls_dlopen_lib.so") }
        .expect("failed to dlopen tls-dlopen-lib");

    // Test 1: catch_unwind works inside a cdylib .so
    let f = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_test_catch_unwind") }
        .expect("dl_test_catch_unwind");
    let result = unsafe { f() };
    assert_eq!(result, 1, "catch_unwind in .so failed");
    println!("PASS: catch_unwind in .so");

    // Test 2: Drop runs during unwind inside a cdylib .so
    let f = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_test_drop_during_unwind") }
        .expect("dl_test_drop_during_unwind");
    let result = unsafe { f() };
    assert_eq!(result, 1, "Drop during unwind in .so failed");
    println!("PASS: Drop during unwind in .so");

    // Test 3: catch_unwind on a thread spawned from the .so
    let f = unsafe { lib.get::<unsafe extern "C" fn() -> u64>(b"dl_test_thread_unwind") }
        .expect("dl_test_thread_unwind");
    let result = unsafe { f() };
    assert_eq!(result, 1, "thread unwind in .so failed");
    println!("PASS: thread unwind in .so");

    println!("all .so unwind tests passed");
}
