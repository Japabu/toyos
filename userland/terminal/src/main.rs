mod console;
mod font;
mod framebuffer;

use std::io::{Read, Write};
use std::os::toyos::io::{self, AsRawFd};
use std::process::{Command, Stdio};

/// FramebufferInfo layout matches kernel's `fd::FramebufferInfo` (repr(C)).
#[repr(C)]
struct FramebufferInfo {
    addr: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
}

fn read_fb_info() -> FramebufferInfo {
    let mut info = FramebufferInfo {
        addr: 0,
        width: 0,
        height: 0,
        stride: 0,
        pixel_format: 0,
    };
    let buf = unsafe {
        std::slice::from_raw_parts_mut(
            &mut info as *mut FramebufferInfo as *mut u8,
            std::mem::size_of::<FramebufferInfo>(),
        )
    };
    let n = io::read_fd(3, buf);
    assert!(n == std::mem::size_of::<FramebufferInfo>(), "Failed to read framebuffer info from FD 3");
    info
}

fn main() {
    std::os::toyos::io::set_stdin_raw(true);

    let fb_info = read_fb_info();
    let fb = framebuffer::Framebuffer::new(
        fb_info.addr,
        fb_info.width,
        fb_info.height,
        fb_info.stride,
        fb_info.pixel_format,
    );
    let font = font::Font::new(include_bytes!(concat!(env!("OUT_DIR"), "/font.bin")));
    let mut console = console::Console::new(fb, font);

    let mut child = Command::new("/initrd/shell")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn shell");

    let mut shell_stdin = child.stdin.take().unwrap();
    let mut shell_stdout = child.stdout.take().unwrap();
    let shell_stdout_fd = shell_stdout.as_raw_fd();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    loop {
        let mask = io::poll(0, shell_stdout_fd);

        if mask & 1 != 0 {
            let mut buf = [0u8; 64];
            let n = stdin.lock().read(&mut buf).unwrap_or(0);
            if n > 0 {
                shell_stdin.write_all(&buf[..n]).ok();
            }
        }

        if mask & 2 != 0 {
            let mut buf = [0u8; 4096];
            let n = shell_stdout.read(&mut buf).unwrap_or(0);
            if n == 0 {
                break;
            }
            console.write_bytes(&buf[..n]);
            stdout.lock().write_all(&buf[..n]).ok();
        }
    }

    drop(shell_stdin);
    drop(shell_stdout);
    child.wait().ok();
}
