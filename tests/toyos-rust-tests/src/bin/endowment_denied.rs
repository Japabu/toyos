//! What a process holds is what its parent gave it, and there is no second
//! place to ask.
//!
//! **The test estate is the one place least authority is not enforced** — a test
//! binary holds what `test-runner` holds, because the 90 guest binaries are not
//! `[programs]` keys and no manifest row can name what any of them needs
//! (`specs/capability-endowment-spec.md` §6.7a). This binary is where least
//! authority *is* asserted, and it works because it builds its own namespaces
//! and spawns itself: nothing here depends on what test-runner handed over.
//!
//! Two halves.
//!
//! **Names.** One child is endowed a namespace carrying `echo` alone and another
//! the same namespace carrying `echo` and `privileged`. The first must resolve
//! `echo` and answer `NotFound` for `privileged`; the second must resolve both.
//! The second arm is the whole of what stops the first passing because the
//! service was never there.
//!
//! **Capabilities.** Three things are reachable no other way — minting a device
//! claim, entering the real-time band, and turning a pid into a process handle
//! — and each is one bit on a handle to a `SysCap` the kernel mints exactly once,
//! for `/bin/init`. A handle that carries the wrong *bit* is refused with a word,
//! because probing what an attenuated capability can still do is what
//! attenuation is for; a handle that is not a capability at all, or is no handle
//! at all, ends the caller, because naming one of those is a bug rather than a
//! question. Both are asserted, and the split is the design.

use std::io::{BufRead, BufReader, Write};
use std::os::toyos::process::CommandExt;
use std::process::{Command, Stdio};

use toyos::endow::{Endowments, SYSCAP_LABEL};
use toyos::syscap::SysCap;
use toyos::{namespace, port, AsHandle};
use toyos_abi::handle::{Rights, HANDLE_INVALID};
use toyos_abi::syscall::{self, DeviceType, SyscallError, SVC_LABEL};
use toyos_abi::RawHandle;

const SELF_PATH: &str = "/bin/test_rs_endowment_denied";
const OPEN: &str = "echo";
const PRIVILEGED: &str = "privileged";

/// `process::HANDLE_FAULT_EXIT_CODE`.
const HANDLE_FAULT: i32 = 139;

/// The kinds of handle that are not a capability, each raised in a child of its
/// own because the kernel's answer to them is to end the caller.
const NOT_A_CAPABILITY: &[(&str, &str)] = &[
    ("claim-absent", "SYS_DEVICE_CLAIM took a handle nobody holds"),
    ("claim-mistyped", "SYS_DEVICE_CLAIM took a pipe"),
    ("rt-absent", "SYS_RT_ENTER took a handle nobody holds"),
    ("rt-mistyped", "SYS_RT_ENTER took a pipe"),
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("probe") => probe(),
        Some(role) => not_a_capability(role),
        None => test(),
    }
}

fn test() {
    only_what_was_given();
    a_right_the_capability_lacks_is_a_word();
    for (role, what_would_be_wrong) in NOT_A_CAPABILITY {
        killed(role, what_would_be_wrong);
    }
    println!("a name outside the namespace resolves to nothing, and a capability is its bits");
}

/// The two namespaces, and the one difference between them.
fn only_what_was_given() {
    let (_open_acceptor, open) = port::create().expect("a port for the open service");
    let (_priv_acceptor, privileged) = port::create().expect("a port for the privileged service");

    let narrow = namespace::build().add(OPEN, &open).finish().expect("the narrow namespace");
    assert_eq!(
        probe_with(narrow.into_raw()),
        format!("{OPEN}=ok {PRIVILEGED}=NotFound"),
        "a child endowed one name reached the other",
    );

    let wide = namespace::build()
        .add(OPEN, &open)
        .add(PRIVILEGED, &privileged)
        .finish()
        .expect("the wide namespace");
    assert_eq!(
        probe_with(wide.into_raw()),
        format!("{OPEN}=ok {PRIVILEGED}=ok"),
        "a child endowed both names could not reach one of them",
    );
    println!("  names: the narrow child reached one, the wide child reached both");
}

