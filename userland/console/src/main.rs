//! The machine's console: `/bin/shell` on the raw framebuffer, no compositor.
//!
//! It exists for a machine with no serial port. `--diag-boot` freezes the
//! kernel's boot log on the panel, which answers "how far did it get and what
//! did it say" and nothing else — every further question costs a reflash and a
//! photograph. This program answers questions by being asked them.
//!
//! Three things follow from that and are not incidental:
//!
//! - **It starts with the kernel's log in the scrollback.** Claiming
//!   `DEVICE_FRAMEBUFFER` stops `panic_console::boot_checkpoint` from ever
//!   painting again, so a console that merely cleared the screen would trade
//!   the diagnostic that works today for one that might. The log comes from
//!   `/log/kernel.log`, which is the same bytes: no syscall reads the
//!   kernel's ring and adding one is not this program's call.
//! - **A fatal panic still takes the screen back.** `render` ignores
//!   `SCREEN_OWNED_BY_USERLAND` entirely — only boot checkpoints honour it —
//!   so the report paints over whatever this program drew.
//!   `screen_console_panic` is the gate.
//! - **The emulator is `/bin/terminal`'s**, unchanged. `Console::new` always
//!   took a raw mapping; the compositor was never below it. This is the caller
//!   whose mapping is the scanout, so it is the one that pays for a read.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::toyos::process;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use terminal::Console;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::shm::SharedMemory;
use toyos::surface::{self, Delivery, Host, Notice};
use toyos::{gpu, FramebufferDev, Keyboard};
use window::Screen;

const FONT: &str = "/share/fonts/JetBrainsMono-Regular-8x16.font";

/// Where `log_file` puts one file per boot, each named for the wall clock at
/// the moment that boot's sink installed.
const KERNEL_LOG_DIR: &str = "/log";

/// How many of those files seed the screen, oldest first.
///
/// Two, and both halves of that are deliberate. The newest is this boot's,
/// which is what the owner is asking about. The one before it is either the
/// previous boot — what a machine that has just come back from a wedge needs on
/// the panel — or, when this boot filled a file and continued in `_0002`, the
/// earlier half of this boot's own log.
const SEED_FILES: usize = 2;

/// HID usage codes. `toyos_keymap::Translator` turns both into escape
/// sequences; this program consumes them before it asks.
const KEY_PAGE_UP: u8 = 0x4B;
const KEY_PAGE_DOWN: u8 = 0x4E;

/// The kernel's log ring is 64 KiB (`kernel/src/drivers/log_ring.rs`), so no
/// more than this was ever in it at one time. The line bound below is the one
/// that normally applies; this one bounds a file with no newlines in it.
const SEED_MAX_BYTES: usize = 64 * 1024;

