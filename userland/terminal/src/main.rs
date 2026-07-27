mod console;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::toyos::process;
use std::process::Command;

use toyos::gpu;
use toyos::poller::{Poller, IORING_POLL_IN};
use toyos::Fd;
use window::Window;

fn main() {
    // Spawn shell first so it initializes while we load the font
    let mut child = Command::new("/bin/shell")
        .stdin(process::tty_piped())
        .stdout(process::tty_piped())
        .stderr(process::tty_piped())
        .spawn()
        .expect("failed to spawn shell");

    let mut window = Window::create_with_title(0, 0, "Terminal");
    gpu::set_screen_size(window.width(), window.height());
    let fb = window.framebuffer();
    let font_data = std::fs::read("/share/fonts/JetBrainsMono-Regular-8x16.font").expect("failed to read font");
    let font = font::Font::from_prebuilt(&font_data);
    let mut console = console::Console::new(fb, font);

    let mut shell_stdin = child.stdin.take().unwrap();
    let mut shell_stdout = child.stdout.take().unwrap();
    let mut shell_stderr = child.stderr.take().unwrap();
    let poller = Poller::new(4);

    loop {
        poller.poll_add_fd(Fd(shell_stdout.as_raw_fd()), IORING_POLL_IN, 0);
        poller.poll_add_fd(Fd(shell_stderr.as_raw_fd()), IORING_POLL_IN, 1);
        poller.poll_add_fd(window.fd(), IORING_POLL_IN, 2);

        let mut ready = [false; 3];
        poller.wait(1, u64::MAX, |token| {
            if (token as usize) < 3 { ready[token as usize] = true; }
        });

        if ready[0] {
            let mut buf = [0u8; 4096];
            let n = shell_stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            console.write_bytes(&buf[..n]);
            std::io::stdout().lock().write_all(&buf[..n]).ok();
            window.present();
        }

        if ready[1] {
            let mut buf = [0u8; 4096];
            let n = shell_stderr.read(&mut buf).unwrap_or(0);
            if n > 0 {
                console.write_bytes(&buf[..n]);
                std::io::stdout().lock().write_all(&buf[..n]).ok();
                window.present();
            }
        }

        if ready[2] {
            match window.recv_event() {
                window::Event::KeyInput(event) => {
                    if event.gui() && event.keycode == 0x06 {
                        // Cmd+C: copy selection to clipboard
                        if let Some(text) = console.get_selection() {
                            window::clipboard_set(&text);
                        }
                    } else if event.len > 0 {
                        shell_stdin.write_all(&event.translated[..event.len as usize]).ok();
                    }
                }
                window::Event::ClipboardPaste(data) => {
                    shell_stdin.write_all(&data).ok();
                }
                window::Event::MouseInput(ev) => {
                    let col = ev.x as usize / console.font_width();
                    let row = ev.y as usize / console.font_height();
                    match ev.event_type {
                        window::MOUSE_PRESS if ev.changed == 1 => {
                            console.mouse_down(col, row);
                            window.present();
                        }
                        window::MOUSE_MOVE if ev.buttons & 1 != 0 => {
                            console.mouse_drag(col, row);
                            window.present();
                        }
                        window::MOUSE_RELEASE if ev.changed == 1 => {
                            if let Some(text) = console.mouse_up(col, row) {
                                window::clipboard_set(&text);
                            }
                            window.present();
                        }
                        window::MOUSE_SCROLL => {
                            let pixels = console.font_height();
                            if ev.scroll < 0 {
                                console.scroll_view_up(pixels);
                            } else if ev.scroll > 0 {
                                console.scroll_view_down(pixels);
                            }
                            window.present();
                        }
                        _ => {}
                    }
                }
                window::Event::Close => break,
                window::Event::Resized => {
                    gpu::set_screen_size(window.width(), window.height());
                    console.resize(window.framebuffer());
                    window.present();
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
