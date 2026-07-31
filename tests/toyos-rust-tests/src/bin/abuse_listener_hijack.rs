//! A listener descriptor must name the listener it was made for, not a string
//! that a later process can bind.
//!
//! `Descriptor::Listener` carried the service *name*, and accept, close and
//! poll all re-resolved that string through the global registry. So the whole
//! attack is `listen(name)`, `dup`, `close(original)`: the close unregisters
//! the name and leaves the dup naming nothing. When the real service then
//! claims the freed name its own `listen` succeeds — its "already running"
//! check passes — and from that moment the stale fd resolves to *its*
//! listener. `accept` on it takes connections meant for the service, and
//! `close` on it unregisters the service.
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
    // Squat the name, then hand the registry back while keeping a descriptor.
    let listener = syscall::listen(NAME).expect("listen on an unclaimed name");
    let stale = syscall::dup(listener).expect("dup the listener fd");
    syscall::close(listener);

    // The real service claims what looks like a free name.
    let mut server = Command::new(SELF_PATH)
        .arg("server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn server");
    let mut out = BufReader::new(server.stdout.take().expect("server stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("server ready line");
    assert_eq!(line.trim(), "ready", "the service did not come up: {line:?}");

    // A client connects to the service by name and leaves a message queued.
    let status = Command::new(SELF_PATH)
        .arg("client")
        .arg(SECRET_1)
        .status()
        .expect("spawn first client");
    assert!(status.success(), "first client exited {status:?}");

    // The stale descriptor must not resolve to the service's listener. There
    // is a queued connection, so an unfixed kernel returns it immediately —
    // this cannot pass by blocking.
    match syscall::accept(stale) {
        Ok(accepted) => {
            let mut buf = [0u8; 128];
            let n = syscall::read(accepted.fd, &mut buf).unwrap_or(0);
            panic!(
                "hijacked the service's listener: accepted a connection from pid {} carrying {:?}",
                accepted.client_pid,
                String::from_utf8_lossy(&buf[..n]).trim(),
            );
        }
        Err(SyscallError::NotFound) => {}
        Err(e) => panic!("accept on a stale listener fd: expected NotFound, got {e:?}"),
    }

    // Closing the stale descriptor must not unregister the live service.
    syscall::close(stale);

    let mut stdin = server.stdin.take().expect("server stdin");
    let got = ask_server(&mut stdin, &mut out, "accept");
    assert_eq!(got, format!("got {SECRET_1}"), "the service lost its first client");

    // …and the name still resolves, for a client that connects after the
    // close. A client whose `connect` fails exits non-zero.
    let status = Command::new(SELF_PATH)
        .arg("client")
        .arg(SECRET_2)
        .status()
        .expect("spawn second client");
    assert!(
        status.success(),
        "second client exited {status:?} — closing the stale fd unregistered the service"
    );

    let got = ask_server(&mut stdin, &mut out, "accept");
    assert_eq!(got, format!("got {SECRET_2}"), "the service lost its second client");

    writeln!(stdin, "quit").expect("tell the server to quit");
    drop(stdin);
    let status = server.wait().expect("wait server");
    assert!(status.success(), "server exited {status:?}");

    println!("stale listener fd refused; the service kept its name and both clients");
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

fn server() {
    let listener = syscall::listen(NAME).expect("service: the name must be free");
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