fn main() {
    // This console *is* the root of its surface tree — there is no compositor
    // in this image — so it both owns the translator and serves the channel a
    // child asks for raw keys on.
    let mut name = [0u8; surface::MAX_NAME];
    let name = surface::service_name(std::process::id(), &mut name).to_string();
    let mut host = Host::listen(&name).expect("console: cannot serve its own surface name");
    let mut translator = window::configured_translator();

    // Spawned first so it initialises while the font loads, as `/bin/terminal`
    // does.
    let mut shell = Shell::spawn(&name);

    let fb_dev = match FramebufferDev::open() {
        Ok(dev) => dev,
        Err(e) => {
            // The same answer soundd and netd give for their absent device: a
            // console with no screen has nothing to report a failure *to*, and
            // a panic here would replace the boot log with a crash report.
            eprintln!("console: no framebuffer ({e:?}), exiting");
            return;
        }
    };
    let info = fb_dev.info().expect("console: framebuffer info");
    let shm = SharedMemory::map(info.token[0], info.stride as usize * info.height as usize * 4)
        .expect("console: the scanout token the framebuffer device just reported");
    let screen = Screen::new(
        shm.as_ptr(),
        info.width as usize,
        info.height as usize,
        info.stride as usize,
        info.pixel_format,
    );

    let font_data = std::fs::read(FONT).expect("console: failed to read the font");
    let font = font::Font::from_prebuilt(&font_data);
    let rows = info.height as usize / font.height();
    let cols = info.width as usize / font.width();
    // One row of overlap, so a paged-back screen still shares a line with the
    // one before it and the reader can tell where he is.
    let page_rows = rows.saturating_sub(1);
    let mut console = Console::new(screen, font);

    let seeded = seed_kernel_log(&mut console);
    present(info.width, info.height);

    let kb = Keyboard::open().expect("console: no keyboard device");
    // What the panel cost, on a machine whose only instrument is the panel.
    // The seed is the heaviest thing this program ever draws — a screenful of
    // log per scrolled row — so a boot that felt slow says so here.
    let (panel_bytes, blits) = console.screen_traffic();
    eprintln!(
        "console: ready {}x{} ({cols}x{rows} cells), kernel log {seeded} bytes, \
         panel {panel_bytes} bytes in {blits} blits",
        info.width, info.height
    );

    // The declared set: the shell's two output pipes, the keyboard, and this
    // console's own surface listener and its clients.
    let poller = Poller::new(3 + Host::POLL_HANDLES);
    const TOKEN_STDOUT: u64 = 0;
    const TOKEN_STDERR: u64 = 1;
    const TOKEN_KEYBOARD: u64 = 2;
    const TOKEN_LISTEN: u64 = 3;
    const TOKEN_CLIENT: u64 = 4;

    loop {
        poller.poll_add_fd(toyos::RawHandle(shell.stdout.as_raw_fd() as u32), IORING_POLL_IN, TOKEN_STDOUT);
        poller.poll_add_fd(toyos::RawHandle(shell.stderr.as_raw_fd() as u32), IORING_POLL_IN, TOKEN_STDERR);
        poller.poll_add(&kb, IORING_POLL_IN, TOKEN_KEYBOARD);
        poller.poll_add_fd(host.listener_fd(), IORING_POLL_IN, TOKEN_LISTEN);
        for fd in host.client_fds() {
            poller.poll_add_fd(fd, IORING_POLL_IN, TOKEN_CLIENT);
        }

        let mut ready = [false; 5];
        poller.wait(1, u64::MAX, |token| {
            if (token as usize) < ready.len() {
                ready[token as usize] = true;
            }
        });

        let mut painted = false;

        if ready[TOKEN_STDOUT as usize] {
            let mut buf = [0u8; 4096];
            match shell.stdout.read(&mut buf).unwrap_or(0) {
                0 => {
                    // A machine whose only console has exited is a machine that
                    // needs a reboot to be asked anything, which is the state
                    // this program exists to get out of. `exit` at the prompt
                    // is an ordinary thing to type.
                    shell.restart(&name);
                    console.write_bytes(b"\n[console] the shell exited; a new one is running\n");
                    painted = true;
                }
                n => {
                    console.write_bytes(&buf[..n]);
                    std::io::stdout().lock().write_all(&buf[..n]).ok();
                    painted = true;
                }
            }
        }

        if ready[TOKEN_STDERR as usize] {
            let mut buf = [0u8; 4096];
            let n = shell.stderr.read(&mut buf).unwrap_or(0);
            if n > 0 {
                console.write_bytes(&buf[..n]);
                std::io::stdout().lock().write_all(&buf[..n]).ok();
                painted = true;
            }
        }

        if ready[TOKEN_LISTEN as usize] {
            host.accept();
        }

        while let Some(notice) = host.poll() {
            match notice {
                // The root of this tree: nothing above to tell, so the re-read
                // happens here and is passed down to every other client.
                Notice::LayoutChanged => {
                    window::load_layout(&mut translator);
                    host.notify_layout();
                    eprintln!("console: keyboard layout is now {}", translator.layout());
                }
                Notice::Grabbed { pid } => {
                    eprintln!("console: pid {pid} has the keyboard until it exits")
                }
                Notice::Released { pid } => eprintln!("console: pid {pid} gave the keyboard back"),
                Notice::Dropped { pid, why } => eprintln!("console: dropping pid {pid} — {why}"),
            }
        }

        if ready[TOKEN_KEYBOARD as usize] {
            let mut events = [toyos_abi::input::RawKeyEvent { keycode: 0, modifiers: 0 }; 16];
            let buf = unsafe {
                std::slice::from_raw_parts_mut(
                    events.as_mut_ptr() as *mut u8,
                    std::mem::size_of_val(&events),
                )
            };
            // Non-blocking for the reason `Keyboard::read_nonblock` documents:
            // an event loop that can park on an empty queue is a frozen screen.
            let n = kb.read_nonblock(buf).unwrap_or(0);
            for &event in &events[..n / std::mem::size_of::<toyos_abi::input::RawKeyEvent>()] {
                // A client that asked for the keys gets the transition whole,
                // releases included, and the translator is left where it is.
                if host.deliver(event) == Delivery::Sent {
                    continue;
                }
                if !event.pressed() {
                    continue;
                }
                match event.keycode {
                    // Unchorded, unlike a windowed terminal's Shift+PageUp:
                    // nothing in this image reads PageUp, and most of what the
                    // panel has to show is the kernel log seeded above the
                    // prompt. A scrollback that needs a chord is one the owner
                    // does not reach for with a laptop in his hands.
                    KEY_PAGE_UP => {
                        console.scroll_view_up(page_rows);
                        painted = true;
                    }
                    KEY_PAGE_DOWN => {
                        console.scroll_view_down(page_rows);
                        painted = true;
                    }
                    usage => {
                        let text = translator.press(usage, window::KeyEvent::from(event).mods());
                        if !text.is_empty() {
                            shell.stdin.write_all(text.as_bytes()).ok();
                        }
                    }
                }
            }
        }

        if painted {
            present(info.width, info.height);
        }
    }
}

