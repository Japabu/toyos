//! A number another process published designates nothing here.
//!
//! This test used to sweep `SYS_PIPE_OPEN` over the dense machine-wide `PipeId`
//! space, because a pipe id *was* the authority: mode 0 handed the caller a
//! reader of somebody else's stream, mode 1 a writer into it, and
//! `SYS_SOCKET_CREATE` was the same reach under another name. That family is
//! retired and a pipe end travels as a handle.
//!
//! The property that replaced it is stronger and this is its test. **A handle
//! is a slot in one process's own table**, so a number lifted out of a
//! sibling's output resolves to this process's slot or to nothing at all — and
//! the attack the old shape describes is not expressible rather than refused.
//! The victim publishes its ends at slots this process is certain not to hold,
//! so `NotFound` here is "that table is not reachable" and not "that slot
//! happened to be empty".
//!
//! Run as `abuse_pipe_owner child`, this binary is the victim: it makes a pipe,
//! parks both ends at two high slots, publishes them, and blocks on stdin so
//! the pipe stays alive.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use toyos::AsHandle;
use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

/// Where the victim parks its ends. High, and nothing in this tree grows a
/// table that far: a process holding 900 handles would be a different bug.
const VICTIM_READ_SLOT: u16 = 900;
const VICTIM_WRITE_SLOT: u16 = 901;

/// The first slot the sweep touches. Below it are stdio and the pipes this
/// process made for the child, and reading one of those would consume bytes
/// their owner is waiting for.
const SWEEP_FROM: u32 = 16;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("child") {
        return child();
    }

    // Pipes this process created are its own business, and joining two of them
    // into one duplex object is too: `SYS_CONNECTION_JOIN` grants nothing,
    // because everything it reaches is already the caller's.
    let (own_read, own_write) = toyos::pipe_pair().expect("a pipe of our own");
    let joined = syscall::connection_join(own_read.as_handle(), own_write.as_handle())
        .expect("two ends this process holds must join");
    syscall::close(joined);
    own_write.write(b"round trip").expect("write our own pipe");
    let mut buf = [0u8; 16];
    let n = own_read.read(&mut buf).expect("read our own pipe");
    assert_eq!(&buf[..n], b"round trip", "our own pipe did not carry its own bytes");

    // Now the victim. It is a sibling: no shared creator, no IPC connection,
    // and this process holds no handle to its pipe.
    let mut victim = Command::new("/bin/test_rs_abuse_pipe_owner")
        .arg("child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn victim");

    let mut out = BufReader::new(victim.stdout.take().expect("victim stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("read the victim's handle values");
    let published: Vec<RawHandle> = line
        .trim()
        .split(' ')
        .map(|v| RawHandle(v.parse().expect("a handle value")))
        .collect();
    assert_eq!(published.len(), 2, "the victim publishes both ends");

    // Every way there is to reach a pipe by naming it, aimed at the victim's
    // numbers. All four answer the same thing, and it is not the victim's pipe.
    for h in &published {
        let mut buf = [0u8; 16];
        assert_eq!(
            syscall::read_nonblock(*h, &mut buf).err(),
            Some(SyscallError::NotFound),
            "handle {} read as if it named something",
            h.0
        );
        assert_eq!(
            syscall::write_nonblock(*h, b"injected").err(),
            Some(SyscallError::NotFound),
            "handle {} took a write",
            h.0
        );
        assert_eq!(
            syscall::pipe_map(*h).err(),
            Some(SyscallError::NotFound),
            "handle {} handed over a ring page",
            h.0
        );
        assert_eq!(
            syscall::connection_join(published[0], published[1]).err(),
            Some(SyscallError::NotFound),
            "a connection was joined out of a sibling's ends",
        );
    }

    // And no slot of this table is the victim's pipe either. The old sweep was
    // over machine-wide ids and had to find live foreign pipes to be worth
    // anything; this one is over the whole 12-bit slot space of *this* table,
    // where every answer is `NotFound` because nothing this process opened
    // reaches that high.
    let mut vacant = 0;
    for slot in SWEEP_FROM..RawHandle::MAX_SLOTS as u32 {
        let h = RawHandle(slot);
        let mut buf = [0u8; 16];
        match syscall::read_nonblock(h, &mut buf) {
            Err(SyscallError::NotFound) => vacant += 1,
            Ok(n) => panic!("slot {slot} read {n} bytes off something this process never opened"),
            Err(SyscallError::WouldBlock) => {}
            Err(SyscallError::PermissionDenied) => {}
            Err(e) => panic!("slot {slot}: unexpected {e:?}"),
        }
    }
    assert!(vacant > 4000, "only {vacant} slots were vacant — the sweep proves nothing");

    // Release the victim and confirm ordinary pipe traffic still flows: the
    // ends this process passed the child are inherited handles, and inheritance
    // is the only way one crossed.
    drop(victim.stdin.take());
    let status = victim.wait().expect("wait victim");
    assert!(status.success(), "victim exited {status:?}");

    println!("a sibling's handle values name nothing here ({vacant} slots vacant)");
}

fn child() {
    let (read, write) = toyos::pipe_pair().expect("the pipe the attack is aimed at");
    let parked_read =
        syscall::dup2(read.as_handle(), VICTIM_READ_SLOT).expect("park the read end");
    let parked_write =
        syscall::dup2(write.as_handle(), VICTIM_WRITE_SLOT).expect("park the write end");
    println!("{} {}", parked_read.0, parked_write.0);
    std::io::stdout().flush().expect("flush");
    // Block until the parent closes our stdin, keeping the pipe alive.
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}
