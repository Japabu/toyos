use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{Interest, Token};

static NEXT_SELECTOR_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Debug)]
struct Registration {
    fd: u64,
    interest: Interest,
    token: Token,
}

#[derive(Debug)]
struct SelectorInner {
    registrations: Vec<Registration>,
}

#[derive(Clone)]
pub struct Event {
    token: usize,
    flags: u8,
}

const FLAG_READABLE: u8 = 1;
const FLAG_WRITABLE: u8 = 2;

pub type Events = Vec<Event>;

#[derive(Debug)]
pub struct Selector {
    inner: Arc<Mutex<SelectorInner>>,
    id: usize,
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        Ok(Selector {
            inner: Arc::new(Mutex::new(SelectorInner {
                registrations: Vec::new(),
            })),
            id: NEXT_SELECTOR_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(Selector {
            inner: Arc::clone(&self.inner),
            id: self.id,
        })
    }

    pub fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        use toyos_abi::syscall::{Fd, POLL_READABLE, POLL_WRITABLE};

        events.clear();

        // Snapshot registrations under lock, then release before blocking
        let regs: Vec<Registration> = {
            let inner = self.inner.lock().unwrap();
            inner.registrations.clone()
        };

        if regs.is_empty() {
            if let Some(timeout) = timeout {
                toyos_abi::syscall::nanosleep(timeout.as_nanos() as u64);
            }
            return Ok(());
        }

        let len = regs.len().min(63);
        let poll_fds: Vec<Fd> = regs[..len]
            .iter()
            .map(|reg| {
                let mut val = reg.fd;
                if reg.interest.is_readable() {
                    val |= POLL_READABLE;
                }
                if reg.interest.is_writable() {
                    val |= POLL_WRITABLE;
                }
                Fd(val)
            })
            .collect();

        let timeout_nanos = timeout.map(|d| d.as_nanos() as u64);
        let result = toyos_abi::syscall::poll_timeout(&poll_fds, timeout_nanos);

        for (i, reg) in regs[..len].iter().enumerate() {
            if result.fd(i) {
                let flags = if reg.interest.is_readable() {
                    FLAG_READABLE
                } else {
                    0
                } | if reg.interest.is_writable() {
                    FLAG_WRITABLE
                } else {
                    0
                };

                // Merge with existing event for same token
                if let Some(existing) = events.iter_mut().find(|e| e.token == reg.token.0) {
                    existing.flags |= flags;
                } else {
                    events.push(Event {
                        token: reg.token.0,
                        flags,
                    });
                }
            }
        }

        Ok(())
    }

    pub fn register_fd(&self, fd: u64, token: Token, interest: Interest) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.registrations.push(Registration {
            fd,
            interest,
            token,
        });
        Ok(())
    }

    pub fn reregister_fd(&self, fd: u64, token: Token, interest: Interest) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(reg) = inner.registrations.iter_mut().find(|r| r.fd == fd) {
            reg.token = token;
            reg.interest = interest;
        } else {
            inner.registrations.push(Registration {
                fd,
                interest,
                token,
            });
        }
        Ok(())
    }

    pub fn deregister_fd(&self, fd: u64) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.registrations.retain(|r| r.fd != fd);
        Ok(())
    }

    #[cfg(debug_assertions)]
    pub fn id(&self) -> usize {
        self.id
    }
}

pub mod event {
    use super::*;
    use std::fmt;

    pub fn token(event: &Event) -> Token {
        Token(event.token)
    }

    pub fn is_readable(event: &Event) -> bool {
        event.flags & FLAG_READABLE != 0
    }

    pub fn is_writable(event: &Event) -> bool {
        event.flags & FLAG_WRITABLE != 0
    }

    pub fn is_error(_event: &Event) -> bool {
        false
    }

    pub fn is_read_closed(_event: &Event) -> bool {
        false
    }

    pub fn is_write_closed(_event: &Event) -> bool {
        false
    }

    pub fn is_priority(_event: &Event) -> bool {
        false
    }

    pub fn is_aio(_event: &Event) -> bool {
        false
    }

    pub fn is_lio(_event: &Event) -> bool {
        false
    }

    pub fn debug_details(f: &mut fmt::Formatter<'_>, event: &Event) -> fmt::Result {
        write!(
            f,
            "Event {{ token: {}, readable: {}, writable: {} }}",
            event.token,
            event.flags & FLAG_READABLE != 0,
            event.flags & FLAG_WRITABLE != 0
        )
    }
}
