//! A process must not be able to attach to another process's pipe by guessing
//! its id.
//!
//! `PipeId`s are dense sequential integers from one kernel-wide counter, and
//! `SYS_PIPE_OPEN` / `SYS_SOCKET_CREATE` take a raw one. Unchecked, mode 0
//! hands the caller a reader (steal the peer's stream), mode 1 a writer (inject
//! into it), and `SYS_PIPE_MAP` on the resulting fd hands over the raw 2 MiB
//! ring page — `for id in 0.. { pipe_open(id, 0) }` is the entire attack.
//!
//! Run as `abuse_pipe_owner child`, this binary is the victim: it creates a
//! pipe, prints its id, and blocks on stdin so the pipe stays alive.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::RawHandle;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("child") {
        return child();
    }

    // Pipes this process created are its own business.
    let own = syscall::pipe().expect("a pipe of our own");
    let own_read_id = syscall::pipe_id(own.read).expect("pipe_id(own read)");
    let own_write_id = syscall::pipe_id(own.write).expect("pipe_id(own write)");
    let fd = syscall::pipe_open(own_read_id, 0).expect("a process must be able to open its own pipe");
    syscall::close(fd);

    // …and so is bundling two of them into a socket.
    let sock = syscall::socket_create(own_read_id, own_write_id)
        .expect("socket_create over pipes this process holds");
    syscall::close(sock);

    // Now the victim. It is a sibling: no shared creator, no IPC connection,
    // and this process holds no descriptor for its pipe.
    let mut victim = Command::new("/bin/test_rs_abuse_pipe_owner")
        .arg("child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn victim");

    let mut out = BufReader::new(victim.stdout.take().expect("victim stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("read victim pipe ids");
    let (read_id, write_id) = {
        let mut it = line.trim().split(' ');
        let r: u64 = it.next().expect("read id").parse().expect("read id");
        let w: u64 = it.next().expect("write id").parse().expect("write id");
        (r, w)
    };

    // The read end: stealing the victim's incoming stream.
    let err = syscall::pipe_open(read_id, 0)
        .err()
        .expect("opening a sibling's pipe for reading must be refused");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a foreign pipe read");

    // The write end: injecting into the victim's stream.
    let err = syscall::pipe_open(write_id, 1)
        .err()
        .expect("opening a sibling's pipe for writing must be refused");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a foreign pipe write");

    // The second route to the same pipes.
    let err = syscall::socket_create(read_id, write_id)
        .err()
        .expect("a socket over a sibling's pipes must be refused");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a foreign socket_create");

    // Half-foreign: one end ours, one end the victim's.
    let err = syscall::socket_create(own_read_id, write_id)
        .err()
        .expect("a socket with one foreign end must be refused");
    assert_eq!(err, SyscallError::PermissionDenied, "wrong error for a half-foreign socket_create");

    // Sweeping the low ids finds every pipe in the system — soundd's, the test
    // runner's, the victim's. None of them may be attachable. Pipes this
    // process holds a descriptor for are legitimately open (that includes the
    // victim's stdio, which this process created for it), so collect those
    // first by walking the fd table.
    let held: Vec<u64> = (0..64).filter_map(|f| syscall::pipe_id(RawHandle(f)).ok()).collect();
    let mut refused = 0;
    let mut vanished = 0;
    for id in 0..256u64 {
        if held.contains(&id) {
            continue;
        }
        match syscall::pipe_open(id, 0) {
            Ok(_) => panic!("pipe {id} was attachable by id"),
            Err(SyscallError::PermissionDenied) => refused += 1,
            Err(SyscallError::NotFound) => vanished += 1,
            Err(e) => panic!("pipe {id}: unexpected {e:?}"),
        }
    }
    assert!(refused > 0, "the sweep found no live foreign pipe to refuse — test is vacuous");

    // Release the victim and confirm normal pipe traffic still flows: the fds
    // this process passed the child are ordinary inherited pipes.
    drop(victim.stdin.take());
    let status = victim.wait().expect("wait victim");
    assert!(status.success(), "victim exited {status:?}");

    syscall::close(own.read);
    syscall::close(own.write);
    println!("foreign pipes refused ({refused} live, {vanished} dead), own pipes still usable");
}

fn child() {
    let p = syscall::pipe().expect("the pipe the attack is aimed at");
    let r = syscall::pipe_id(p.read).expect("pipe_id");
    let w = syscall::pipe_id(p.write).expect("pipe_id");
    println!("{r} {w}");
    std::io::stdout().flush().expect("flush");
    // Block until the parent closes our stdin, keeping the pipe alive.
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}
