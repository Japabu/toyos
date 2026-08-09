//! The in-guest half of the layout and wizard gates, as a surface.
//!
//! This program is a **surface owner**, built out of exactly the pieces
//! `/bin/terminal` and `/bin/console` are: it claims the keyboard, holds one
//! `Translator`, serves `toyos::surface::Host`, and gives its child the
//! service name in the environment. What it does not have is a screen — so
//! every assertion the host makes reads a console line instead of a pixel,
//! which is why the layout and wizard gates run here and not against
//! `/bin/console`.
//!
//! One binary rather than two: each is ~1.8 MiB of statically linked std, and
//! the initrd goes into a FAT volume sized from its contents. Modes are
//! `run test_rs_locale_gate <mode>`, which the test runner has always
//! supported.
//!
//! In RUST_SKIP: every mode waits to be typed at through QMP, so on its own
//! nothing ever answers it.
//!
//! - `layout` — run the real `locale swiss-german`, which writes the config
//!   and tells this surface it moved, then print what every key types. Driven
//!   by `swiss_german_layout`.
//! - `detect` — run `locale detect` and relay its conversation while the
//!   wizard holds this surface's keys. **The keyboard is claimed here**, which
//!   is the shape the compositor and `/bin/console` put it in and the shape
//!   that used to make the wizard refuse. Driven by `locale_detect` and
//!   `locale_detect_unrecognized`.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use toyos::device::Keyboard;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::surface::{self, Delivery, Host, Notice};
use toyos_abi::input::RawKeyEvent;
use window::Translator;

const EVENT_SIZE: usize = std::mem::size_of::<RawKeyEvent>();

const TOKEN_KEYBOARD: u64 = 1;
const TOKEN_LISTEN: u64 = 2;
const TOKEN_CLIENT: u64 = 3;

fn main() {
    let mut buf = [0u8; surface::MAX_NAME];
    let name = surface::service_name(std::process::id(), &mut buf).to_string();
    let host = Host::listen(&name).expect("locale_gate: cannot serve its own surface name");
    let keyboard = Keyboard::open().expect("locale_gate: no keyboard device");
    let surface = Surface { host, keyboard, translator: window::configured_translator() };

    match std::env::args().nth(1).as_deref() {
        Some("layout") => layout(surface, &name),
        Some("detect") => detect(surface, &name),
        other => panic!("locale_gate: unknown mode {other:?}"),
    }
}

/// Everything a surface owner is, minus the screen.
struct Surface {
    host: Host,
    keyboard: Keyboard,
    translator: Translator,
}

impl Surface {
    /// Read the keyboard and hand every transition on: to the client holding
    /// the grab if there is one, and otherwise to `typed`, which is where a
    /// terminal would write the shell's stdin.
    fn drain_keyboard(&mut self, mut typed: impl FnMut(&window::KeyEvent, &str)) {
        let mut buf = [0u8; 512];
        let n = self.keyboard.read_nonblock(&mut buf).unwrap_or(0);
        for chunk in buf[..n].chunks_exact(EVENT_SIZE) {
            let event = RawKeyEvent { keycode: chunk[0], modifiers: chunk[1] };
            if self.host.deliver(event) == Delivery::Sent {
                continue;
            }
            let key = window::KeyEvent::from(event);
            let text = if key.pressed() {
                self.translator.press(key.keycode, key.mods())
            } else {
                window::Emit::EMPTY
            };
            typed(&key, text.as_str());
        }
    }

    /// The notices a surface owner acts on, with this one's logging.
    fn drain_notices(&mut self) {
        while let Some(notice) = self.host.poll() {
            match notice {
                Notice::LayoutChanged => {
                    window::load_layout(&mut self.translator);
                    self.host.notify_layout();
                    println!("surface: layout is now {}", self.translator.layout());
                }
                Notice::Grabbed { pid } => println!("surface: pid {pid} has the keys"),
                Notice::Released { pid } => println!("surface: pid {pid} gave the keys back"),
                Notice::Dropped { pid, why } => println!("surface: dropped pid {pid} — {why}"),
            }
        }
    }
}

