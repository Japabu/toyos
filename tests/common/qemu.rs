use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use super::compile;

/// Result of a single test run inside QEMU.
#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub error: Option<String>,
}

/// A running QEMU instance with serial command interface.
pub struct QemuInstance {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    rx: Receiver<String>,
    _reader_thread: thread::JoinHandle<String>,
}

/// Prepare the boot image and return (boot_image_path, nvme_image_path, repo_root).
fn prepare_boot(
    c_tests: &[(String, Vec<u8>)],
    rust_tests: &[(String, Vec<u8>)],
) -> (PathBuf, PathBuf, PathBuf) {
    let repo = compile::repo_root();

    build_kernel(&repo);
    build_bootloader(&repo);

    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();

    // test-runner (init process)
    let (name, data) = build_toyos_crate(&repo.join("userland/test-runner"));
    initrd_files.push((format!("bin/{name}"), data));

    // toybox (provides echo, cat, etc. for process tests)
    let (name, data) = build_toyos_crate(&repo.join("userland/toybox"));
    initrd_files.push((format!("bin/{name}"), data));
    let mut symlinks = Vec::new();
    for entry in fs::read_dir(repo.join("userland/toybox/src")).unwrap() {
        let entry = entry.unwrap();
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.ends_with(".rs") && fname != "main.rs" {
            let cmd = fname.strip_suffix(".rs").unwrap();
            symlinks.push((format!("bin/{cmd}"), "bin/toybox".to_string()));
        }
    }

    // Add test binaries
    for (name, data) in c_tests {
        initrd_files.push((format!("bin/test_c_{name}"), data.clone()));
    }
    for (name, data) in rust_tests {
        if name.ends_with(".so") {
            // Shared libraries go into /lib/
            initrd_files.push((format!("lib/{name}"), data.clone()));
        } else {
            initrd_files.push((format!("bin/test_rs_{name}"), data.clone()));
        }
    }

    let initrd_bytes = create_initrd(&initrd_files, &symlinks);

    let kernel_bytes = fs::read(repo.join("kernel/target/x86_64-unknown-none/debug/kernel"))
        .expect("Failed to read kernel");
    let bl_bytes = fs::read(repo.join("bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi"))
        .expect("Failed to read bootloader");

    let esp = create_fat_volume(&kernel_bytes, &bl_bytes, &initrd_bytes);
    let disk = create_gpt_disk(esp);

    let pid = std::process::id();
    let test_dir = env::temp_dir().join(format!("toyos-tests-{pid}"));
    fs::create_dir_all(&test_dir).ok();
    let boot_image = test_dir.join("test-bootable.img");
    fs::write(&boot_image, &disk).expect("Failed to write test boot image");

    let nvme_image = test_dir.join("test-nvme.img");
    if !nvme_image.exists() {
        fs::write(&nvme_image, vec![0u8; 128 * 1024 * 1024]).expect("Failed to write NVMe image");
    }

    (boot_image, nvme_image, repo)
}

/// Build the QEMU command with standard arguments.
fn qemu_command(boot_image: &Path, nvme_image: &Path, repo: &Path, gdb_stub: bool) -> Command {
    let ovmf_dir = repo.join("ovmf");

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu
        .arg("-machine").arg("q35")
        .arg("-cpu").arg("qemu64,+rdrand")
        .arg("-smp").arg("2")
        .arg("-m").arg("4G")
        .arg("-drive").arg(format!("if=pflash,format=raw,unit=0,file={},readonly=on", ovmf_dir.join("OVMF_CODE-pure-efi.fd").display()))
        .arg("-drive").arg(format!("if=pflash,format=raw,unit=1,file={},readonly=on", ovmf_dir.join("OVMF_VARS-pure-efi.fd").display()))
        .arg("-device").arg("nec-usb-xhci,id=xhci")
        .arg("-drive").arg(format!("if=none,id=stick,format=raw,file={}", boot_image.display()))
        .arg("-device").arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")
        .arg("-device").arg("usb-kbd,bus=xhci.0")
        .arg("-drive").arg(format!("if=none,id=nvme0,format=raw,file={}", nvme_image.display()))
        .arg("-device").arg("nvme,serial=deadbeef,drive=nvme0")
        .arg("-vga").arg("none")
        .arg("-display").arg("none")
        .arg("-netdev").arg("user,id=net0")
        .arg("-device").arg("virtio-net-pci-non-transitional,netdev=net0")
        .arg("-serial").arg("stdio")
        .arg("-no-reboot");

    if gdb_stub {
        qemu.arg("-s");
    }

    qemu
}

