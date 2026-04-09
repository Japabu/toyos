use crate::FromEnvErrorInner;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::process::Command;
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};

#[derive(Debug)]
pub struct Client {
    read: File,
    write: File,
    read_fd: RawFd,
    write_fd: RawFd,
}

#[derive(Debug)]
pub struct Acquired {
    byte: u8,
}

impl Client {
    pub fn new(mut limit: usize) -> io::Result<Client> {
        let fds = toyos_abi::syscall::pipe();
        let read_fd = fds.read.0;
        let write_fd = fds.write.0;

        let read = unsafe { File::from_raw_fd(read_fd) };
        let write = unsafe { File::from_raw_fd(write_fd) };

        // Write tokens into the pipe
        const BUFFER: [u8; 128] = [b'|'; 128];
        let mut w = &write;
        while limit > 0 {
            let n = limit.min(BUFFER.len());
            w.write_all(&BUFFER[..n])?;
            limit -= n;
        }

        Ok(Client {
            read,
            write,
            read_fd,
            write_fd,
        })
    }

    pub(crate) unsafe fn open(s: &str, _check_pipe: bool) -> Result<Client, FromEnvErrorInner> {
        let mut parts = s.splitn(2, ',');
        let read = parts.next().unwrap();
        let write = match parts.next() {
            Some(w) => w,
            None => {
                return Err(FromEnvErrorInner::CannotParse(format!(
                    "expected `R,W`, found `{s}`"
                )))
            }
        };
        let read_fd: RawFd = read
            .parse()
            .map_err(|e| FromEnvErrorInner::CannotParse(format!("cannot parse `read` fd: {e}")))?;
        let write_fd: RawFd = write.parse().map_err(|e| {
            FromEnvErrorInner::CannotParse(format!("cannot parse `write` fd: {e}"))
        })?;

        if read_fd < 0 {
            return Err(FromEnvErrorInner::NegativeFd(read_fd));
        }
        if write_fd < 0 {
            return Err(FromEnvErrorInner::NegativeFd(write_fd));
        }

        // Take ownership of the inherited fds
        Ok(Client {
            read: File::from_raw_fd(read_fd),
            write: File::from_raw_fd(write_fd),
            read_fd,
            write_fd,
        })
    }

    pub fn acquire(&self) -> io::Result<Acquired> {
        let mut buf = [0];
        let mut read = &self.read;
        loop {
            match read.read(&mut buf) {
                Ok(1) => return Ok(Acquired { byte: buf[0] }),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "early EOF on jobserver pipe",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn try_acquire(&self) -> io::Result<Option<Acquired>> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub fn release(&self, data: Option<&Acquired>) -> io::Result<()> {
        let byte = data.map(|d| d.byte).unwrap_or(b'+');
        match (&self.write).write(&[byte])? {
            1 => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to write token back to jobserver",
            )),
        }
    }

    pub fn string_arg(&self) -> String {
        format!("{},{}", self.read_fd, self.write_fd)
    }

    pub fn available(&self) -> io::Result<usize> {
        Ok(0)
    }

    pub fn configure(&self, cmd: &mut Command) {
        use std::os::toyos::process::CommandExt;
        // Pass the jobserver pipe fds to the child process
        cmd.inherit_fd(self.read_fd as u32, self.read_fd as u32);
        cmd.inherit_fd(self.write_fd as u32, self.write_fd as u32);
    }
}

#[derive(Debug)]
pub struct Helper {
    thread: JoinHandle<()>,
}

pub(crate) fn spawn_helper(
    client: crate::Client,
    state: Arc<super::HelperState>,
    mut f: Box<dyn FnMut(io::Result<crate::Acquired>) + Send>,
) -> io::Result<Helper> {
    let thread = Builder::new().spawn(move || {
        state.for_each_request(|_| f(client.acquire()));
    })?;

    Ok(Helper { thread })
}

impl Helper {
    pub fn join(self) {
        drop(self.thread.join());
    }
}
