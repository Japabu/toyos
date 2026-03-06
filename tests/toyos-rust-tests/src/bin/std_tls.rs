use std::cell::Cell;

// Functions from tls-lib (loaded as shared library via DT_NEEDED)
#[link(name = "tls_lib")]
extern "C" {
    fn tls_increment() -> u64;
    fn tls_get_counter() -> u64;
    fn tls_get_label() -> u64;
    fn tls_set_label(val: u64);
}

// TLS in the main executable
thread_local! {
    static LOCAL_VALUE: Cell<u64> = const { Cell::new(42) };
}

fn main() {
    // Test 1: exe-local thread_local works
    LOCAL_VALUE.with(|v| {
        assert_eq!(v.get(), 42, "exe TLS initial value");
        v.set(100);
        assert_eq!(v.get(), 100, "exe TLS after set");
    });
    println!("PASS: exe thread_local");

    // Test 2: shared library TLS works
    unsafe {
        assert_eq!(tls_get_counter(), 0, "lib TLS initial counter");
        assert_eq!(tls_increment(), 1, "lib TLS first increment");
        assert_eq!(tls_increment(), 2, "lib TLS second increment");
        assert_eq!(tls_get_counter(), 2, "lib TLS counter after increments");
    }
    println!("PASS: lib thread_local counter");

    // Test 3: shared library TLS doesn't alias exe TLS
    unsafe {
        assert_eq!(tls_get_label(), 0xDEAD_BEEF, "lib TLS initial label");
        tls_set_label(0xCAFE_BABE);
        assert_eq!(tls_get_label(), 0xCAFE_BABE, "lib TLS label after set");
    }
    // Verify exe TLS wasn't corrupted
    LOCAL_VALUE.with(|v| {
        assert_eq!(v.get(), 100, "exe TLS not corrupted by lib TLS ops");
    });
    println!("PASS: TLS isolation between exe and lib");

    println!("all TLS tests passed");
}