/// Spawn QEMU and wait for the test-runner to signal ===READY===.
/// Returns the QemuInstance with serial I/O channels.
fn spawn_and_wait_ready(mut qemu: Command) -> QemuInstance {
    qemu.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    eprintln!("[qemu] Launching QEMU...");
    let mut child = qemu.spawn().expect("Failed to launch QEMU");

    let stdin = BufWriter::new(child.stdin.take().unwrap());
    let stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel::<String>();
    let reader_thread = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut full_log = String::new();
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    full_log.push_str(&line);
                    full_log.push('\n');
                    eprintln!("[serial] {line}");
                    if tx.send(line).is_err() { break; }
                }
                Err(_) => break,
            }
        }
        full_log
    });

    // Wait for ===READY=== from the test-runner
    let boot_timeout = Duration::from_secs(10);
    let start = Instant::now();
    loop {
        if start.elapsed() > boot_timeout {
            let _ = child.kill();
            panic!("[qemu] Boot timed out waiting for ===READY===");
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) if line.contains("===READY===") => {
                eprintln!("[qemu] Test runner ready");
                break;
            }
            Ok(ref line) if line.contains("SEGFAULT") || line.contains("KERNEL PANIC") => {
                let _ = child.kill();
                panic!("[qemu] Init process crashed during boot: {line}");
            }
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait();
                panic!("[qemu] QEMU died before ===READY=== (status: {status:?})");
            }
        }
    }

    QemuInstance {
        child,
        stdin,
        rx,
        _reader_thread: reader_thread,
    }
}

impl QemuInstance {
    /// Build everything and boot QEMU with the given test binaries in the initrd.
    /// Waits for the test-runner to signal ===READY=== before returning.
    pub fn boot(
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(c_tests, rust_tests, false)
    }

    /// Boot QEMU with configurable GDB stub.
    pub fn boot_with_options(
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        gdb_stub: bool,
    ) -> Self {
        let (boot_image, nvme_image, repo) = prepare_boot(c_tests, rust_tests);
        let qemu = qemu_command(&boot_image, &nvme_image, &repo, gdb_stub);
        spawn_and_wait_ready(qemu)
    }

    /// Get mutable access to QEMU's stdin (serial input).
    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

    /// Flush QEMU's stdin.
    pub fn flush_stdin(&mut self) {
        self.stdin.flush().expect("Failed to flush QEMU stdin");
    }

