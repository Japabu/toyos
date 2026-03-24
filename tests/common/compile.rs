use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use std::{env, fs, thread};
use wait_timeout::ChildExt;

/// Root of the repository.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Directory containing the TinyCC test cases.
pub fn testcases_dir() -> PathBuf {
    repo_root().join("tests/testcases/tinycc")
}

/// Path to the toyos-libc crate.
pub fn libc_dir() -> PathBuf {
    repo_root().join("userland/libc")
}

/// Build (if needed) and return the toyos libc archive path.
pub fn libc_archive_toyos() -> PathBuf {
    let libc_dir = libc_dir();
    let target = "x86_64-unknown-toyos";
    let target_dir = libc_dir.join("target");
    let archive = target_dir.join(format!("{target}/release/libtoyos_libc.a"));

    // Build if the archive doesn't exist
    if !archive.exists() {
        let mut cmd = Command::new("cargo");
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
pub fn toyos_include_paths() -> Vec<PathBuf> {
    vec![libc_dir().join("include")]
}

/// Include paths for system (host) headers.
fn system_include_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "macos")]
    {
        let sdk = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("xcrun failed");
        let sdk_path = String::from_utf8(sdk.stdout).unwrap();
        paths.push(PathBuf::from(format!("{}/usr/include", sdk_path.trim())));
    }
    #[cfg(target_os = "linux")]
    {
        paths.push(PathBuf::from("/usr/include"));
    }
    paths
}

/// Compile a C test file to object bytes using toyos-cc.
/// Uses system headers for host targets, toyos-libc headers for ToyOS.
/// Returns (main object bytes, companion object bytes).
pub fn compile_c(name: &str, target: Option<&str>) -> (Vec<u8>, Vec<Vec<u8>>) {
    let dir = testcases_dir();
    let c_file = dir.join(format!("{name}.c"));
    let source = fs::read_to_string(&c_file)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", c_file.display()));

    let is_toyos = target.map_or(false, |t| t.contains("toyos"));
    let mut include_paths = if is_toyos {
        toyos_include_paths()
    } else {
        system_include_paths()
    };
    include_paths.push(dir.clone());

    let opts = toyos_cc::CompileOptions {
        include_paths,
        defines: Vec::new(),
        target: target.map(|t| t.to_string()),
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

/// Link object bytes using the system linker and system libc. Returns path to the binary.
fn link_host(obj: &[u8], extra_objs: &[Vec<u8>], name: &str, target: Option<&str>) -> PathBuf {
    let pid = std::process::id();
    let suffix = target.map_or("".to_string(), |t| format!("-{t}"));
    let obj_path = env::temp_dir().join(format!("toyos-host-{name}{suffix}-{pid}.o"));
    fs::write(&obj_path, obj).unwrap();

    let mut extra_paths = Vec::new();
    for (i, extra) in extra_objs.iter().enumerate() {
        let p = env::temp_dir().join(format!("toyos-host-{name}{suffix}-{pid}-extra{i}.o"));
        fs::write(&p, extra).unwrap();
        extra_paths.push(p);
    }

    let bin = env::temp_dir().join(format!("toyos-host-{name}{suffix}-{pid}.bin"));

    let mut cmd = Command::new("cc");
    cmd.arg(&obj_path);
    for p in &extra_paths { cmd.arg(p); }
    cmd.arg("-o").arg(&bin).arg("-lm");
    if let Some(t) = target {
        if t.starts_with("x86_64") {
            cmd.arg("-arch").arg("x86_64");
        }
    }

    let output = cmd.output().unwrap_or_else(|e| panic!("cc failed: {e}"));
    assert!(output.status.success(), "system cc link failed:\n{}", String::from_utf8_lossy(&output.stderr));

    let _ = fs::remove_file(&obj_path);
    for p in &extra_paths { let _ = fs::remove_file(p); }

    bin
}

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Run a command with a timeout. Kills the child if it exceeds the deadline.
fn run_with_timeout(cmd: &mut Command, name: &str) -> Output {
    use std::io::Read;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap_or_else(|e| {
        panic!("failed to run {name}: {e}");
    });
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut r = stdout;
        r.read_to_end(&mut buf).ok();
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        let mut r = stderr;
        r.read_to_end(&mut buf).ok();
        buf
    });
    match child.wait_timeout(TEST_TIMEOUT).expect("wait failed") {
        Some(status) => Output {
            status,
            stdout: stdout_thread.join().unwrap_or_default(),
            stderr: stderr_thread.join().unwrap_or_default(),
        },
        None => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test binary {name} timed out after {}s", TEST_TIMEOUT.as_secs());
        }
    }
}

/// Compile with toyos-cc, link with system cc, run, and check output.
pub fn run_host_test(
    obj: &[u8], extras: &[Vec<u8>], name: &str, args: &[&str], target: Option<&str>,
) {
    let dir = testcases_dir();
    let expect_file = dir.join(format!("{name}.expect"));
    assert!(expect_file.exists(), "missing expect file: {}", expect_file.display());
    let expected = fs::read_to_string(&expect_file).unwrap();

    let bin = link_host(obj, extras, name, target);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let is_x86_64_cross = target.map_or(false, |t| t.starts_with("x86_64"));
    let mut cmd = if is_x86_64_cross {
        let mut c = Command::new("arch");
        c.args(["-x86_64"]).arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    cmd.args(args).current_dir(&dir);

    let run = run_with_timeout(&mut cmd, name);

    let _ = fs::remove_file(&bin);

    let actual = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "test {name} failed with status {}:\nstdout: {}\nstderr: {}",
        run.status,
        actual,
        String::from_utf8_lossy(&run.stderr),
    );

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "output mismatch for {name}\n--- expected ---\n{}\n--- actual ---\n{}",
        expected.trim_end(),
        actual.trim_end(),
    );
}
