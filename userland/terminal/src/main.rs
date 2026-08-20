//! A shell in a window: the compositor's keys in, the emulator's pixels out.
//!
//! It is a surface in both directions. Above it the compositor forwards whole
//! key transitions and `window::Window` holds this process's translator;
//! below it `toyos::surface::Host` lets a child ask for the transitions
//! instead of the bytes, which is what `locale detect` needs and what a
//! terminal writing only translated bytes into a pipe could never give it.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::toyos::process;
use std::os::toyos::process::CommandExt;
use std::process::Command;

use terminal::Console;
use toyos::poller::{Poller, READABLE};
use toyos::port;
use toyos::surface::{self, Delivery, Host, Notice};
use toyos::RawHandle;
use window::Window;

const TOKEN_STDOUT: u64 = 0;
const TOKEN_STDERR: u64 = 1;
const TOKEN_WINDOW: u64 = 2;
const TOKEN_LISTEN: u64 = 3;
const TOKEN_CLIENT: u64 = 4;

/// Give the compositor the cells the emulator just repainted, and nothing when
/// it repainted none.
///
/// The whole of why a typed character does not cost a window repaint: the
/// emulator already blits one cell into the shared buffer, and before this the
/// compositor was told only that *something* had changed and had to assume all
/// of it had.
fn present(console: &Console, window: &Window) {
    if let Some(damage) = console.take_damage() {
        window.present_damage(damage);
    }
}