    /// Run a single test by sending a `run` command over serial.
    pub fn run_test(&mut self, name: &str, timeout: Duration) -> TestResult {
        writeln!(self.stdin, "run {name}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");

        let start = Instant::now();
        let mut stdout = String::new();
        let mut in_test = false;

        loop {
            if start.elapsed() > timeout {
                return TestResult {
                    name: name.to_string(),
                    exit_code: None,
                    stdout,
                    error: Some(format!("timed out after {}s", timeout.as_secs())),
                };
            }

            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    if line.contains("===TEST_START ") {
                        in_test = true;
                    } else if let Some(rest) = line.strip_prefix("===TEST_END ") {
                        let rest = rest.strip_suffix("===").unwrap_or(rest);
                        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
                        let (exit_code, error) = if parts.len() > 1 {
                            if let Some(code_str) = parts[1].strip_prefix("exit=") {
                                (code_str.parse::<i32>().ok(), None)
                            } else if let Some(err) = parts[1].strip_prefix("error=") {
                                (None, Some(err.to_string()))
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        };
                        let status = match (exit_code, &error) {
                            (Some(0), None) => "PASS",
                            _ => "FAIL",
                        };
                        eprintln!("[test] {status} {name}");
                        return TestResult {
                            name: name.to_string(),
                            exit_code,
                            stdout,
                            error,
                        };
                    } else if line.starts_with("[kernel] KERNEL PANIC:") {
                        return TestResult {
                            name: name.to_string(),
                            exit_code: None,
                            stdout,
                            error: Some(format!("kernel panic: {line}")),
                        };
                    } else if in_test {
                        // Filter kernel log lines; handle partial lines where
                        // test output and kernel log are merged (no trailing newline)
                        if line.starts_with("[kernel] ") {
                            // Pure kernel line, skip
                        } else if let Some(idx) = line.find("[kernel] ") {
                            // Test output merged with kernel log on same line
                            stdout.push_str(&line[..idx]);
                            stdout.push('\n');
                        } else {
                            stdout.push_str(&line);
                            stdout.push('\n');
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return TestResult {
                        name: name.to_string(),
                        exit_code: None,
                        stdout,
                        error: Some("QEMU disconnected".to_string()),
                    };
                }
            }
        }
    }
}

impl Drop for QemuInstance {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "quit");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Build the kernel. Uses the `toyos` toolchain (linked by the main build system).
fn build_kernel(repo: &Path) {
    let path_env = path_with_toyos_ld(repo);
    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-none"])
            .current_dir(repo.join("kernel"))
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("PATH", &path_env)
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build kernel"
    );
}

/// Build the bootloader with test-runner as init.
fn build_bootloader(repo: &Path) {
    let path_env = path_with_toyos_ld(repo);
    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-uefi"])
            .current_dir(repo.join("bootloader"))
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("PATH", &path_env)
            .env("INIT_PROGRAMS", "/bin/test-runner")
            .env_remove("RUSTC")
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build bootloader"
    );
}

/// Build toyos-ld as a host binary. Returns path to the binary.
fn build_toyos_ld(repo: &Path) -> PathBuf {
    static CACHE: std::sync::LazyLock<std::sync::Mutex<Option<PathBuf>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(None));
    let mut cache = CACHE.lock().unwrap();
    if let Some(p) = cache.as_ref() {
        return p.clone();
    }
    let toyos_ld_dir = repo.join("toyos-ld");
    let host = host_triple();
    assert!(
        Command::new("cargo")
            .args(["build", "--release", "--target", &host])
            .current_dir(&toyos_ld_dir)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build toyos-ld"
    );
    let path = repo.join(format!("toyos-ld/target/{host}/release/toyos-ld"))
        .canonicalize()
        .expect("toyos-ld binary not found after build");
    *cache = Some(path.clone());
    path
}

fn host_triple() -> String {
    let output = Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .expect("Failed to run rustc");
    let text = String::from_utf8(output.stdout).unwrap();
    text.lines()
        .find(|l| l.starts_with("host:"))
        .map(|l| l.strip_prefix("host: ").unwrap().to_string())
        .expect("Could not determine host triple")
}

/// PATH with toyos-ld's directory prepended so rustc finds the linker.
fn path_with_toyos_ld(repo: &Path) -> String {
    let ld = build_toyos_ld(repo);
    let ld_dir = ld.parent().unwrap();
    match env::var("PATH") {
        Ok(p) => format!("{}:{p}", ld_dir.display()),
        Err(_) => ld_dir.display().to_string(),
    }
}

