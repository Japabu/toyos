//! The in-guest half of the layout and wizard gates, in three modes.
//!
//! One binary rather than three: each is ~1.8 MiB of statically linked std,
//! and the initrd goes into a FAT volume sized from its contents. Modes are
//! `run test_rs_locale_gate <mode>`, which the test runner has always
//! supported.
//!
//! In RUST_SKIP: every mode waits to be typed at through QMP, so on its own
//! nothing ever answers it.
//!
//! - `layout` — select `swiss-german` through the real command, then print
//!   every key event that arrives. Driven by `swiss_german_layout`.
//! - `detect` — run `locale detect` and relay its conversation. The wizard
//!   claims the keyboard, so this process must not. Driven by `locale_detect`
//!   and `locale_detect_unrecognized`.
//! - `busy` — hold the keyboard claim and *then* run `locale detect`, which is
//!   the shape the compositor and `/bin/console` put it in. Driven by
//!   `locale_detect_refuses_a_held_keyboard`.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use toyos::device::Keyboard;
use toyos_abi::input::RawKeyEvent;

const EVENT_SIZE: usize = std::mem::size_of::<RawKeyEvent>();

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("layout") => layout(),
        Some("detect") => detect(false),
        Some("busy") => detect(true),
        other => panic!("locale_gate: unknown mode {other:?}"),
    }
}

fn spawn_locale(args: &[&str]) -> std::process::Child {
    Command::new("/bin/toybox")
        .arg("locale")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("locale_gate: cannot run /bin/toybox")
}

fn layout() {
    // `Command::output()` asks `spawn` for the pipe and drops it, so its
    // stderr is always empty (known-issues §5).
    let out = spawn_locale(&["swiss-german"])
        .wait_with_output()
        .expect("locale_gate: locale never exited");
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        println!("locale: {line}");
    }
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        println!("locale-err: {line}");
    }

    let keyboard = Keyboard::open().expect("locale_gate: no keyboard device");
    println!("===SWISS_READY===");

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut buf = [0u8; 512];
    let mut seen = 0;
    while Instant::now() < deadline {
        let n = keyboard.read_nonblock(&mut buf).unwrap_or(0);
        if n == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        for chunk in buf[..n].chunks_exact(EVENT_SIZE) {
            let len = chunk[2] as usize;
            let translated = String::from_utf8_lossy(&chunk[3..3 + len.min(5)]).into_owned();
            println!("kev usage=0x{:02x} mods=0x{:02x} tr={:?}", chunk[0], chunk[1], translated);
            seen += 1;
        }
    }
    println!("kev done seen={seen}");
}

fn detect(hold_the_keyboard: bool) {
    let claim = hold_the_keyboard
        .then(|| Keyboard::open().expect("locale_gate: no keyboard device to hold"));

    let mut child = spawn_locale(&["detect"]);
    let stdout = child.stdout.take().expect("locale_gate: no stdout pipe");
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) => println!("detect: {line}"),
            Err(e) => {
                println!("detect: read failed: {e}");
                break;
            }
        }
    }
    println!("===DETECT_DRAINED===");

    let out = child.wait_with_output().expect("locale_gate: the wizard never exited");
    for line in String::from_utf8_lossy(&out.stderr).lines() {
        println!("detect-err: {line}");
    }
    drop(claim);
    println!("===DETECT_DONE===");
}
