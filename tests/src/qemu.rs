use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use crate::compile;

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
    _stderr_thread: thread::JoinHandle<String>,
}

impl QemuInstance {
    /// Build everything and boot QEMU with the given test binaries in the initrd.
    /// Waits for the test-runner to signal ===READY=== before returning.
    pub fn boot(
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        let repo = compile::repo_root();

        build_kernel(&repo);
        build_bootloader(&repo);

        let toyos_ld = build_toyos_ld(&repo);

        let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();

        // test-runner (init process)
        let (name, data) = build_toyos_crate(&repo, &repo.join("userland/test-runner"), &toyos_ld);
        initrd_files.push((name, data));

        // Add test binaries
        for (name, data) in c_tests {
            initrd_files.push((format!("test_c_{name}"), data.clone()));
        }
        for (name, data) in rust_tests {
            initrd_files.push((format!("test_rs_{name}"), data.clone()));
        }

        let initrd_bytes = create_initrd(&initrd_files, &[]);

        let kernel_bytes = fs::read(repo.join("kernel/target/x86_64-unknown-none/debug/kernel"))
            .expect("Failed to read kernel");
        let bl_bytes = fs::read(repo.join("bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi"))
            .expect("Failed to read bootloader");

        let esp = create_fat_volume(&kernel_bytes, &bl_bytes, &initrd_bytes);
        let disk = create_gpt_disk(esp);

        let test_dir = env::temp_dir().join("toyos-tests");
        fs::create_dir_all(&test_dir).ok();
        let boot_image = test_dir.join("test-bootable.img");
        fs::write(&boot_image, &disk).expect("Failed to write test boot image");

        let nvme_image = test_dir.join("test-nvme.img");
        if !nvme_image.exists() {
            fs::write(&nvme_image, vec![0u8; 128 * 1024 * 1024]).expect("Failed to write NVMe image");
        }

        let ovmf_dir = repo.join("bootable/ovmf");

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
            .arg("-no-reboot")
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        eprintln!("[qemu] Launching QEMU...");
        let mut child = qemu.spawn().expect("Failed to launch QEMU");

        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let stderr_thread = thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut log = String::new();
            for line in reader.lines().flatten() {
                log.push_str(&line);
                log.push('\n');
            }
            log
        });

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
        let boot_timeout = Duration::from_secs(120);
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
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    panic!("[qemu] QEMU died before ===READY===");
                }
            }
        }

        QemuInstance {
            child,
            stdin,
            rx,
            _reader_thread: reader_thread,
            _stderr_thread: stderr_thread,
        }
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
                    } else if in_test && !line.starts_with("[kernel] ") {
                        stdout.push_str(&line);
                        stdout.push('\n');
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

/// Build the kernel for test mode.
fn build_kernel(repo: &Path) {
    let toyos_ld = build_toyos_ld(repo);
    assert!(
        Command::new("cargo")
            .args(["build"])
            .current_dir(repo.join("kernel"))
            .env("CARGO_TARGET_X86_64_UNKNOWN_NONE_LINKER", toyos_ld.to_str().unwrap())
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build kernel"
    );
}

/// Build the bootloader with test-init feature.
fn build_bootloader(repo: &Path) {
    let toyos_ld = build_toyos_ld(repo);
    assert!(
        Command::new("cargo")
            .args(["build", "--features", "test-init"])
            .current_dir(repo.join("bootloader"))
            .env("CARGO_TARGET_X86_64_UNKNOWN_UEFI_LINKER", toyos_ld.to_str().unwrap())
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build bootloader with test-init"
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
    let toyos_ld_dir = repo.join("userland/toyos-ld");
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
    let path = toyos_ld_dir
        .join(format!("target/{host}/release/toyos-ld"))
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

/// Build a ToyOS userland crate. Returns (binary_name, binary_bytes).
fn build_toyos_crate(_repo: &Path, crate_path: &Path, toyos_ld: &Path) -> (String, Vec<u8>) {
    let name = crate_path.file_name().unwrap().to_str().unwrap().to_string();
    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-toyos"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("CARGO_TARGET_X86_64_UNKNOWN_TOYOS_LINKER", toyos_ld.to_str().unwrap())
            .env_remove("RUSTC")
            .current_dir(crate_path)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build userland/{name}"
    );
    let binary = crate_path.join(format!("target/x86_64-unknown-toyos/debug/{name}"));
    let data = fs::read(&binary).unwrap_or_else(|e| {
        panic!("Failed to read {}: {e}", binary.display());
    });
    (name, data)
}

/// Build all binaries in a multi-binary crate. Returns vec of (binary_name, bytes).
pub fn build_toyos_bins(crate_path: &Path, toyos_ld: &Path) -> Vec<(String, Vec<u8>)> {
    assert!(
        Command::new("cargo")
            .args(["build", "--target", "x86_64-unknown-toyos", "--bins"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("CARGO_TARGET_X86_64_UNKNOWN_TOYOS_LINKER", toyos_ld.to_str().unwrap())
            .env_remove("RUSTC")
            .current_dir(crate_path)
            .status()
            .expect("Failed to run cargo")
            .success(),
        "Failed to build toyos-rust-tests"
    );

    let bin_dir = crate_path.join("target/x86_64-unknown-toyos/debug");
    let mut results = Vec::new();

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

/// Create a TyFS initrd image from files and symlinks.
fn create_initrd(files: &[(String, Vec<u8>)], symlinks: &[(String, String)]) -> Vec<u8> {
    use tyfs::SimpleFs;

    struct VecDisk { data: Vec<u8> }
    impl tyfs::Disk for VecDisk {
        fn read(&mut self, offset: u64, buf: &mut [u8]) {
            let off = offset as usize;
            buf.copy_from_slice(&self.data[off..off + buf.len()]);
        }
        fn write(&mut self, offset: u64, buf: &[u8]) {
            let off = offset as usize;
            self.data[off..off + buf.len()].copy_from_slice(buf);
        }
        fn flush(&mut self) {}
    }

    let data_size: usize = files.iter().map(|(name, d)| name.len() + d.len()).sum::<usize>()
        + symlinks.iter().map(|(name, target)| name.len() + target.len()).sum::<usize>();
    let toc_size = (files.len() + symlinks.len()) * 64;
    let size = (64 + data_size + toc_size + 4095) & !4095;
    let size = size.max(4096);

    let disk = VecDisk { data: vec![0u8; size] };
    let mut tyfs = SimpleFs::format(disk, size as u64);

    for (name, data) in files {
        if !tyfs.create(name, data, 0) {
            panic!("Failed to add '{name}' to test initrd");
        }
    }
    for (name, target) in symlinks {
        if !tyfs.create_symlink(name, target) {
            panic!("Failed to add symlink '{name}' -> '{target}' to test initrd");
        }
    }

    tyfs.into_disk().data
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
