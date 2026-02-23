mod console;
mod font;
mod framebuffer;

use std::os::toyos::io;

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

    // Create pipes for shell communication
    let (shell_stdin_r, shell_stdin_w) = io::pipe();
    let (shell_stdout_r, shell_stdout_w) = io::pipe();

    // Spawn shell with pipes
    let shell_pid = io::spawn(&["/initrd/shell"], shell_stdin_r, shell_stdout_w);
    io::close_fd(shell_stdin_r);
    io::close_fd(shell_stdout_w);

    // Event loop: multiplex keyboard input and shell output
    loop {
        let mask = io::poll(0, shell_stdout_r);

        if mask & 1 != 0 {
            let mut buf = [0u8; 64];
            let n = io::read_stdin_raw(&mut buf).unwrap_or(0);
            if n > 0 {
                io::write_fd(shell_stdin_w, &buf[..n]);
            }
        }

        if mask & 2 != 0 {
            let mut buf = [0u8; 4096];
            let n = io::read_fd(shell_stdout_r, &mut buf);
            if n == 0 {
                break;
            }
            console.write_bytes(&buf[..n]);
        }
    }

    io::close_fd(shell_stdin_w);
    io::close_fd(shell_stdout_r);
    io::waitpid(shell_pid);
}
