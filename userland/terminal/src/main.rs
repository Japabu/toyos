mod console;
mod framebuffer;

use std::io::{Read, Write};
use std::os::toyos::io::{self, AsRawFd};
use std::os::toyos::process;
use std::process::Command;

use window::Window;

fn main() {
    let mut window = Window::create(0, 0);
    io::set_screen_size(window.width(), window.height());
    let fb = framebuffer::Framebuffer::new(
        window.buffer_ptr() as u64,
        window.width(),
        window.height(),
        window.width(),
        window.pixel_format(),
    );
    let font_data = std::fs::read("/initrd/font.bin").expect("failed to read font");
    let font = font::Font::new(&font_data);
    let mut console = console::Console::new(fb, font);

    let mut child = Command::new("/initrd/shell")
        .stdin(process::tty_piped())
        .stdout(process::tty_piped())
        .spawn()
        .expect("failed to spawn shell");

    let mut shell_stdin = child.stdin.take().unwrap();
    let mut shell_stdout = child.stdout.take().unwrap();
    let shell_stdout_fd = shell_stdout.as_raw_fd();

    loop {
        let ready = io::poll(&[shell_stdout_fd]);

        if ready.fd(0) {
            let mut buf = [0u8; 4096];
            let n = shell_stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            console.write_bytes(&buf[..n]);
            std::io::stdout().lock().write_all(&buf[..n]).ok();
            window.present();
        }

        if ready.messages() {
            match window.recv_event() {
                window::Event::KeyInput(event) => {
                    shell_stdin.write_all(&event.bytes[..event.len as usize]).ok();
                }
                window::Event::Close => break,
                window::Event::Resized => {
                    io::set_screen_size(window.width(), window.height());
                    let fb = framebuffer::Framebuffer::new(
                        window.buffer_ptr() as u64,
                        window.width(),
                        window.height(),
                        window.width(),
                        window.pixel_format(),
                    );
                    console.resize(fb);
                    window.present();
                }
            }
        }
    }

    drop(shell_stdin);
    drop(shell_stdout);
    child.wait().ok();
}
