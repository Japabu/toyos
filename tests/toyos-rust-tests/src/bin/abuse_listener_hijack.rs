//! The real service is never handed a name somebody still holds a live fd on.
//!
//! The original defect: `Descriptor::Listener` carried the service *name*, and
//! accept, close and poll all re-resolved that string through the global
//! registry. The attack was `listen(name)`, `dup`, `close(original)` — the
//! close unregistered the name and left the dup naming nothing, so when the
//! real service claimed the freed name its own `listen` succeeded, and from
//! that moment the stale fd resolved to *its* listener: `accept` on it took
//! the service's connections and `close` on it unregistered the service.
//! `e42532f` gave the descriptor a `ListenerId` instead — never reused, so a
//! stale fd names nothing forever.
//!
//! **The attack's setup no longer exists**, and that is what this file now
//! certifies. A listener is refcounted by the descriptors naming it, so
//! `close` on one of two unregisters nothing and the real service's `listen`
//! is refused: the squat is loud instead of silent, and the service knows the
//! name is taken rather than being handed a name under a stranger's fd. The
//! `ListenerId` remains the second line and is no longer reachable to test —
//! nothing can now produce a descriptor whose listener is gone.
//!
//! Run as `... server` this binary is the service; as `... client <secret>` it
//! is one of its clients.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use toyos_abi::syscall::{self, SyscallError};

const NAME: &str = "abuse-listener-hijack";
const SELF_PATH: &str = "/bin/test_rs_abuse_listener_hijack";
const SECRET_1: &str = "first-client-payload";
const SECRET_2: &str = "second-client-payload";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("server") => server(),
        Some("client") => client(&args.next().expect("client role needs a secret")),
        Some(other) => panic!("unknown role {other:?}"),
        None => attacker(),
    }
}

fn attacker() {
    // Squat the name, then try to hand the registry back while keeping a
    // descriptor. The close is what used to unregister it.
    let listener = syscall::listen(NAME).expect("listen on an unclaimed name");
    let stale = syscall::dup(listener).expect("dup the listener fd");
    syscall::close(listener);

    // The real service must be told the name is taken. A kernel that lets this
    // succeed has handed it a name a stranger holds a live descriptor on, and
    // every assertion after this one is about the damage that does.
    let mut server = Command::new(SELF_PATH)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let mut out = BufReader::new(server.stdout.take().expect("server stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("server first line");
    assert_eq!(
        line.trim(),
        "taken",
        "the service claimed a name the attacker still holds a descriptor on: {line:?}"
    );
    drop(server.stdin.take());
    assert!(server.wait().expect("wait server").success(), "server exited nonzero");

    // The attacker's own descriptor is what holds the name, so it serves —
    // this is a squat, which `SYS_LISTEN` has no namespace to prevent (known
    // issues §1), and not a hijack of somebody else's listener.
    let status = Command::new(SELF_PATH)
        .arg("client")
        .arg(SECRET_1)
        .status()
        .expect("spawn first client");
    assert!(status.success(), "first client exited {status:?}");
    let accepted = syscall::accept(stale).expect("the surviving descriptor must still serve");
    let mut buf = [0u8; 128];
    let n = syscall::read(accepted.fd, &mut buf).expect("read the client's message");
    assert_eq!(String::from_utf8_lossy(&buf[..n]).trim(), SECRET_1);
    syscall::close(accepted.fd);

    // The last descriptor releases the name, and only then.
    syscall::close(stale);

    let mut server = Command::new(SELF_PATH)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let mut out = BufReader::new(server.stdout.take().expect("server stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("server ready line");
    assert_eq!(
        line.trim(),
        "ready",
        "the name was still bound after its last descriptor closed: {line:?}"
    );

    let status = Command::new(SELF_PATH)
        .arg("client")
        .arg(SECRET_2)
        .status()
        .expect("spawn second client");
    assert!(status.success(), "second client exited {status:?}");

    let mut stdin = server.stdin.take().expect("server stdin");
    let got = ask_server(&mut stdin, &mut out, "accept");
    assert_eq!(got, format!("got {SECRET_2}"), "the service lost its client");

    writeln!(stdin, "quit").expect("tell the server to quit");
    drop(stdin);
    let status = server.wait().expect("wait server");
    assert!(status.success(), "server exited {status:?}");

    println!("the squatted name stayed the squatter's while it held an fd, and freed on the last close");
}

fn ask_server(
    stdin: &mut std::process::ChildStdin,
    out: &mut BufReader<std::process::ChildStdout>,
    cmd: &str,
) -> String {
    writeln!(stdin, "{cmd}").expect("write server command");
    stdin.flush().expect("flush server command");
    let mut line = String::new();
    out.read_line(&mut line).expect("read server reply");
    line.trim().to_string()
}

/// Reports what `listen` said and then serves, so the attacker can assert on
/// the answer rather than on the service dying.
fn server() {
    let listener = match syscall::listen(NAME) {
        Ok(fd) => fd,
        Err(SyscallError::AlreadyExists) => {
            println!("taken");
            std::io::stdout().flush().expect("flush taken");
            return;
        }
        Err(e) => panic!("service: listen said {e:?}"),
    };
    println!("ready");
    std::io::stdout().flush().expect("flush ready");

    let stdin = std::io::stdin();
    let mut cmd = String::new();
    loop {
        cmd.clear();
        if stdin.lock().read_line(&mut cmd).expect("service: read command") == 0 {
            break;
        }
        match cmd.trim() {
            "accept" => {
                let accepted = syscall::accept(listener).expect("service: accept");
                let mut buf = [0u8; 128];
                let n = syscall::read(accepted.fd, &mut buf).expect("service: read client");
                println!("got {}", String::from_utf8_lossy(&buf[..n]).trim());
                std::io::stdout().flush().expect("service: flush reply");
                syscall::close(accepted.fd);
            }
            "quit" => break,
            other => panic!("service: unknown command {other:?}"),
        }
    }
    syscall::close(listener);
}

fn client(secret: &str) {
    let sock = syscall::connect(NAME).expect("client: connect by name");
    let msg = format!("{secret}\n");
    syscall::write(sock, msg.as_bytes()).expect("client: write");
    syscall::close(sock);
}
