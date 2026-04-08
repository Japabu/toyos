use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use super::compile;

/// When true, serial output is printed to stderr as it arrives.
pub static VERBOSE: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct TestResult {
    pub name: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub serial: String,
    pub error: Option<String>,
}

pub struct QemuInstance {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    rx: Receiver<String>,
    _reader_thread: thread::JoinHandle<String>,
}

/// Build all binaries in a test crate.
pub fn build_toyos_bins(crate_path: &Path) -> Vec<(String, Vec<u8>)> {
    let repo = compile::repo_root();
    let quiet = !VERBOSE.load(Ordering::Relaxed);
    toyos_build::build::build_toyos_bins(&repo, crate_path, quiet)
}

/// All kernel serial output goes through log!() which prepends "[kernel ...]".
/// User program output goes through serial::write directly with no prefix.
fn is_kernel_line(line: &str) -> bool {
    line.starts_with("[kernel ")
}

impl QemuInstance {
    /// Build everything and boot QEMU with test binaries in the initrd.
    /// `test_crate` is the path to the test crate (must contain a `system.toml`).
    pub fn boot(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(test_crate, c_tests, rust_tests, false, false)
    }

    pub fn boot_with_options(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        gdb_stub: bool,
        debug_wait: bool,
    ) -> Self {
        let repo = compile::repo_root();

        // Package test binaries as extra initrd files
        let mut extra_files: Vec<(String, Vec<u8>)> = Vec::new();
        for (name, data) in c_tests {
            extra_files.push((format!("bin/test_c_{name}"), data.clone()));
        }
        for (name, data) in rust_tests {
            if name.ends_with(".so") {
                extra_files.push((format!("lib/{name}"), data.clone()));
            } else {
                extra_files.push((format!("bin/test_rs_{name}"), data.clone()));
            }
        }

        let config_path = test_crate.join("system.toml");
        assert!(
            config_path.exists(),
            "Test crate missing system.toml: {}",
            config_path.display()
        );

        let quiet = !VERBOSE.load(Ordering::Relaxed);
        let disk = toyos_build::build::build_test_image(&repo, &config_path, debug_wait, quiet, &extra_files);

        let pid = std::process::id();
        let test_dir = env::temp_dir().join(format!("toyos-tests-{pid}"));
        fs::create_dir_all(&test_dir).ok();

        let boot_image = test_dir.join("test-bootable.img");
        fs::write(&boot_image, &disk).expect("Failed to write test boot image");

        let nvme_image = test_dir.join("test-nvme.img");
        if !nvme_image.exists() {
            fs::write(&nvme_image, vec![0u8; 128 * 1024 * 1024])
                .expect("Failed to write NVMe image");
        }

        let qemu = qemu_command(&boot_image, &nvme_image, gdb_stub);
        spawn_and_wait_ready(qemu, debug_wait)
    }

    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

    pub fn flush_stdin(&mut self) {
        self.stdin.flush().expect("Failed to flush QEMU stdin");
    }

    pub fn run_test(&mut self, name: &str, timeout: Duration) -> TestResult {
        writeln!(self.stdin, "run {name}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");

        let start = Instant::now();
        let mut stdout = String::new();
        let mut serial = String::new();
        let mut in_test = false;

        loop {
            if start.elapsed() > timeout {
                return TestResult {
                    name: name.to_string(),
                    exit_code: None,
                    stdout,
                    serial,
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
                        return TestResult {
                            name: name.to_string(),
                            exit_code,
                            stdout,
                            serial,
                            error,
                        };
                    } else if line.contains("KERNEL PANIC") {
                        return TestResult {
                            name: name.to_string(),
                            exit_code: None,
                            stdout,
                            serial,
                            error: Some(format!("kernel panic: {line}")),
                        };
                    } else if in_test {
                        serial.push_str(&line);
                        serial.push('\n');
                        if is_kernel_line(&line) {
                            // pure kernel line
                        } else if let Some(idx) = line.find("[kernel ") {
                            // user output with kernel suffix on same line
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
                        serial,
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

fn qemu_command(boot_image: &Path, nvme_image: &Path, gdb_stub: bool) -> Command {
    let repo = compile::repo_root();
    let ovmf_dir = repo.join("ovmf");

    let mut qemu = Command::new("qemu-system-x86_64");

    let kvm = cfg!(target_arch = "x86_64") && Path::new("/dev/kvm").exists();
    if kvm {
        qemu.arg("-accel").arg("kvm");
    }

    qemu.arg("-machine")
        .arg("q35")
        .arg("-cpu")
        .arg(if kvm { "host,+rdrand,+smap,+fsgsbase,+x2apic" } else { "qemu64,+rdrand,+smap,+fsgsbase,+x2apic" })
        .arg("-smp")
        .arg("2")
        .arg("-m")
        .arg("4G")
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=0,file={},readonly=on",
            ovmf_dir.join("OVMF_CODE-pure-efi.fd").display()
        ))
        .arg("-drive")
        .arg(format!(
            "if=pflash,format=raw,unit=1,file={},readonly=on",
            ovmf_dir.join("OVMF_VARS-pure-efi.fd").display()
        ))
        .arg("-device")
        .arg("nec-usb-xhci,id=xhci")
        .arg("-drive")
        .arg(format!(
            "if=none,id=stick,format=raw,file={}",
            boot_image.display()
        ))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick,bootindex=0")
        .arg("-device")
        .arg("usb-kbd,bus=xhci.0")
        .arg("-drive")
        .arg(format!(
            "if=none,id=nvme0,format=raw,file={}",
            nvme_image.display()
        ))
        .arg("-device")
        .arg("nvme,serial=deadbeef,drive=nvme0")
        .arg("-vga")
        .arg("none")
        .arg("-display")
        .arg("none")
        .arg("-netdev")
        .arg("user,id=net0")
        .arg("-device")
        .arg("virtio-net-pci-non-transitional,netdev=net0")
        .arg("-serial")
        .arg("stdio")
        .arg("-no-reboot");

    if gdb_stub {
        qemu.arg("-s");
    }

    qemu
}

fn spawn_and_wait_ready(mut qemu: Command, no_timeout: bool) -> QemuInstance {
    qemu.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if VERBOSE.load(Ordering::Relaxed) {
        eprintln!("[qemu] Launching QEMU...");
    }
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
                    if VERBOSE.load(Ordering::Relaxed) {
                        eprintln!("[serial] {line}");
                    }
                    if tx.send(line).is_err() {
                        break;
                    }
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
                if VERBOSE.load(Ordering::Relaxed) {
                    eprintln!("[qemu] Test runner ready");
                }
                break;
            }
            Ok(ref line)
                if !no_timeout
                    && (line.contains("SEGFAULT")
                        || line.contains("KERNEL PANIC")
                        || line.contains("!!! PANIC !!!")) =>
            {
                let mut crash_msg = line.clone();
                let drain_deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < drain_deadline {
                    match rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(bt_line) => {
                            crash_msg.push('\n');
                            crash_msg.push_str(&bt_line);
                        }
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
