use std::path::Path;
use std::time::Duration;
use toyos_tests::compile;
use toyos_tests::qemu::{self, QemuInstance};

#[test]
fn toyos_rust_tests() {
    let repo = compile::repo_root();
    let rust_tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("toyos-rust-tests");

    if !rust_tests_dir.join("Cargo.toml").exists() {
        eprintln!("No toyos-rust-tests crate found, skipping");
        return;
    }

    let toyos_ld = qemu_toyos_ld(&repo);
    let rust_bins = qemu::build_toyos_bins(&rust_tests_dir, &toyos_ld);

    if rust_bins.is_empty() {
        eprintln!("No Rust test binaries found, skipping");
        return;
    }

    let mut qemu = QemuInstance::boot(&[], &rust_bins);

    let mut failures = Vec::new();
    let mut passed = 0usize;
    let test_bins: Vec<_> = rust_bins.iter().filter(|(name, _)| !name.ends_with(".so")).collect();
    let total = test_bins.len();

    for (name, _) in &test_bins {
        let test_name = format!("test_rs_{name}");
        let result = qemu.run_test(&test_name, Duration::from_secs(30));
        let display_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

        if let Some(err) = &result.error {
            eprintln!("FAIL {display_name}: error: {err}");
            failures.push(format!("{display_name}: error: {err}"));
            continue;
        }

        match result.exit_code {
            Some(0) => {
                eprintln!("PASS {display_name}");
                passed += 1;
            }
            Some(code) => {
                eprintln!("FAIL {display_name}: exited with code {code}");
                failures.push(format!(
                    "{display_name}: exited with code {code}\nstdout:\n{}",
                    result.stdout
                ));
            }
            None => {
                eprintln!("FAIL {display_name}: no exit code");
                failures.push(format!("{display_name}: no exit code\nstdout:\n{}", result.stdout));
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