fn main() {
    // **This terminal's surface is a port it makes, not a name it registers.**
    // One per instance: the connector goes into the namespace of the shell it
    // spawns and nowhere else, so nothing outside that subtree can reach this
    // terminal's keyboard and nothing inside it can name a service this
    // process was not given. `TOYOS_SURFACE` — an ambient string naming a
    // machine-wide service — is what that replaces.
    let (acceptor, connector) =
        port::create().expect("terminal: the kernel refused a port of its own");
    let mut host = Host::serve(acceptor);

    // Spawn shell first so it initializes while we load the font
    let mut child = shell(&connector);

    let mut window = Window::create_with_title(0, 0, "Terminal").unwrap_or_else(|e| {
        eprintln!("terminal: {e}");
        std::process::exit(1);
    });
    let font_data = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);
    let mut console = Console::new(window.screen(), font);

    let mut shell_stdin = child.stdin.take().unwrap();
    let mut shell_stdout = child.stdout.take().unwrap();
    let mut shell_stderr = child.stderr.take().unwrap();
    let poller = Poller::new(3 + Host::POLL_HANDLES);

    // The window exists and the shell's stdin is a pipe this process owns, so
    // from here a keystroke the compositor forwards has somewhere to land even
    // if the shell has not reached its first read. Before it, one is dropped
    // with no trace — which is what the desktop tests used to compensate for by
    // retyping against a clock, making every one of their verdicts a statement
    // about how long a desktop takes to come up on the host of the day
    // (`issues/design-debt/`).
    eprintln!("terminal: ready");

    loop {
        poller.watch_raw(RawHandle(shell_stdout.as_raw_fd() as u32), READABLE, TOKEN_STDOUT);
        poller.watch_raw(RawHandle(shell_stderr.as_raw_fd() as u32), READABLE, TOKEN_STDERR);
        poller.watch_raw(window.handle(), READABLE, TOKEN_WINDOW);
        poller.watch_raw(host.acceptor_handle(), READABLE, TOKEN_LISTEN);
        for client in host.client_handles() {
            poller.watch_raw(client, READABLE, TOKEN_CLIENT);
        }

        let mut ready = [false; 5];
        poller.wait(1, u64::MAX, |token| {
            if (token as usize) < ready.len() { ready[token as usize] = true; }
        });

        if ready[TOKEN_STDOUT as usize] {
            let mut buf = [0u8; 4096];
            let n = shell_stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            console.write_bytes(&buf[..n]);
            std::io::stdout().lock().write_all(&buf[..n]).ok();
            present(&console, &window);
        }

        if ready[TOKEN_STDERR as usize] {
            let mut buf = [0u8; 4096];
            let n = shell_stderr.read(&mut buf).unwrap_or(0);
            if n > 0 {
                console.write_bytes(&buf[..n]);
                std::io::stdout().lock().write_all(&buf[..n]).ok();
                present(&console, &window);
            }
        }

        if ready[TOKEN_LISTEN as usize] {
            host.accept();
        }

        while let Some(notice) = host.poll() {
            match notice {
                // Up, not down: the compositor is the root of the tree and
                // broadcasts to every window, this one included, so the
                // re-read arrives back through `Event::LayoutChanged`.
                Notice::LayoutChanged => window.notify_layout_changed(),
                Notice::Grabbed { client } => {
                    eprintln!("terminal: client {client} has the keyboard until it exits")
                }
                Notice::Released { client } => {
                    eprintln!("terminal: client {client} gave the keyboard back")
                }
                Notice::Dropped { client, why } => {
                    eprintln!("terminal: dropping client {client} — {why}")
                }
            }
        }

        if ready[TOKEN_WINDOW as usize] {
            match window.recv_event() {
                // A client holding the grab takes the transition whole, and
                // the translator is not advanced — see `Window::text`.
                window::Event::KeyInput(key) if host.deliver(key.into()) == Delivery::Sent => {}
                window::Event::KeyInput(key) if key.gui() && key.keycode == 0x06 => {
                    // Cmd+C: copy selection to clipboard
                    if let Some(text) = console.get_selection() {
                        window::clipboard_set(&text).ok();
                    }
                }
                window::Event::KeyInput(key) => {
                    let press = window.press(key);
                    if !press.text().is_empty() {
                        shell_stdin.write_all(press.text().as_bytes()).ok();
                    }
                }
                window::Event::LayoutChanged => host.notify_layout(),
                window::Event::ClipboardPaste(data) => {
                    shell_stdin.write_all(&data).ok();
                }
                window::Event::MouseInput(ev) => {
                    let col = ev.x as usize / console.font_width();
                    let row = ev.y as usize / console.font_height();
                    match ev.event_type {
                        window::MOUSE_PRESS if ev.changed == 1 => {
                            console.mouse_down(col, row);
                            present(&console, &window);
                        }
                        window::MOUSE_MOVE if ev.buttons & 1 != 0 => {
                            console.mouse_drag(col, row);
                            present(&console, &window);
                        }
                        window::MOUSE_RELEASE if ev.changed == 1 => {
                            if let Some(text) = console.mouse_up(col, row) {
                                window::clipboard_set(&text).ok();
                            }
                            present(&console, &window);
                        }
                        window::MOUSE_SCROLL => {
                            if ev.scroll < 0 {
                                console.scroll_view_up(1);
                            } else if ev.scroll > 0 {
                                console.scroll_view_down(1);
                            }
                            present(&console, &window);
                        }
                        _ => {}
                    }
                }
                window::Event::Close => break,
                window::Event::Resized => {
                    console.resize(window.screen());
                    present(&console, &window);
                }
                window::Event::Frame => {}
            }
        }
    }

    drop(shell_stdin);
    drop(shell_stdout);
    drop(shell_stderr);
    child.wait().ok();
}

/// Start the shell: `[programs.shell]`'s own row, plus this terminal's surface.
///
/// **The row is init's to build and the surface is this terminal's to give.**
/// A shell's authority is a decision the manifest makes, and until the launcher
/// existed a terminal could hand a child only what it held itself — so it
/// handed over a hand-written union of its own names. `provide` is the shape
/// that replaces it: the one connector no manifest can name travels from here,
/// everything else comes from the declaration, and a name this terminal happens
/// to hold is no longer a name its shell inherits.
fn shell(surface: &toyos::port::Connector) -> std::process::Child {
    let handed = surface
        .duplicate()
        .expect("terminal: the kernel refused a duplicate of its own surface connector");
    Command::new("/bin/shell")
        .provide(surface::SERVICE, handed.into_raw().0)
        .stdin(process::tty_piped())
        .stdout(process::tty_piped())
        .stderr(process::tty_piped())
        .spawn()
        .expect("failed to spawn shell")
}