/// The newest [`SEED_FILES`] kernel logs on `/log`, oldest first.
///
/// By name, which is by time: `log_file` names each boot's file for the wall
/// clock in a form that sorts chronologically, and a boot's continuation parts
/// sort directly after the file they continue.
///
/// The one machine this orders wrongly is one whose RTC never answered, where
/// every boot is `unknown-NN` and `NN` is the lowest free index rather than an
/// increasing one — after sixteen such boots the indices wrap and the newest
/// name is no longer the newest boot. Naming the file the kernel is actually
/// writing would need a syscall to ask it, which is `SYS_QUERY`'s job and not
/// this program's to invent.
fn newest_kernel_logs() -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(KERNEL_LOG_DIR) else { return Vec::new() };
    let mut paths: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "log"))
        .collect();
    paths.sort();
    paths.split_off(paths.len().saturating_sub(SEED_FILES))
}

/// Push this boot's kernel log into the scrollback; returns the bytes written.
///
/// Reading a file rather than the ring itself is not a workaround: `log_file`
/// seeds the sink from the ring's *retained* window, so the file opens at this
/// boot's first line and carries everything up to the last idle pass. What it
/// cannot carry is anything logged after this program read it — for that the
/// owner has a shell and `cat` on the file this names, which is the whole point.
fn seed_kernel_log(console: &mut Console) -> usize {
    let mut log = Vec::new();
    for path in newest_kernel_logs() {
        if let Ok(bytes) = std::fs::read(&path) {
            log.extend_from_slice(&bytes);
        }
    }
    if log.is_empty() {
        // Never silently: a blank screen where the boot log used to be is the
        // one outcome that would make this program a downgrade.
        console.write_bytes(
            b"[console] no kernel log on /log - this machine has no /log, so the\n\
              [console] screen starts here rather than at the first boot line.\n\n",
        );
        return 0;
    }
    let tail = seed_tail(&log);
    console.write_bytes(tail);
    console.write_bytes(b"\n");
    tail.len()
}

/// The tail of `log` worth rendering.
///
/// `Console` keeps [`terminal::console::SCROLLBACK_ROWS`] rows and drops what
/// falls past them as it arrives, so an older line costs one full-screen scroll
/// to draw and is then thrown away. A line that wraps takes more than one row,
/// so this is a ceiling on what survives rather than an estimate of it.
fn seed_tail(log: &[u8]) -> &[u8] {
    let window = &log[log.len().saturating_sub(SEED_MAX_BYTES)..];
    let mut newlines = 0;
    for (i, &b) in window.iter().enumerate().rev() {
        if b == b'\n' {
            newlines += 1;
            if newlines > terminal::console::SCROLLBACK_ROWS {
                return &window[i + 1..];
            }
        }
    }
    window
}

/// One present per drained batch, never per byte. Free on a GOP framebuffer —
/// `gop.rs`'s `present_rect` is empty, because the scanout *is* the memory just
/// written — and one transfer per batch on virtio-gpu, the only backend where
/// it costs anything.
fn present(width: u32, height: u32) {
    gpu::present(0, 0, width, height).expect("console owns the framebuffer");
}

struct Shell {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
}

impl Shell {
    fn spawn(surface_name: &str) -> Shell {
        let mut child = Command::new("/bin/shell")
            .env(surface::HOST_ENV, surface_name)
            .stdin(process::tty_piped())
            .stdout(process::tty_piped())
            .stderr(process::tty_piped())
            .spawn()
            .expect("console: failed to spawn /bin/shell");
        Shell {
            stdin: child.stdin.take().expect("console: shell stdin"),
            stdout: child.stdout.take().expect("console: shell stdout"),
            stderr: child.stderr.take().expect("console: shell stderr"),
            child,
        }
    }

    fn restart(&mut self, surface_name: &str) {
        self.child.wait().ok();
        *self = Shell::spawn(surface_name);
    }
}