fn probe_with(ns: RawHandle) -> String {
    let mut child = Command::new(SELF_PATH)
        .arg("probe")
        .endow(SVC_LABEL, ns.0)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the probe");
    let mut out = BufReader::new(child.stdout.take().expect("probe stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("the probe's report");
    assert!(child.wait().expect("wait the probe").success(), "the probe exited nonzero");
    line.trim().to_string()
}

/// A capability handle that resolves and does not carry the bit.
///
/// The test estate's own cap is `DEVICE | DUP`, so the RT arm needs no
/// narrowing at all — this binary genuinely cannot enter the real-time band,
/// which is the privilege a device claim was never enough to confer.
fn a_right_the_capability_lacks_is_a_word() {
    let cap: SysCap = Endowments::get()
        .take(SYSCAP_LABEL)
        .expect("test-runner endows every binary it spawns a system capability");

    assert_eq!(
        syscall::rt_enter(cap.as_handle()),
        Err(SyscallError::PermissionDenied),
        "a capability without RT entered the real-time band",
    );

    let toothless = cap.narrowed(Rights::DUP).expect("a capability carrying less");
    assert_eq!(
        syscall::device_claim(toothless.as_handle(), DeviceType::Keyboard).err(),
        Some(SyscallError::PermissionDenied),
        "a capability without DEVICE minted a claim",
    );

    // And the unnarrowed one does carry `DEVICE`, so the refusal above was the
    // bit and not the call. What it answers beyond that is a fact about the
    // machine — `NotFound` for a class this boot has no driver for — and the
    // assertion says only that it is not the refusal. A claim is released as
    // soon as it is taken, because a claim moves and the boot runs other
    // binaries that need this one.
    let with_the_bit = syscall::device_claim(cap.as_handle(), DeviceType::Keyboard);
    assert_ne!(
        with_the_bit.err(),
        Some(SyscallError::PermissionDenied),
        "the estate's capability does not carry DEVICE either, so the refusal above proved nothing",
    );
    if let Ok(claim) = with_the_bit {
        syscall::close(claim);
    }
    println!("  capability: refused for the bit it lacks, allowed for the bit it has");
}

/// Run `role` and require that the kernel ended it at its call.
///
/// The marker is what gives the arm teeth: a child that died before reaching
/// the call would otherwise pass while asserting nothing.
fn killed(role: &str, what_would_be_wrong: &str) {
    let child = Command::new(SELF_PATH)
        .arg(role)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {role}: {e}"));
    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait {role}: {e}"));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("reached {role}"),
        "{role} never reached its call",
    );
    assert_eq!(out.status.code(), Some(HANDLE_FAULT), "{what_would_be_wrong}");
    println!("  {role}: ended the caller");
}

fn probe() {
    let answer = |name: &str| match toyos::endow::namespace().map(|ns| ns.open(name)) {
        Some(Ok(_)) => "ok".to_string(),
        Some(Err(e)) => format!("{e:?}"),
        None => "no namespace".to_string(),
    };
    println!("{OPEN}={} {PRIVILEGED}={}", answer(OPEN), answer(PRIVILEGED));
    std::io::stdout().flush().expect("probe: flush");
}

fn not_a_capability(role: &str) -> ! {
    assert!(
        NOT_A_CAPABILITY.iter().any(|(name, _)| *name == role),
        "unknown role {role:?}",
    );
    println!("reached {role}");
    std::io::stdout().flush().expect("flush the marker");
    // stdout: a handle this process certainly holds, and certainly not a
    // capability.
    let handle = if role.ends_with("mistyped") { RawHandle(1) } else { HANDLE_INVALID };
    let answered = if role.starts_with("claim") {
        format!("{:?}", syscall::device_claim(handle, DeviceType::Keyboard))
    } else {
        format!("{:?}", syscall::rt_enter(handle))
    };
    panic!("{role} was answered {answered} instead of ending the caller");
}
