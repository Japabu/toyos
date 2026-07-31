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

/// The hardware shape QEMU presents to the guest.
///
/// Not a display setting: each variant is a whole machine. `Headless` is the
/// historical test config -- no VGA and no GPU device at all, so firmware
/// publishes no GOP and `kernel_args.gop_framebuffer` is zero. `Gop` swaps in
/// `-vga std` so firmware publishes a linear framebuffer, which is the path a
/// laptop takes and the only one in which the on-screen panic console renders
/// anything. `Metal` goes the whole way to the target laptop's shape.
#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    Headless,
    Gop,
    /// M1 metal-sim: GOP, NVMe, xHCI with the boot stick on it, i8042 from
    /// q35, and nothing else -- no virtio device, no USB HID, no 16550. The
    /// guest has no channel out but the framebuffer, so a boot under this
    /// profile is observed with [`QemuInstance::screendump_while`] and
    /// nothing else. Keeping a serial port here would defeat the point: the
    /// T14 has none, and a profile that talks over one is not simulating it.
    Metal,
}

impl Profile {
    fn virtio(self) -> bool {
        self != Profile::Metal
    }
}

pub struct BootOptions {
    pub gdb_stub: bool,
    pub debug_wait: bool,
    pub smp: u32,
    pub profile: Profile,
    /// Open a per-instance QMP socket, which `screendump` needs. Per-instance
    /// because screen tests boot their own QEMU and several may exist at once.
    pub qmp: bool,
    pub kernel_features: &'static [&'static str],
    /// Give the guest a USB keyboard. False is what makes an i8042 test
    /// measure anything: QEMU activates one input handler per device class,
    /// so with `usb-kbd` present every injected keystroke goes to it and a
    /// PS/2 test passes for the wrong reason. Ignored under
    /// [`Profile::Metal`], which never had one.
    pub usb_hid: bool,
    /// Give the machine an i8042 at all. `-machine q35,i8042=off` is the one
    /// absence scenario QEMU can stage.
    pub i8042: bool,
    /// The console line that means the boot reached the state under test.
    /// Anything other than [`DEFAULT_READY`] also declares that a panic is the
    /// expected outcome rather than a boot failure -- the early-panic screen
    /// test never reaches userland at all. Ignored under [`Profile::Metal`],
    /// which has no console for a marker to arrive on.
    pub ready_marker: &'static str,
}

