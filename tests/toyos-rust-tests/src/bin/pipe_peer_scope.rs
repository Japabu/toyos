//! This test passes because a hole is open, and goes red when it is closed.
//!
//! `be604ef` gated `SYS_PIPE_OPEN` and named its own residual: a process
//! holding a socket to a pipe's creator may open *any* pipe that creator ever
//! makes, including ones meant for a different client. `may_open_pipe`'s third
//! clause is a property of the pair (caller, creator), and the fact it is
//! missing is *which* pipe — which only the creator knows and only a message
//! payload carries. No check the kernel can compute narrows it.
//!
//! So this asserts the residual is still there. It is the standing record that
//! the hole is real rather than theoretical, and it fails loudly on the day
//! `SYS_HANDLE_SEND` (`specs/capability-handles-spec.md` §8) makes the id
//! unnecessary. Read the panic message before "fixing" it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use toyos_abi::syscall::{self, SyscallError};

const SELF_PATH: &str = "/bin/test_rs_pipe_peer_scope";
const NAME: &str = "pipe-peer-scope";
const NOT_FOR_YOU: &[u8] = b"meant-for-a-different-client\n";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("creator") {
        return creator();
    }

    let mut creator = Command::new(SELF_PATH)
        .arg("creator")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn creator");
    let mut out = BufReader::new(creator.stdout.take().expect("creator stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("creator pipe id");
    let pipe_id: u64 = line.trim().parse().expect("pipe id");

    // The whole of this process's claim on that pipe: it connected to the
    // creator's service by name. The id never travelled over that connection.
    let sock = syscall::connect(NAME).expect("connect to the creator's service");

    match syscall::pipe_open(pipe_id, 0) {
        Ok(fd) => {
            let mut buf = [0u8; 128];
            let n = syscall::read(fd, &mut buf).expect("read the peer's pipe");
            assert_eq!(
                &buf[..n],
                NOT_FOR_YOU,
                "opened the peer's pipe but read something unexpected"
            );
            syscall::close(fd);
        }
        Err(SyscallError::PermissionDenied) => panic!(
            "GOOD NEWS, BAD TEST: `SYS_PIPE_OPEN` now refuses a peer that was never \
             handed this id, so be604ef's residual is closed. Delete this file and \
             assert the refusal instead — see specs/capability-handles-spec.md §8."
        ),
        Err(e) => panic!("unexpected {e:?}"),
    }

    syscall::close(sock);
    let mut cin = creator.stdin.take().expect("creator stdin");
    writeln!(cin, "quit").expect("tell the creator to quit");
    drop(cin);
    assert!(creator.wait().expect("wait creator").success(), "creator exited nonzero");

    println!("residual still open: a socket to the creator reaches every pipe it makes");
}

fn creator() {
    let listener = syscall::listen(NAME).expect("creator: listen");
    let p = syscall::pipe();
    syscall::write(p.write, NOT_FOR_YOU).expect("creator: seed the pipe");
    let id = syscall::pipe_id(p.read).expect("creator: pipe_id");
    println!("{id}");
    std::io::stdout().flush().expect("creator: flush");

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    syscall::close(p.read);
    syscall::close(p.write);
    syscall::close(listener);
}
