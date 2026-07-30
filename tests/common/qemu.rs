use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use super::compile;

/// When true, serial output is printed to stderr as it arrives.
pub static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Distinguishes the wav capture of each QEMU boot within one test process.
static BOOT_SEQ: AtomicU32 = AtomicU32::new(0);

pub struct BootOptions {
    pub gdb_stub: bool,
    pub debug_wait: bool,
    pub smp: u32,
}

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            gdb_stub: false,
            debug_wait: false,
            smp: 2,
        }
    }
}

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
    audio_wav: PathBuf,
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

/// The in-guest runner's end-of-test marker. Matched anywhere in the line, not
/// as a prefix: the virtio-console is shared and not line-atomic, so a daemon
/// mid-`println!` pushes the marker into the middle of its line. Anchoring on
/// the prefix made the harness miss the marker and time out — measured at 1 in
/// 120 audio boots, where it looked like a guest hang rather than a lost line.
const END_MARKER: &str = "===TEST_END ";

impl QemuInstance {
    /// Build everything and boot QEMU with test binaries in the initrd.
    /// `test_crate` is the path to the test crate (must contain a `system.toml`).
    pub fn boot(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
    ) -> Self {
        Self::boot_with_options(test_crate, c_tests, rust_tests, BootOptions::default())
    }

    pub fn boot_with_options(
        test_crate: &Path,
        c_tests: &[(String, Vec<u8>)],
        rust_tests: &[(String, Vec<u8>)],
        options: BootOptions,
    ) -> Self {
        let repo = compile::repo_root();

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
        let disk = toyos_build::build::build_test_image(
            &repo,
            &config_path,
            options.debug_wait,
            quiet,
            &extra_files,
        );

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

        let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let audio_wav = test_dir.join(format!("audio-{seq}.wav"));
        let _ = fs::remove_file(&audio_wav);

        let qemu = qemu_command(&boot_image, &nvme_image, &audio_wav, &options);
        spawn_and_wait_ready(qemu, options.debug_wait, audio_wav)
    }

    /// The wav file the virtio-sound device records into for this boot.
    /// The RIFF size fields stay 0 until QEMU exits cleanly — parse to EOF.
    pub fn audio_wav_path(&self) -> &Path {
        &self.audio_wav
    }

    pub fn stdin_mut(&mut self) -> &mut BufWriter<ChildStdin> {
        &mut self.stdin
    }

    pub fn flush_stdin(&mut self) {
        self.stdin.flush().expect("Failed to flush QEMU stdin");
    }

    /// Keep collecting serial output for `dur` after a test has returned.
    /// soundd flushes its final stats window when the last client leaves,
    /// which races the client process's exit — so the line the audio gate
    /// reads lands on either side of `===TEST_END===`.
    pub fn drain_serial(&mut self, dur: Duration) -> String {
        let deadline = Instant::now() + dur;
        let mut out = String::new();
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return out;
            };
            match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                }
                Err(RecvTimeoutError::Timeout) => return out,
                Err(RecvTimeoutError::Disconnected) => return out,
            }
        }
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
                    } else if let Some(at) = line.find(END_MARKER) {
                        // Everything before the marker is a line some other
                        // console writer was in the middle of when the runner
                        // printed; it is still real output and the audio gate
                        // reads soundd's stats out of it.
                        if at > 0 && in_test {
                            serial.push_str(&line[..at]);
                            serial.push('\n');
                        }
                        let rest = &line[at + END_MARKER.len()..];
                        let rest = rest.split_once("===").map_or(rest, |(head, _)| head);
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
        let _ = fs::remove_file(&self.audio_wav);
    }
}

fn qemu_command(
    boot_image: &Path,
    nvme_image: &Path,
    audio_wav: &Path,
    options: &BootOptions,
) -> Command {
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
        .arg(options.smp.to_string())
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
        // virtio-sound records everything the guest plays into a per-boot
        // wav for glitch analysis; timer-period matches the interactive
        // config in src/qemu.rs so test timing represents what users hear.
        .arg("-audiodev")
        .arg(format!(
            "wav,id=audio0,path={},timer-period=5000",
            audio_wav.display()
        ))
        .arg("-device")
        .arg("virtio-sound-pci,audiodev=audio0,streams=1")
        // virtio-console on stdio is the primary I/O channel; UART goes to
        // a temp file so early-boot logs and panic fallback still land
        // somewhere when the kernel switches backends.
        .arg("-serial")
        .arg("file:/tmp/toyos-test-uart-early.log")
        .arg("-chardev")
        .arg("stdio,id=cs0,signal=off")
        .arg("-device")
        .arg("virtio-serial-pci-non-transitional,id=virtio-serial0,max_ports=1")
        .arg("-device")
        .arg("virtconsole,chardev=cs0,id=console0")
        .arg("-no-reboot");

    if options.gdb_stub {
        qemu.arg("-s");
    }

    qemu
}

fn spawn_and_wait_ready(mut qemu: Command, no_timeout: bool, audio_wav: PathBuf) -> QemuInstance {
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
        audio_wav,
    }
}