/// The in-guest test runner's startup marker.
pub const DEFAULT_READY: &str = "===READY===";

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            gdb_stub: false,
            debug_wait: false,
            smp: 2,
            profile: Profile::Headless,
            qmp: false,
            kernel_features: &[],
            usb_hid: true,
            i8042: true,
            ready_marker: DEFAULT_READY,
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
    uart_log: PathBuf,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
    boot_log: String,
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
        let mut features: Vec<&str> = options.kernel_features.to_vec();
        if options.debug_wait {
            features.push("debug-wait");
        }
        let disk = toyos_build::build::build_test_image(
            &repo,
            &config_path,
            &features,
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

        let qmp_socket = options.qmp.then(|| test_dir.join(format!("qmp-{seq}.sock")));
        if let Some(path) = &qmp_socket {
            let _ = fs::remove_file(path);
        }
        let screendump = test_dir.join(format!("screen-{seq}.ppm"));

        // Per-instance, not a fixed /tmp path: the audio gate boots dozens of
        // guests and a screen test waits on this file, so a shared one would
        // let instances read each other's early boot.
        let uart_log = test_dir.join(format!("uart-{seq}.log"));
        let _ = fs::remove_file(&uart_log);

        let qemu = qemu_command(
            &boot_image,
            &nvme_image,
            &audio_wav,
            &uart_log,
            qmp_socket.as_deref(),
            &options,
        );
        spawn_and_wait_ready(qemu, &options, audio_wav, uart_log, qmp_socket, screendump)
    }

    /// Capture the guest's scanout through QMP and return the decoded PPM.
    ///
    /// After a halt the guest is stopped, so the dump is stable. QEMU writes
    /// the file itself, so the only synchronization needed is the command's
    /// own reply.
    pub fn screendump(&mut self) -> super::screen::Ppm {
        let socket = self
            .qmp_socket
            .as_ref()
            .expect("screendump needs BootOptions { qmp: true }");
        let _ = fs::remove_file(&self.screendump);
        qmp_screendump(socket, &self.screendump);
        let bytes = fs::read(&self.screendump).expect("screendump: QEMU wrote no file");
        super::screen::Ppm::parse(&bytes)
    }

    /// Screendump until the decoded screen carries `needle`, or the timeout.
    ///
    /// The halt_all_cpus paths paint before they flush, so a marker on serial
    /// already proves the paint finished and one dump is enough. The panic
    /// handler's own path is the other way round — the drain is what emits the
    /// report, and the paint happens after it — so there the host has to look
    /// more than once.
    pub fn screendump_until(&mut self, needle: &str, timeout: Duration) -> super::screen::Ppm {
        self.screendump_while(timeout, Duration::from_millis(100), |dump| {
            dump.text().contains(needle)
        })
    }

    /// Screendump until `done`, or the timeout. The metal-sim profile has no
    /// console, so this is the only way to observe that boot at all — and
    /// what it watches for is a pixel pattern, not text.
    ///
    /// Returns the last dump either way; a caller that timed out gets the
    /// screen as its diagnostic, which under metal-sim is where the kernel's
    /// boot checkpoints and panic report are.
    pub fn screendump_while(
        &mut self,
        timeout: Duration,
        interval: Duration,
        done: impl Fn(&super::screen::Ppm) -> bool,
    ) -> super::screen::Ppm {
        let deadline = Instant::now() + timeout;
        loop {
            let dump = self.screendump();
            if done(&dump) || Instant::now() >= deadline {
                return dump;
            }
            thread::sleep(interval);
        }
    }

    /// Every console line the guest printed before the ready marker.
    ///
    /// The kernel's own boot lines sit in the log ring until the scheduler
    /// drains them, by which time the virtio-console is the backend — so the
    /// 16550 file holds only the bootloader, and this is the only place a
    /// host test can read what the kernel said while booting. Empty under
    /// [`Profile::Metal`], which has no console to say it on.
    pub fn boot_log(&self) -> &str {
        &self.boot_log
    }

    /// Everything the guest put on the 16550 before it switched to the
    /// virtio-console — the only record a guest that died early leaves.
    pub fn uart_log(&self) -> String {
        fs::read_to_string(&self.uart_log).unwrap_or_default()
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

    /// Send `command` and wait for `marker` on the console.
    ///
    /// For a guest that will never report `===TEST_END`, which is any guest
    /// the fatal path has run through: every CPU is halted by the time the
    /// marker arrives.
    pub fn command_until(&mut self, command: &str, marker: &str, timeout: Duration) -> bool {
        writeln!(self.stdin, "{command}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");
        let deadline = Instant::now() + timeout;
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.rx.recv_timeout(left) {
                Ok(line) if line.contains(marker) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    /// The QMP socket this instance opened. Injection needs it, and it needs
    /// `BootOptions { qmp: true }`.
    pub fn qmp_socket(&self) -> &Path {
        self.qmp_socket.as_ref().expect("qmp_socket needs BootOptions { qmp: true }")
    }

    pub fn run_test(&mut self, name: &str, timeout: Duration) -> TestResult {
        self.run_test_hooked(name, timeout, "", |_| {})
    }

    /// `run_test`, with `action` run once the guest prints `ready_line`.
    ///
    /// The hook is inside the read loop because that is the only place the
    /// two facts meet: the guest is holding the keyboard fd, and the host has
    /// not injected yet. A sleep would be a guess in both directions.
    pub fn run_test_hooked(
        &mut self,
        name: &str,
        timeout: Duration,
        ready_line: &str,
        action: impl FnOnce(&Path),
    ) -> TestResult {
        writeln!(self.stdin, "run {name}").expect("Failed to write to QEMU stdin");
        self.stdin.flush().expect("Failed to flush QEMU stdin");

        let mut action = Some(action);
        let mut fire = |line: &str, socket: Option<&PathBuf>| {
            if ready_line.is_empty() || !line.contains(ready_line) {
                return;
            }
            if let Some(action) = action.take() {
                action(socket.expect("run_test_hooked needs BootOptions { qmp: true }"));
            }
        };

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
                    fire(&line, self.qmp_socket.as_ref());
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
        let _ = fs::remove_file(&self.uart_log);
        let _ = fs::remove_file(&self.screendump);
        if let Some(socket) = &self.qmp_socket {
            let _ = fs::remove_file(socket);
        }
    }
}

/// A QMP session. Line-delimited JSON: greeting, `qmp_capabilities`, then
/// commands; the reply carrying `return` is the completion signal. A handful
/// of commands with fixed shapes does not justify a JSON dependency.
struct Qmp {
    stream: std::os::unix::net::UnixStream,
    pending: Vec<u8>,
}

impl Qmp {
    fn connect(socket: &Path) -> Self {
        use std::os::unix::net::UnixStream;
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(e) => {
                    assert!(
                        Instant::now() < deadline,
                        "qmp: cannot connect to {}: {e}",
                        socket.display()
                    );
                    thread::sleep(Duration::from_millis(50));
                }
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut qmp = Self { stream, pending: Vec::new() };
        qmp.await_reply("\"QMP\"");
        qmp.execute("{\"execute\":\"qmp_capabilities\"}");
        qmp
    }

    fn await_reply(&mut self, want: &str) {
        use std::io::Read;
        let start = Instant::now();
        loop {
            if let Some(pos) =
                self.pending.windows(want.len()).position(|w| w == want.as_bytes())
            {
                self.pending.drain(..pos + want.len());
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "qmp: no {want} in reply: {}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut buf = [0u8; 4096];
            let n = self.stream.read(&mut buf).expect("qmp: read failed");
            assert!(n > 0, "qmp: socket closed waiting for {want}");
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    fn execute(&mut self, command: &str) {
        self.stream.write_all(command.as_bytes()).unwrap();
        self.stream.write_all(b"\n").unwrap();
        self.await_reply("\"return\"");
    }
}

fn qmp_screendump(socket: &Path, out: &Path) {
    let mut qmp = Qmp::connect(socket);
    qmp.execute(&format!(
        "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":\"{}\"}}}}",
        out.display()
    ));
}

/// One `input-send-event` carrying every key transition in `events`, given as
/// `(qcode, down)`. Sending them as one command is what makes a chord like
/// Shift+B arrive as a chord rather than as a race.
pub fn qmp_send_keys(socket: &Path, events: &[(&str, bool)]) {
    let body: Vec<String> = events
        .iter()
        .map(|(qcode, down)| {
            format!(
                "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":{{\"type\":\"qcode\",\"data\":\"{qcode}\"}}}}}}"
            )
        })
        .collect();
    let mut qmp = Qmp::connect(socket);
    qmp.execute(&format!(
        "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{}]}}}}",
        body.join(",")
    ));
}

/// Relative pointer motion and/or a button transition, as one event batch.
pub fn qmp_send_mouse(socket: &Path, dx: i32, dy: i32, button: Option<(&str, bool)>) {
    let mut body: Vec<String> = Vec::new();
    if let Some((name, down)) = button {
        body.push(format!(
            "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"{name}\"}}}}"
        ));
    }
    if dx != 0 {
        body.push(format!(
            "{{\"type\":\"rel\",\"data\":{{\"axis\":\"x\",\"value\":{dx}}}}}"
        ));
    }
    if dy != 0 {
        body.push(format!(
            "{{\"type\":\"rel\",\"data\":{{\"axis\":\"y\",\"value\":{dy}}}}}"
        ));
    }
    if body.is_empty() {
        return;
    }
    let mut qmp = Qmp::connect(socket);
    qmp.execute(&format!(
        "{{\"execute\":\"input-send-event\",\"arguments\":{{\"events\":[{}]}}}}",
        body.join(",")
    ));
}

/// The argv `options` would launch QEMU with, built against placeholder
/// paths. A profile's claim about which devices exist is a claim about this
/// list and nothing else — no screendump can see a device that is present but
/// unused — so this is what a profile assertion has to read.
pub fn profile_argv(options: &BootOptions) -> Vec<String> {
    let p = Path::new("/nonexistent");
    qemu_command(p, p, p, p, None, options)
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn qemu_command(
    boot_image: &Path,
    nvme_image: &Path,
    audio_wav: &Path,
    uart_log: &Path,
    qmp_socket: Option<&Path>,
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
        .arg(if options.i8042 { "q35" } else { "q35,i8042=off" })
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
        .arg("-drive")
        .arg(format!(
            "if=none,id=nvme0,format=raw,file={}",
            nvme_image.display()
        ))
        .arg("-device")
        .arg("nvme,serial=deadbeef,drive=nvme0")
        .arg("-vga")
        .arg(match options.profile {
            Profile::Headless => "none",
            Profile::Gop | Profile::Metal => "std",
        })
        .arg("-display")
        .arg("none")
        .arg("-no-reboot");

    // The T14's keyboard is PS/2 and its touchpad I2C-HID; it has no USB HID
    // at all, so metal-sim must not have one either.
    if options.profile.virtio() && options.usb_hid {
        qemu.arg("-device").arg("usb-kbd,bus=xhci.0");
    }

    if options.profile.virtio() {
        qemu.arg("-netdev")
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
            .arg(format!("file:{}", uart_log.display()))
            .arg("-chardev")
            .arg("stdio,id=cs0,signal=off")
            .arg("-device")
            .arg("virtio-serial-pci-non-transitional,id=virtio-serial0,max_ports=1")
            .arg("-device")
            .arg("virtconsole,chardev=cs0,id=console0");
    } else {
        qemu.arg("-serial").arg("none");
    }

    if options.gdb_stub {
        qemu.arg("-s");
    }
    if let Some(socket) = qmp_socket {
        qemu.arg("-qmp")
            .arg(format!("unix:{},server,nowait", socket.display()));
    }

    qemu
}

fn spawn_and_wait_ready(
    mut qemu: Command,
    options: &BootOptions,
    audio_wav: PathBuf,
    uart_log: PathBuf,
    qmp_socket: Option<PathBuf>,
    screendump: PathBuf,
) -> QemuInstance {
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

    // A metal-sim guest has no console at all, so there is no marker to wait
    // for: the caller polls the framebuffer. Blocking here would only time out.
    let boot_log = if options.profile.virtio() {
        wait_for_ready(&mut child, &rx, options, &uart_log)
    } else {
        String::new()
    };

    QemuInstance {
        child,
        stdin,
        rx,
        _reader_thread: reader_thread,
        audio_wav,
        uart_log,
        qmp_socket,
        screendump,
        boot_log,
    }
}

/// Returns every line seen on the way to the marker — see [`QemuInstance::boot_log`].
fn wait_for_ready(
    child: &mut Child,
    rx: &Receiver<String>,
    options: &BootOptions,
    uart_log: &Path,
) -> String {
    let no_timeout = options.debug_wait;
    let ready = options.ready_marker;
    let panic_aborts = ready == DEFAULT_READY;
    let boot_timeout = Duration::from_secs(10);
    let start = Instant::now();
    let mut seen = String::new();
    loop {
        if !no_timeout && start.elapsed() > boot_timeout {
            let _ = child.kill();
            panic!("[qemu] Boot timed out waiting for {ready}");
        }
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(line) if line.contains(ready) => {
                seen.push_str(&line);
                seen.push('\n');
                if VERBOSE.load(Ordering::Relaxed) {
                    eprintln!("[qemu] Reached {ready}");
                }
                break;
            }
            Ok(ref line)
                if panic_aborts
                    && !no_timeout
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
            Ok(line) => {
                seen.push_str(&line);
                seen.push('\n');
                continue;
            }
            // A guest that dies before virtio-console init never reaches
            // stdio at all; the UART file is the only channel it has.
            Err(RecvTimeoutError::Timeout) => {
                if !panic_aborts
                    && fs::read_to_string(uart_log).is_ok_and(|s| s.contains(ready))
                {
                    break;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait();
                panic!("[qemu] QEMU died before {ready} (status: {status:?})");
            }
        }
    }
    seen
}
