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
    debug_wait: bool,
) -> (PathBuf, PathBuf, PathBuf) {
    let repo = compile::repo_root();

    // Use the main build system for toolchain, kernel, and bootloader
    toyos::build::build_for_tests(&repo, debug_wait);

    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();

    // test-runner (init process)
    let (name, data) = toyos::build::build_toyos_crate(&repo, &repo.join("userland/test-runner"));
    initrd_files.push((format!("bin/{name}"), data));

    // toybox (provides echo, cat, etc. for process tests)
    let (name, data) = toyos::build::build_toyos_crate(&repo, &repo.join("userland/toybox"));
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
            initrd_files.push((format!("lib/{name}"), data.clone()));
        } else {
            initrd_files.push((format!("bin/test_rs_{name}"), data.clone()));
        }
    }

    let initrd_bytes = toyos::image::create_initrd(&initrd_files, &symlinks);

    let kernel_bytes = fs::read(repo.join("kernel/target/x86_64-unknown-none/debug/kernel"))
        .expect("Failed to read kernel");
    let bl_bytes = fs::read(repo.join("bootloader/target/x86_64-unknown-uefi/debug/bootloader.efi"))
        .expect("Failed to read bootloader");

    let esp = toyos::image::create_fat_volume(&kernel_bytes, &bl_bytes, &initrd_bytes);
    let disk = toyos::image::create_gpt_disk(esp);

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
fn spawn_and_wait_ready(mut qemu: Command, no_timeout: bool) -> QemuInstance {
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

    let boot_timeout = Duration::from_secs(10);
    let start = Instant::now();
    loop {
        if !no_timeout && start.elapsed() > boot_timeout {
            let _ = child.kill();
            panic!("[qemu] Boot timed out waiting for ===READY===");
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) if line.contains("===READY===") => {
                eprintln!("[qemu] Test runner ready");
                break;
            }
            Ok(ref line) if !no_timeout && (line.contains("SEGFAULT") || line.contains("KERNEL PANIC") || line.contains("!!! PANIC !!!")) => {
                let mut crash_msg = line.clone();
                let drain_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < drain_deadline {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(bt_line) => { crash_msg.push('\n'); crash_msg.push_str(&bt_line); }
                        Err(_) => break,
                    }
                }
                let _ = child.kill();
                panic!("[qemu] Init process crashed during boot:\n{crash_msg}");
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
    pub fn boot(
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(c_tests, rust_tests, false, false)
    }

    /// Boot QEMU with configurable GDB stub and kernel debug-wait.
    pub fn boot_with_options(
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        gdb_stub: bool,
        debug_wait: bool,
    ) -> Self {
        let (boot_image, nvme_image, repo) = prepare_boot(c_tests, rust_tests, debug_wait);
        let qemu = qemu_command(&boot_image, &nvme_image, &repo, gdb_stub);
        spawn_and_wait_ready(qemu, debug_wait)
    }

    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

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
                        if line.starts_with("[kernel] ") {
                            // Pure kernel line, skip
                        } else if let Some(idx) = line.find("[kernel] ") {
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

/// Build all binaries in a multi-binary crate. Delegates to the main build system.
pub fn build_toyos_bins(crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    let repo = compile::repo_root();
    toyos::build::build_toyos_bins(&repo, crate_path)
}
