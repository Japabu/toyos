//! What a process holds is what its parent gave it, and there is no second
//! place to ask.
//!
//! **The test estate is the one place least authority is not enforced** — a test
//! binary holds what `test-runner` holds, because the guest binaries are not
//! `[programs]` keys and no manifest row can name what any of them needs.
//! This binary is where least
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
//! **Capabilities.** Four things are reachable no other way — minting a device
//! claim, entering the real-time band, turning a pid into a process handle, and
//! powering the machine off
//! — and each is one bit on a handle to a `SysCap` the kernel mints exactly once,
//! for `/bin/init`. A handle that carries the wrong *bit* is refused with a word,
//! because probing what an attenuated capability can still do is what
//! attenuation is for; a handle that is **no handle at all** ends the caller.
//!
//! **The shutdown arms are the ones with a machine behind them.** `SYS_SHUTDOWN`
//! used to take no argument, so both of them made the call and the guest went
//! away: a kernel that stops demanding `Rights::POWER` does not fail these
//! assertions, it powers off the boot they run on, which is the loudest red
//! this suite can produce and exactly the defect being denied.
//!
//! **A wrong-typed handle is refused with a word here, and that is a property of
//! the check rather than an exception to the policy.** The table resolves rights
//! before type, and `DEVICE`, `RT` and `POWER` are bits only a `SysCap` ever
//! carries — so
//! nothing of another type can reach the type check at all, and presenting one is
//! indistinguishable from presenting an attenuated capability. Asserted, because
//! it is the answer a caller gets and a test that expected the kill would be
//! asserting something the design cannot do.

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

/// Presenting no handle at all, each raised in a child of its own because the
/// kernel's answer to it is to end the caller.
const NOT_A_HANDLE: &[(&str, &str)] = &[
    ("claim-absent", "SYS_DEVICE_CLAIM took a handle nobody holds"),
    ("rt-absent", "SYS_RT_ENTER took a handle nobody holds"),
    // The third, and the only one whose failure mode is not a wrong exit code:
    // a kernel that took this handle would cut the power to the guest the
    // parent is waiting on.
    ("shutdown-absent", "SYS_SHUTDOWN took a handle nobody holds"),
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("probe") => probe(),
        Some(role) => not_a_handle(role),
        None => test(),
    }
}

fn test() {
    only_what_was_given();
    a_right_the_capability_lacks_is_a_word();
    for (role, what_would_be_wrong) in NOT_A_HANDLE {
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

    // **Narrowed and not the estate's own cap, because this estate does carry
    // `POWER`.** `run shutdown` is how a dozen host-side gates end their guest,
    // so `tests/testcases` names `power` on the test-runner row and every
    // binary it spawns holds a duplicate — including this one. The subject is a
    // capability that resolves and lacks the bit, which is what `toothless` is.
    //
    // There is no arm for the unnarrowed cap here, and there cannot be: the
    // call that proves the estate *does* hold `POWER` does not come back, and
    // `run shutdown` at the end of a dozen host-side gates is that proof.
    assert_eq!(
        toothless.shutdown(),
        SyscallError::PermissionDenied,
        "a capability without POWER shut the machine down",
    );

    // A handle that is not a capability at all. It never reaches the type
    // check: `DEVICE` and `RT` are bits nothing else carries, so this is the
    // same refusal the narrowed capability got.
    assert_eq!(
        syscall::device_claim(RawHandle(1), DeviceType::Keyboard).err(),
        Some(SyscallError::PermissionDenied),
        "a pipe was taken as a capability by SYS_DEVICE_CLAIM",
    );
    assert_eq!(
        syscall::rt_enter(RawHandle(1)),
        Err(SyscallError::PermissionDenied),
        "a pipe was taken as a capability by SYS_RT_ENTER",
    );
    assert_eq!(
        syscall::shutdown(RawHandle(1)),
        SyscallError::PermissionDenied,
        "a pipe was taken as a capability by SYS_SHUTDOWN",
    );

    // And the unnarrowed one does carry `DEVICE`, so the refusals above were
    // the bit and not the call. What it answers beyond that is a fact about
    // the machine — `NotFound` for a class this boot has no driver for — and
    // the assertion says only that it is not the refusal. A claim is released
    // as soon as it is taken, because a claim moves and the boot runs other
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
    println!("  capability: refused for the bit it lacks and for having none, allowed for the bit it has");
    println!("  power: a capability without POWER, and a pipe, were both refused the machine");
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

fn not_a_handle(role: &str) -> ! {
    assert!(NOT_A_HANDLE.iter().any(|(name, _)| *name == role), "unknown role {role:?}");
    println!("reached {role}");
    std::io::stdout().flush().expect("flush the marker");
    let answered = match role {
        "claim-absent" => {
            format!("{:?}", syscall::device_claim(HANDLE_INVALID, DeviceType::Keyboard))
        }
        "rt-absent" => format!("{:?}", syscall::rt_enter(HANDLE_INVALID)),
        // If this comes back at all the kernel refused it, which is already the
        // wrong answer for a handle nobody holds. If it does not come back, the
        // machine this child is running on has been powered off and the parent
        // never reads a word of this.
        _ => format!("{:?}", syscall::shutdown(HANDLE_INVALID)),
    };
    panic!("{role} was answered {answered} instead of ending the caller");
}
