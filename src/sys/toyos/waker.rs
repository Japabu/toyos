use crate::sys::Selector;
use crate::Token;
use std::io;
use toyos_abi::syscall::Fd;

#[derive(Debug)]
pub struct Waker {
    write_fd: u64,
}

impl Waker {
    pub fn new(selector: &Selector, token: Token) -> io::Result<Waker> {
        let pipe = toyos_abi::syscall::pipe();
        selector.register_fd(pipe.read.0, token, crate::Interest::READABLE)?;
        Ok(Waker {
            write_fd: pipe.write.0,
        })
    }

    pub fn wake(&self) -> io::Result<()> {
        let _ = toyos_abi::syscall::write_nonblock(Fd(self.write_fd), &[1]);
        Ok(())
    }
}
