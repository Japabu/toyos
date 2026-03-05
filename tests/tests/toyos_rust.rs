use std::path::Path;
use toyos_tests::compile;
use toyos_tests::qemu;

#[test]
#[ignore] // Run with: cargo test --test toyos_rust -- --ignored
fn toyos_rust_tests() {
    let repo = compile::repo_root();
    let rust_tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("toyos-rust-tests");

    if !rust_tests_dir.join("Cargo.toml").exists() {
        eprintln!("No toyos-rust-tests crate found, skipping");
        return;
    }

    // Build all Rust test binaries for ToyOS
    let toyos_ld = qemu_toyos_ld(&repo);
    let rust_bins = qemu::build_toyos_bins_public(&rust_tests_dir, &toyos_ld);

    if rust_bins.is_empty() {
        eprintln!("No Rust test binaries found, skipping");
        return;
    }

    // Run all tests in QEMU
    let session = qemu::run_qemu_tests(&[], &rust_bins);

    // Verify results — assert-based, exit 0 = pass
    let mut failures = Vec::new();
    let total = session.results.len();
    let mut passed = 0usize;

    for result in &session.results {
        let test_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

        if let Some(err) = &result.error {
            eprintln!("FAIL {test_name}: error: {err}");
            failures.push(format!("{test_name}: error: {err}"));
            continue;
        }

        match result.exit_code {
            Some(0) => {
                eprintln!("PASS {test_name}");
                passed += 1;
            }
            Some(code) => {
                eprintln!("FAIL {test_name}: exited with code {code}");
                failures.push(format!(
                    "{test_name}: exited with code {code}\nstdout:\n{}",
                    result.stdout
                ));
            }
            None => {
                eprintln!("FAIL {test_name}: no exit code");
                failures.push(format!("{test_name}: no exit code\nstdout:\n{}", result.stdout));
            }
        }
    }

    eprintln!("\n{passed}/{total} Rust tests passed");

    if !failures.is_empty() {
        panic!(
            "{} Rust test(s) failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}

fn qemu_toyos_ld(repo: &Path) -> std::path::PathBuf {
    let toyos_ld_dir = repo.join("userland/toyos-ld");
    let host = compile::host_target();
    let path = toyos_ld_dir.join(format!("target/{host}/release/toyos-ld"));
    if path.exists() {
        return path;
    }
    // Build it
    assert!(
        std::process::Command::new("cargo")
            .args(["build", "--release", "--target", host])
            .current_dir(&toyos_ld_dir)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build toyos-ld"
    );
    path.canonicalize().expect("toyos-ld binary not found")
}
