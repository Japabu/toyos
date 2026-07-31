//! Being allowed to map a shared region must not carry the right to hand it on.
//!
//! `shared_memory::grant` accepted the caller if it was the owner *or already
//! in the region's `allowed` list*, so permission was transitive: soundd grants
//! its per-client audio ring to a client, and that client can grant it to
//! anyone it likes. Nothing anywhere reports it to the owner. The target pid
//! was also unchecked, so `allowed` would take pids that have never existed —
//! a userland-driven kernel `Vec` with no bound at all, since `Pid`s are
//! monotonic and never reused.
//!
//! Three roles. `owner <middleman-pid>` allocates a region, writes a secret,
//! grants it to the middleman only, and stays alive. The default role *is* the
//! middleman: legitimately granted, it then tries to pass the region on to
//! `attacker <token>`, which maps it and reports what it saw.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use toyos_abi::syscall::{self, SyscallError};
use toyos_abi::Pid;

const SELF_PATH: &str = "/bin/test_rs_abuse_shared_grant";
const SECRET: &[u8] = b"owner-private-bytes-do-not-share";
const REGION: usize = 4096;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("owner") => owner(args.next().expect("owner needs the middleman pid").parse().unwrap()),
        Some("attacker") => attacker(args.next().expect("attacker needs a token").parse().unwrap()),
        Some(other) => panic!("unknown role {other:?}"),
        None => middleman(),
    }
}

fn middleman() {
    let me = syscall::getpid();

    let mut owner = Command::new(SELF_PATH)
        .arg("owner")
        .arg(me.0.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn owner");
    let owner_pid = owner.id();
    let mut owner_out = BufReader::new(owner.stdout.take().expect("owner stdout"));
    let mut line = String::new();
    owner_out.read_line(&mut line).expect("owner token line");
    let token: u32 = line.trim().parse().expect("owner token");

    // The grant we were actually given still works, and this is what makes the
    // rest of the test non-vacuous: the region is real and holds the secret.
    let ptr = unsafe { syscall::map_shared(token) }.expect("the owner granted us; this map must work");
    let seen = unsafe { core::slice::from_raw_parts(ptr, SECRET.len()) };
    assert_eq!(seen, SECRET, "the owner's region did not contain the secret");

    // Pass it on to a process the owner never named.
    let mut attacker = Command::new(SELF_PATH)
        .arg("attacker")
        .arg(token.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn attacker");
    let attacker_pid = attacker.id();
    let regrant = syscall::grant_shared(token, Pid(attacker_pid));

    let mut attacker_in = attacker.stdin.take().expect("attacker stdin");
    writeln!(attacker_in, "go").expect("release the attacker");
    attacker_in.flush().expect("flush");
    let mut attacker_out = BufReader::new(attacker.stdout.take().expect("attacker stdout"));
    let mut said = String::new();
    attacker_out.read_line(&mut said).expect("attacker report");
    let said = said.trim().to_string();

    // Asserted after the fact so a kernel that allows the re-grant is caught
    // with the stolen bytes in hand rather than at the first refusal.
    assert_eq!(
        regrant,
        Err(SyscallError::PermissionDenied),
        "a grantee re-granted the owner's region to pid {attacker_pid}, which then reported {said:?}"
    );
    assert!(
        said.starts_with("denied"),
        "the attacker mapped a region its owner never granted it: {said:?}"
    );

    drop(attacker_in);
    assert!(attacker.wait().expect("wait attacker").success(), "attacker exited nonzero");

    // A region of our own, to test the target rather than the caller.
    let own = syscall::alloc_shared(REGION);

    // `Pid`s come from a monotonic `IdMap` and are never reused, so this one
    // has never named a process and never will before this test ends.
    let ghost = Pid(me.0 + 100_000);
    assert_eq!(
        syscall::grant_shared(own, ghost),
        Err(SyscallError::InvalidArgument),
        "granting to pid {} — a process that has never existed — must be refused",
        ghost.0
    );

    // …while the owner granting to a live process is exactly what daemons do,
    // and must keep working.
    syscall::grant_shared(own, Pid(owner_pid))
        .expect("an owner granting its own region to a live process must still work");

    syscall::release_shared(own);
    syscall::release_shared(token);

    let mut owner_in = owner.stdin.take().expect("owner stdin");
    writeln!(owner_in, "quit").expect("tell the owner to quit");
    drop(owner_in);
    assert!(owner.wait().expect("wait owner").success(), "owner exited nonzero");

    println!("re-grant refused, ghost target refused, owner grant and map still work");
}

fn owner(middleman_pid: u32) {
    let token = syscall::alloc_shared(REGION);
    let ptr = unsafe { syscall::map_shared(token) }.expect("owner: map its own region");
    unsafe { core::ptr::copy_nonoverlapping(SECRET.as_ptr(), ptr, SECRET.len()) };

    syscall::grant_shared(token, Pid(middleman_pid)).expect("owner: grant to the middleman");
    println!("{token}");
    std::io::stdout().flush().expect("owner: flush token");

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    syscall::release_shared(token);
}

fn attacker(token: u32) {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).expect("attacker: wait for go");
    match unsafe { syscall::map_shared(token) } {
        Ok(ptr) => {
            let seen = unsafe { core::slice::from_raw_parts(ptr, SECRET.len()) };
            println!("read {}", String::from_utf8_lossy(seen));
        }
        Err(e) => println!("denied {e:?}"),
    }
    std::io::stdout().flush().expect("attacker: flush report");
    // Drain stdin so the parent's close is what ends us.
    let mut rest = String::new();
    let _ = std::io::stdin().read_to_string(&mut rest);
}
