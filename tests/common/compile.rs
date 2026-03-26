use std::path::{Path, PathBuf};
use std::{env, fs};

/// Root of the repository.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Directory containing the TinyCC test cases.
pub fn testcases_dir() -> PathBuf {
    repo_root().join("tests/testcases/tinycc")
}

/// Path to the toyos-libc crate.
fn libc_dir() -> PathBuf {
    repo_root().join("userland/libc")
}

/// Build (if needed) and return the toyos libc archive path.
fn libc_archive_toyos() -> PathBuf {
    let libc_dir = libc_dir();
    let target = "x86_64-unknown-toyos";
    let target_dir = libc_dir.join("target");
    let archive = target_dir.join(format!("{target}/release/libtoyos_libc.a"));

    if !archive.exists() {
        let mut cmd = std::process::Command::new("cargo");
        for (key, _) in env::vars() {
            if key.starts_with("CARGO") || key == "RUSTC" || key == "RUSTFLAGS" {
                cmd.env_remove(&key);
            }
        }
        let output = cmd
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .args(["rustc", "--release", "--target", target, "--crate-type", "staticlib"])
            .arg("--manifest-path")
            .arg(libc_dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir)
            .output()
            .unwrap_or_else(|e| panic!("failed to run cargo for toyos-libc: {e}"));
        assert!(
            output.status.success(),
            "toyos-libc build failed:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }

    assert!(archive.exists(), "expected staticlib at {}", archive.display());
    archive
}

/// Include paths for toyos-libc headers.
fn toyos_include_paths() -> Vec<PathBuf> {
    vec![libc_dir().join("include")]
}

/// Compile a C test file to object bytes using toyos-cc for ToyOS.
/// Returns (main object bytes, companion object bytes).
pub fn compile_c(name: &str) -> (Vec<u8>, Vec<Vec<u8>>) {
    let dir = testcases_dir();
    let c_file = dir.join(format!("{name}.c"));
    let source = fs::read_to_string(&c_file)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", c_file.display()));

    let mut include_paths = toyos_include_paths();
    include_paths.push(dir.clone());

    let opts = toyos_cc::CompileOptions {
        include_paths,
        defines: Vec::new(),
        target: Some("x86_64-unknown-toyos".to_string()),
        opt_level: 0,
    };

    let obj = toyos_cc::compile(&source, &format!("{name}.c"), &opts);

    // Compile companion files (e.g., "104+_inline.c" for "104_inline")
    let mut extras = Vec::new();
    if let Some(idx) = name.find('_') {
        let prefix = &name[..idx];
        let file_suffix = &name[idx..];
        let companion_name = format!("{}+{}.c", prefix, file_suffix);
        let companion = dir.join(&companion_name);
        if companion.exists() {
            let companion_source = fs::read_to_string(&companion)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", companion.display()));
            let extra = toyos_cc::compile(&companion_source, &companion_name, &opts);
            extras.push(extra);
        }
    }

    (obj, extras)
}

/// Link object bytes as a PIE ELF for ToyOS. Returns the linked binary bytes.
pub fn link_toyos(obj: &[u8], extra_objs: &[Vec<u8>], name: &str) -> Vec<u8> {
    let libc_path = libc_archive_toyos();
    let lib_dir = libc_path.parent().unwrap().to_path_buf();

    let pid = std::process::id();
    let obj_path = env::temp_dir().join(format!("toyos-test-{name}-{pid}.o"));
    fs::write(&obj_path, obj).unwrap();

    let mut inputs: Vec<PathBuf> = vec![obj_path.clone()];
    let mut extra_paths = Vec::new();
    for (i, extra) in extra_objs.iter().enumerate() {
        let p = env::temp_dir().join(format!("toyos-test-{name}-{pid}-extra{i}.o"));
        fs::write(&p, extra).unwrap();
        inputs.push(p.clone());
        extra_paths.push(p);
    }

    let objects = toyos_ld::resolve_libs_with_entry(
        &inputs,
        &[lib_dir],
        &["toyos_libc".to_string()],
        Some("_start"),
    )
    .unwrap_or_else(|e| panic!("resolve_libs failed: {e}"));

    let _ = fs::remove_file(&obj_path);
    for p in &extra_paths {
        let _ = fs::remove_file(p);
    }

    toyos_ld::link_full(&objects, "_start", true, false)
        .unwrap_or_else(|e| panic!("toyos-ld link failed: {e}"))
}