/// Build a ToyOS userland crate. Returns (binary_name, binary_bytes).
fn build_toyos_crate(crate_path: &Path) -> (String, Vec<u8>) {
    let name = crate_path.file_name().unwrap().to_str().unwrap().to_string();
    let repo = compile::repo_root();
    let path_env = path_with_toyos_ld(&repo);
    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-toyos"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("PATH", &path_env)
            .env_remove("RUSTC")
            .current_dir(crate_path)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build userland/{name}"
    );
    // Workspace members output to the workspace target dir (parent of crate_path)
    let workspace_target = crate_path.parent().unwrap().join("target");
    let binary = workspace_target.join(format!("x86_64-unknown-toyos/debug/{name}"));
    // Fall back to crate-local target dir for standalone crates
    let binary = if binary.exists() {
        binary
    } else {
        crate_path.join(format!("target/x86_64-unknown-toyos/debug/{name}"))
    };
    let data = fs::read(&binary).unwrap_or_else(|e| {
        panic!("Failed to read {}: {e}", binary.display());
    });
    (name, data)
}

/// Build all binaries in a multi-binary crate. Returns vec of (binary_name, bytes).
/// Also builds any cdylib subcrates and includes their .so files.
pub fn build_toyos_bins(crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    let bin_dir = crate_path.join("target/x86_64-unknown-toyos/debug");
    let repo = compile::repo_root();
    let path_env = path_with_toyos_ld(&repo);
    let mut results = Vec::new();

    // Build cdylib subcrates first (test shared libraries)
    let mut lib_search_dirs = Vec::new();
    for entry in fs::read_dir(crate_path).unwrap() {
        let entry = entry.unwrap();
        let sub_path = entry.path();
        if !sub_path.is_dir() { continue; }
        let cargo_toml = sub_path.join("Cargo.toml");
        if !cargo_toml.exists() { continue; }
        let toml_text = fs::read_to_string(&cargo_toml).unwrap();
        if !toml_text.contains("cdylib") { continue; }

        let lib_name = sub_path.file_name().unwrap().to_str().unwrap();
        eprintln!("[build] Building cdylib subcrate: {lib_name}");
        assert!(
            Command::new("cargo")
                .args(["build", "--target", "x86_64-unknown-toyos"])
                .env("RUSTUP_TOOLCHAIN", "toyos")
                .env("PATH", &path_env)
                .env_remove("RUSTC")
                .current_dir(&sub_path)
                .status()
                .expect("Failed to run cargo")
                .success(),
            "Failed to build cdylib {lib_name}"
        );

        let lib_out = sub_path.join("target/x86_64-unknown-toyos/debug");
        lib_search_dirs.push(lib_out.clone());

        // Collect .so files from the output
        for so_entry in fs::read_dir(&lib_out).unwrap() {
            let so_entry = so_entry.unwrap();
            let name = so_entry.file_name().to_str().unwrap().to_string();
            if name.ends_with(".so") {
                let data = fs::read(so_entry.path()).unwrap();
                results.push((name, data));
            }
        }
    }

    // Build test binaries, linking against cdylib outputs
    let mut rustflags = String::new();
    for dir in &lib_search_dirs {
        rustflags.push_str(&format!("-L {} ", dir.display()));
    }

    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-toyos", "--bins"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("PATH", &path_env)
            .env("RUSTFLAGS", &rustflags)
            .env_remove("RUSTC")
            .current_dir(crate_path)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build toyos-rust-tests"
    );

    let bin_src = crate_path.join("src/bin");
    if bin_src.exists() {
        for entry in fs::read_dir(&bin_src).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_str().unwrap().strip_suffix(".rs").unwrap().to_string();
            let binary = bin_dir.join(&name);
            if binary.exists() {
                let data = fs::read(&binary).unwrap();
                results.push((name, data));
            }
        }
    }

    results
}

