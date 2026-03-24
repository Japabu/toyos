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

    Mutex::new(QemuInstance::boot(&[], &rust_bins))
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
                        Duration::from_secs(30),
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
    std_mmap,
    std_process,
    std_sync,
    std_threading,
    std_tls,
    std_tls_dlopen,
    std_tls_multi_crate,
    std_tls_cranelift,
    std_unwind,
);
