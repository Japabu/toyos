use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
mod common;
use common::qemu::{self, QemuInstance, TestResult};

static QEMU: LazyLock<Mutex<QemuInstance>> = LazyLock::new(|| {
    let rust_tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/toyos-rust-tests");

    assert!(
        rust_tests_dir.join("Cargo.toml").exists(),
        "No toyos-rust-tests crate found"
    );

    let rust_bins = qemu::build_toyos_bins(&rust_tests_dir);

    assert!(!rust_bins.is_empty(), "No Rust test binaries found");

    Mutex::new(QemuInstance::boot(&rust_tests_dir, &[], &rust_bins))
});

fn check_test_result(result: &TestResult) {
    let test_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        panic!("FAIL {test_name}: {err}");
    }

    match result.exit_code {
        Some(0) => {}
        Some(code) => panic!(
            "FAIL {test_name}: exited with code {code}\nstdout:\n{}",
            result.stdout
        ),
        None => panic!(
            "FAIL {test_name}: no exit code\nstdout:\n{}",
            result.stdout
        ),
    }
}

macro_rules! toyos_rust_tests {
    ($($name:ident),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                let result = {
                    let mut qemu = QEMU.lock().unwrap_or_else(|e| e.into_inner());
                    qemu.run_test(
                        &format!("test_rs_{}", stringify!($name)),
                        Duration::from_secs(5),
                    )
                };
                check_test_result(&result);
            }
        )*
    };
}

toyos_rust_tests!(
    allocator_stress,
    demand_paging_sse,
    std_alloc,
    std_fs,
    std_fs_write,
    std_io,
    mmap_stress,
    std_mmap,
    std_process,
    std_sync,
    std_threading,
    std_tls,
    std_tls_dlopen,
    std_tls_multi_crate,
    std_tls_cranelift,
    std_unwind,
    std_unwind_so,
);

#[test]
fn panic_recovery() {
    let result = {
        let mut qemu = QEMU.lock().unwrap_or_else(|e| e.into_inner());
        qemu.run_test(
            "test_rs_panic_recovery",
            Duration::from_secs(10),
        )
    };
    check_test_result(&result);
}

/// Verify that panic_recovery produces proper diagnostics in serial output.
/// Checks all three fault paths: syscall panic, kernel fault, user segfault.
#[test]
fn panic_recovery_diagnostics() {
    let result = {
        let mut qemu = QEMU.lock().unwrap_or_else(|e| e.into_inner());
        qemu.run_test(
            "test_rs_panic_recovery",
            Duration::from_secs(5),
        )
    };
    check_test_result(&result);

    // 1. Syscall panic: PANIC header + kernel backtrace + syscall context + user backtrace
    assert!(result.serial.contains("!!! PANIC !!!"),
        "expected PANIC header\nstdout:\n{}", result.serial);
    assert!(result.serial.contains("SYS_DEBUG"),
        "expected SYS_DEBUG in panic message\nstdout:\n{}", result.serial);
    assert!(result.serial.contains("Syscall: num=92"),
        "expected syscall context in panic report\nstdout:\n{}", result.serial);
    assert!(result.serial.contains("User backtrace:"),
        "expected user backtrace in panic report\nstdout:\n{}", result.serial);

    // 2. Kernel fault during syscall: classified as user fault (cr2=0 is in user address range)
    //    Produces SEGFAULT with register dump and backtrace
    assert!(result.serial.contains("Registers:"),
        "expected register dump from kernel fault\nstdout:\n{}", result.serial);

    // 3. User segfault: SEGFAULT with symbolized backtrace
    assert!(result.serial.contains("SEGFAULT tid="),
        "expected SEGFAULT header\nstdout:\n{}", result.serial);
    assert!(result.serial.contains("deliberate_null_deref"),
        "expected deliberate_null_deref in segfault backtrace\nstdout:\n{}", result.serial);

    // All three should have symbolized backtraces (name+0xoffset)
    assert!(result.serial.contains("+0x"),
        "expected symbolized backtraces\nstdout:\n{}", result.serial);
}
