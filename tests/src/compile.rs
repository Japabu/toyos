use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use std::{env, fs, thread};
use wait_timeout::ChildExt;

/// Root of the repository.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Directory containing the TinyCC test cases.
pub fn testcases_dir() -> PathBuf {
    repo_root().join("tests/testcases/tinycc")
}

/// Path to the toyos-libc crate.
pub fn libc_dir() -> PathBuf {
    repo_root().join("userland/libc")
}

/// Get the pre-built libc archive for the given target.
/// Archives are built during `cargo test` compilation (build.rs).
pub fn libc_archive(target: &str) -> PathBuf {
    let path = if target.contains("toyos") {
        PathBuf::from(env!("TOYOS_LIBC_TOYOS"))
    } else if target == "x86_64-apple-darwin" && cfg!(target_arch = "aarch64") {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        { PathBuf::from(env!("TOYOS_LIBC_X86_64_APPLE")) }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        { panic!("x86_64-apple-darwin libc not available on this platform") }
    } else {
        PathBuf::from(env!("TOYOS_LIBC_HOST"))
    };
    assert!(path.exists(), "pre-built libc not found at {}", path.display());
    path
}

/// Detect the host target triple.
pub fn host_target() -> &'static str {
    if cfg!(target_arch = "aarch64") && cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "macos") {
        "x86_64-apple-darwin"
    } else if cfg!(target_arch = "x86_64") && cfg!(target_os = "linux") {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(target_arch = "aarch64") && cfg!(target_os = "linux") {
        "aarch64-unknown-linux-gnu"
    } else {
        panic!("unsupported host")
    }
}

/// Include paths for toyos-libc headers.
pub fn libc_include_paths() -> Vec<PathBuf> {
    vec![libc_dir().join("include")]
}

/// Compile a C test file to object bytes using toyos-cc as a library.
/// Returns (main object bytes, companion object bytes).
pub fn compile_c(name: &str, target: Option<&str>) -> (Vec<u8>, Vec<Vec<u8>>) {
    let dir = testcases_dir();
    let c_file = dir.join(format!("{name}.c"));
    let source = fs::read_to_string(&c_file)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", c_file.display()));

    let mut include_paths = libc_include_paths();
    include_paths.push(dir.clone());

    let opts = toyos_cc::CompileOptions {
        include_paths,
        defines: Vec::new(),
        target: target.map(|t| t.to_string()),
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

/// Link object bytes for host execution. Returns the linked binary bytes.
pub fn link_host(obj: &[u8], extra_objs: &[Vec<u8>], name: &str, target: Option<&str>) -> Vec<u8> {
    let label = target.unwrap_or("native");
    let libc_target = target.unwrap_or(host_target());
    let libc_path = libc_archive(libc_target);
    let libc_data = fs::read(&libc_path).unwrap();

    let mut objects: Vec<(String, Vec<u8>)> = vec![(format!("{name}.o"), obj.to_vec())];
    for (i, extra) in extra_objs.iter().enumerate() {
        objects.push((format!("{name}-extra{i}.o"), extra.clone()));
    }
    objects.push((libc_path.display().to_string(), libc_data));

    let is_macho = target.map_or(cfg!(target_os = "macos"), |t| t.contains("apple"));
    let entry = if is_macho { "_main" } else { "main" };

    let linked = if is_macho {
        toyos_ld::link_macho(&objects, entry, true)
    } else {
        toyos_ld::link_full(&objects, entry, true, false)
    };
    linked.unwrap_or_else(|e| panic!("toyos-ld ({label}) link failed: {e}"))
}

/// Link object bytes as a PIE ELF for ToyOS. Returns the linked binary bytes.
pub fn link_toyos(obj: &[u8], extra_objs: &[Vec<u8>], name: &str) -> Vec<u8> {
    let libc_path = libc_archive("x86_64-unknown-toyos");
    let lib_dir = libc_path.parent().unwrap().to_path_buf();

    // Write objects to temp files for resolve_libs_with_entry
    let pid = std::process::id();
    let obj_path = env::temp_dir().join(format!("toyos-test-toyos-{name}-{pid}.o"));
    fs::write(&obj_path, obj).unwrap();

    let mut inputs: Vec<PathBuf> = vec![obj_path.clone()];
    let mut extra_paths = Vec::new();
    for (i, extra) in extra_objs.iter().enumerate() {
        let p = env::temp_dir().join(format!("toyos-test-toyos-{name}-{pid}-extra{i}.o"));
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
    .unwrap_or_else(|e| panic!("resolve_libs (toyos) failed: {e}"));

    // Clean up temp files
    let _ = fs::remove_file(&obj_path);
    for p in &extra_paths {
        let _ = fs::remove_file(p);
    }

    toyos_ld::link_full(&objects, "_start", true, false)
        .unwrap_or_else(|e| panic!("toyos-ld (toyos) link failed: {e}"))
}

/// Link object bytes using the system linker and system libc. Returns path to the binary.
fn link_with_system_cc(obj: &[u8], extra_objs: &[Vec<u8>], name: &str, target: Option<&str>) -> PathBuf {
    let pid = std::process::id();
    let suffix = target.map_or("".to_string(), |t| format!("-{t}"));
    let obj_path = env::temp_dir().join(format!("toyos-syslibc-{name}{suffix}-{pid}.o"));
    fs::write(&obj_path, obj).unwrap();

    let mut extra_paths = Vec::new();
    for (i, extra) in extra_objs.iter().enumerate() {
        let p = env::temp_dir().join(format!("toyos-syslibc-{name}{suffix}-{pid}-extra{i}.o"));
        fs::write(&p, extra).unwrap();
        extra_paths.push(p);
    }

    let bin = env::temp_dir().join(format!("toyos-syslibc{suffix}-{name}-{pid}.bin"));

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
fn run_with_timeout(cmd: &mut Command, name: &str, label: &str) -> Output {
    use std::io::Read;

    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap_or_else(|e| {
        panic!("failed to run {name} ({label}): {e}");
    });
    // Drain stdout/stderr in background threads to prevent pipe deadlocks
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
            panic!("test binary {name} ({label}) timed out after {}s", TEST_TIMEOUT.as_secs());
        }
    }
}

/// Compile, link with system libc, run a C test on the host.
pub fn run_host_test_system_libc(name: &str, args: &[&str], target: Option<&str>) {
    let label = format!("syslibc-{}", target.unwrap_or("native"));
    let dir = testcases_dir();
    let expect_file = dir.join(format!("{name}.expect"));
    assert!(expect_file.exists(), "missing expect file: {}", expect_file.display());
    let expected = fs::read_to_string(&expect_file).unwrap();

    let (obj, extras) = compile_c(name, target);
    let bin = link_with_system_cc(&obj, &extras, name, target);

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

    let run = run_with_timeout(&mut cmd, name, &label);

    let _ = fs::remove_file(&bin);

    let actual = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "test binary {name} ({label}) failed with status {}:\nstdout: {}\nstderr: {}",
        run.status,
        actual,
        String::from_utf8_lossy(&run.stderr),
    );

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "output mismatch for {name} ({label})\n--- expected ---\n{}\n--- actual ---\n{}",
        expected.trim_end(),
        actual.trim_end(),
    );
}

