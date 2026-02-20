use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use core::arch::asm;

const SYS_WRITE: u64 = 0;
const SYS_READ: u64 = 1;

fn syscall_write(buf: *const u8, len: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_WRITE => ret,
            in("rdi") buf,
            in("rsi") len,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

fn syscall_read(buf: *mut u8, len: usize) -> isize {
    let ret: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") SYS_READ => ret,
            in("rdi") buf,
            in("rsi") len,
            out("rcx") _,
            out("r11") _,
        );
    }
    ret
}

pub struct Stdin;
pub struct Stdout;
pub type Stderr = Stdout;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = syscall_read(buf.as_mut_ptr(), buf.len());
        if n < 0 { Err(io::Error::new(io::ErrorKind::Other, "toyos io error")) } else { Ok(n as usize) }
    }

    fn read_buf(&mut self, _cursor: BorrowedCursor<'_>) -> io::Result<()> {
        Ok(())
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let buf = match bufs.first_mut() {
            Some(b) => b,
            None => return Ok(0),
        };
        self.read(buf)
    }

    fn is_read_vectored(&self) -> bool {
        false
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = syscall_write(buf.as_ptr(), buf.len());
        if n < 0 { Err(io::Error::new(io::ErrorKind::Other, "toyos io error")) } else { Ok(n as usize) }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            total += self.write(buf)?;
        }
        Ok(total)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 64;

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}

pub fn panic_output() -> Option<Vec<u8>> {
    Some(Vec::new())
}