fn spawn_locale(args: &[&str], surface_name: &str) -> Child {
    Command::new("/bin/toybox")
        .arg("locale")
        .args(args)
        .env(surface::HOST_ENV, surface_name)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("locale_gate: cannot run /bin/toybox")
}

fn layout(mut surface: Surface, name: &str) {
    // `Command::output()` asks `spawn` for the pipe and drops it, so its
    // stderr is always empty (`specs/issues/kernel/command-output-empty-stderr.md`).
    let out = spawn_locale(&["swiss-german"], name)
        .wait_with_output()
        .expect("locale_gate: locale never exited");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("locale: {line}");
    }
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        println!("locale-err: {line}");
    }

    // The child connected, said the config moved, and exited. Its frame is in
    // the pipe whether or not it is still running, so this is the same accept
    // and the same drain a terminal does inside its event loop.
    surface.host.accept();
    surface.drain_notices();
    println!("===SWISS_READY===");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut seen = 0;
    while Instant::now() < deadline {
        surface.drain_keyboard(|key, text| {
            println!("kev usage=0x{:02x} mods=0x{:02x} tr={:?}", key.keycode, key.modifiers, text);
            seen += 1;
        });
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("kev done seen={seen}");
}

fn detect(mut surface: Surface, name: &str) {
    let mut child = spawn_locale(&["detect"], name);
    let stdout = child.stdout.take().expect("locale_gate: no stdout pipe");

    // The relay is a thread doing blocking reads, and the surface runs here.
    //
    // Not two halves of one poll loop: the wizard's whole conversation is a
    // few hundred bytes and then a hang-up, so the interesting event is the
    // *end* of its output — and whether a pipe whose writer has gone reads
    // ready is a property of the kernel this test is not about. A blocking
    // read answers it directly.
    let wizard_done = Arc::new(AtomicBool::new(false));
    let relay_done = wizard_done.clone();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        // Bytes, not chars: the wizard's legends are `§` and the like, and a
        // byte pushed into a `String` as a `char` turns two UTF-8 bytes into
        // two Latin-1 ones — which reads as a mangled prompt on the host and
        // is a defect in this relay rather than in anything under test.
        let mut line: Vec<u8> = Vec::new();
        while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
            while line.last() == Some(&b'\n') || line.last() == Some(&b'\r') {
                line.pop();
            }
            println!("detect: {}", String::from_utf8_lossy(&line));
            line.clear();
        }
        relay_done.store(true, Ordering::Relaxed);
    });

    let poller = Poller::new(1 + Host::POLL_HANDLES);
    let deadline = Instant::now() + Duration::from_secs(25);
    while !wizard_done.load(Ordering::Relaxed) && Instant::now() < deadline {
        poller.poll_add(&surface.keyboard, IORING_POLL_IN, TOKEN_KEYBOARD);
        poller.poll_add_fd(surface.host.listener_fd(), IORING_POLL_IN, TOKEN_LISTEN);
        for fd in surface.host.client_fds() {
            poller.poll_add_fd(fd, IORING_POLL_IN, TOKEN_CLIENT);
        }

        let mut ready = [false; 4];
        poller.wait(1, 50_000_000, |token| {
            if (token as usize) < ready.len() {
                ready[token as usize] = true;
            }
        });

        if ready[TOKEN_LISTEN as usize] {
            surface.host.accept();
        }
        surface.drain_notices();

        // Keys are drained on every pass, ready or not: the wizard's grab
        // arrives between two of them, and a transition read before the grab
        // was granted would be translated into nothing anyone is reading.
        surface.drain_keyboard(|_, _| {});
    }
    if !wizard_done.load(Ordering::Relaxed) {
        println!("locale_gate: the wizard was still running after 25s");
    }
    println!("===DETECT_DRAINED===");

    let mut stderr = child.stderr.take().expect("locale_gate: no stderr pipe");
    child.wait().expect("locale_gate: the wizard never exited");
    let mut err = Vec::new();
    stderr.read_to_end(&mut err).ok();
    for line in String::from_utf8_lossy(&err).lines() {
        println!("detect-err: {line}");
    }
    println!("===DETECT_DONE===");
}