/// Compile, link, and run a C test on the host. Compares output against .expect file.
pub fn run_host_test(name: &str, args: &[&str], target: Option<&str>) {
    let label = target.unwrap_or("native");
    let dir = testcases_dir();
    let expect_file = dir.join(format!("{name}.expect"));
    assert!(expect_file.exists(), "missing expect file: {}", expect_file.display());
    let expected = fs::read_to_string(&expect_file).unwrap();

    let (obj, extras) = compile_c(name, target);
    let linked = link_host(&obj, &extras, name, target);

    // Write binary
    let pid = std::process::id();
    let suffix = target.map_or("".to_string(), |t| format!("-{t}"));
    let bin = env::temp_dir().join(format!("toyos-test{suffix}-{name}-{pid}.bin"));
    fs::write(&bin, &linked).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Run
    let is_x86_64_cross = target.map_or(false, |t| t.starts_with("x86_64"));
    let mut cmd = if is_x86_64_cross {
        let mut c = Command::new("arch");
        c.args(["-x86_64"]).arg(&bin);
        c
    } else {
        Command::new(&bin)
    };
    cmd.args(args).current_dir(&dir);

    let run = run_with_timeout(&mut cmd, name, label);

    let _ = fs::remove_file(&bin);

    let actual = String::from_utf8_lossy(&run.stdout);
    assert!(
        run.status.success(),
        "test binary {name} ({label}) failed with status {}:\nstdout: {}\nstderr: {}",
        run.status,
        actual,
        String::from_utf8_lossy(&run.stderr),
    );

    assert_eq!(
        actual.trim_end(),
        expected.trim_end(),
        "output mismatch for {name} ({label})\n--- expected ---\n{}\n--- actual ---\n{}",
        expected.trim_end(),
        actual.trim_end(),
    );
}