/// Create a bcachefs initrd image from files and symlinks.
fn create_initrd(files: &[(String, Vec<u8>)], symlinks: &[(String, String)]) -> Vec<u8> {
    let data_size: usize = files.iter().map(|(name, d)| name.len() + d.len()).sum::<usize>()
        + symlinks.iter().map(|(name, target)| name.len() + target.len()).sum::<usize>();
    // bcachefs needs enough blocks: 4KB per block, generous sizing for btree overhead
    let n_blocks = ((data_size + files.len() * 4096 + symlinks.len() * 4096 + 256 * 1024) / 4096).max(128);
    let io = bcachefs::VecBlockIO::new(n_blocks as u64);
    let mut fs = bcachefs::Formatted::format(io);

    for (name, data) in files {
        fs.create(name, data, 0)
            .unwrap_or_else(|e| panic!("failed to add '{name}' to test initrd: {e:?}"));
    }
    for (name, target) in symlinks {
        fs.create_symlink(name, target, 0)
            .unwrap_or_else(|e| panic!("failed to add symlink '{name}' -> '{target}' to test initrd: {e:?}"));
    }

    fs.into_io().into_vec()
}

/// Create a FAT32 ESP volume.
fn create_fat_volume(kernel: &[u8], bootloader: &[u8], initrd: &[u8]) -> Vec<u8> {
    use fatfs::FsOptions;
    use std::io::Cursor;

    let content_size = kernel.len() + bootloader.len() + initrd.len();
    let total_size = (content_size + 4 * 1024 * 1024).max(34 * 1024 * 1024);
    let mut volume = vec![0u8; total_size];

    fatfs::format_volume(
        Cursor::new(&mut volume),
        fatfs::FormatVolumeOptions::new().fat_type(fatfs::FatType::Fat32),
    )
    .expect("Failed to format FAT volume");

    {
        let fat = fatfs::FileSystem::new(Cursor::new(&mut volume), FsOptions::new())
            .expect("Failed to open FAT filesystem");

        fat.root_dir()
            .create_dir("EFI").unwrap()
            .create_dir("BOOT").unwrap()
            .create_file("BOOTx64.EFI").unwrap()
            .write_all(bootloader).unwrap();

        let toyos_dir = fat.root_dir().create_dir("toyos").unwrap();
        toyos_dir.create_file("kernel.elf").unwrap().write_all(kernel).unwrap();
        toyos_dir.create_file("initrd.img").unwrap().write_all(initrd).unwrap();
    }

    volume
}

/// Create a GPT disk image.
fn create_gpt_disk(esp_volume: Vec<u8>) -> Vec<u8> {
    use std::io::{Cursor, Read, Seek, SeekFrom};

    let overhead = 100 * 1024;
    let total_size = esp_volume.len() + overhead;
    let mut disk = vec![0u8; total_size];

    let mut cursor = Cursor::new(&mut disk);
    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
        u32::try_from((total_size / 512) - 1).unwrap_or(0xFF_FF_FF_FF),
    );
    mbr.overwrite_lba0(&mut cursor).expect("failed to write MBR");

    let mut gdisk = gpt::GptConfig::default()
        .initialized(false)
        .writable(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .create_from_device(Box::new(cursor), None)
        .expect("failed to create GPT disk");

    gdisk
        .update_partitions(std::collections::BTreeMap::<u32, gpt::partition::Partition>::new())
        .expect("failed to initialize partition table");

    let esp_id = gdisk
        .add_partition("EFI System", esp_volume.len() as u64, gpt::partition_types::EFI, 0, None)
        .expect("failed to add ESP partition");

    let esp_start = gdisk.partitions().get(&esp_id).unwrap()
        .bytes_start(gpt::disk::LogicalBlockSize::Lb512)
        .expect("failed to get ESP start") as usize;

    let mut disk_device = gdisk.write().expect("failed to write GPT");
    disk_device.seek(SeekFrom::Start(0)).expect("failed to seek");
    let mut final_bytes = vec![0u8; total_size];
    disk_device.read_exact(&mut final_bytes).expect("failed to read disk");
    final_bytes[esp_start..esp_start + esp_volume.len()].copy_from_slice(&esp_volume);

    final_bytes
}
