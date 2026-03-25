use std::cell::Cell;

// Multiple thread_local variables at different offsets to test for aliasing.
thread_local! {
    static COUNTER_A: Cell<u64> = const { Cell::new(0) };
    static COUNTER_B: Cell<u64> = const { Cell::new(0) };
    static LABEL: Cell<u64> = const { Cell::new(0xDEAD_BEEF) };
    static BUFFER: Cell<[u8; 64]> = const { Cell::new([0xAB; 64]) };
    // Mimics cranelift's CURRENT_PASS: a Cell that tracks nested RAII scope
    static CURRENT_SCOPE: Cell<u32> = const { Cell::new(0) };
}

#[no_mangle]
pub extern "C" fn dl_tls_increment_a() -> u64 {
    COUNTER_A.with(|c| { let v = c.get() + 1; c.set(v); v })
}

#[no_mangle]
pub extern "C" fn dl_tls_increment_b() -> u64 {
    COUNTER_B.with(|c| { let v = c.get() + 1; c.set(v); v })
}

#[no_mangle]
pub extern "C" fn dl_tls_get_a() -> u64 {
    COUNTER_A.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn dl_tls_get_b() -> u64 {
    COUNTER_B.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn dl_tls_get_label() -> u64 {
    LABEL.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn dl_tls_set_label(val: u64) {
    LABEL.with(|c| c.set(val));
}

#[no_mangle]
pub extern "C" fn dl_tls_check_buffer() -> u64 {
    BUFFER.with(|c| {
        let buf = c.get();
        if buf.iter().all(|&b| b == 0xAB) { 1 } else { 0 }
    })
}

/// Push a scope ID, returns the previous scope ID.
/// Mimics cranelift timing: start_pass(X) → CURRENT_PASS.replace(X)
#[no_mangle]
pub extern "C" fn dl_tls_scope_push(scope_id: u32) -> u32 {
    CURRENT_SCOPE.with(|c| c.replace(scope_id))
}

/// Pop a scope ID (restore previous). Returns what CURRENT_SCOPE was.
/// Mimics cranelift timing: Drop → CURRENT_PASS.replace(prev)
#[no_mangle]
pub extern "C" fn dl_tls_scope_pop(restore_to: u32) -> u32 {
    CURRENT_SCOPE.with(|c| c.replace(restore_to))
}

/// Test catch_unwind inside a cdylib .so — verifies the .so's unwinding crate
/// has its EH frame finder registered (via .init_array constructor).
#[no_mangle]
pub extern "C" fn dl_test_catch_unwind() -> u64 {
    let result = std::panic::catch_unwind(|| {
        panic!("intentional panic from .so");
    });
    if result.is_err() { 1 } else { 0 }
}

/// Test that Drop runs during unwind inside a cdylib .so.
#[no_mangle]
pub extern "C" fn dl_test_drop_during_unwind() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static DROP_COUNT: AtomicU64 = AtomicU64::new(0);
    DROP_COUNT.store(0, Ordering::SeqCst);

    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    let result = std::panic::catch_unwind(|| {
        let _g1 = Guard;
        let _g2 = Guard;
        panic!("unwind with guards");
    });
    if result.is_err() && DROP_COUNT.load(Ordering::SeqCst) == 2 { 1 } else { 0 }
}

/// Test catch_unwind on a thread spawned from inside the .so.
#[no_mangle]
pub extern "C" fn dl_test_thread_unwind() -> u64 {
    let handle = std::thread::spawn(|| {
        std::panic::catch_unwind(|| {
            panic!("panic on .so thread");
        }).is_err()
    });
    match handle.join() {
        Ok(true) => 1,
        _ => 0,
    }
}
