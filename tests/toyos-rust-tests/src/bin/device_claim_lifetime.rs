//! A device claim lives exactly as long as the one descriptor that names it.
//!
//! `SYS_OPEN_DEVICE` is the machine's only arbitration: whoever holds the
//! claim owns the keyboard, the scanout, the NIC ring or the audio buffer, and
//! everything gated on `device::is_owner` follows from it. `dup` used to hand
//! that back — the descriptor was cloned as a plain value and `close` released
//! the class unconditionally, so `open_device(d); dup(fd); close(fd)` freed
//! the device for anyone to take while leaving the caller a working
//! descriptor. On the framebuffer that is two processes composing to one
//! scanout; on the keyboard it is one process reading another's keystrokes.
//!
//! The mouse is the device under test because it is the one every machine
//! shape has: `try_claim` gates the other four on a driver having registered
//! something, so on a headless boot they answer `NotFound` and prove nothing.
//!
//! Roles: no argument is the test; `claimer` tries to take the mouse and
//! reports what it got; `holder` takes it, says so, and waits to be killed.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use toyos_abi::syscall::{self, DeviceType, MmapFlags, MmapProt, SpawnArgs, SyscallError};

const SELF_PATH: &str = "/bin/test_rs_device_claim_lifetime";

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("claimer") => claimer(),
        Some("holder") => holder(),
        Some(other) => panic!("unknown role {other:?}"),
        None => test(),
    }
}

fn test() {
    let mouse = syscall::open_device(DeviceType::Mouse).expect("the mouse must be unclaimed");

    // The exploit, and the only reason this file exists. If `dup` gives back a
    // second descriptor, the claim is no longer exclusive to anything.
    match syscall::dup(mouse) {
        Ok(stale) => {
            syscall::close(mouse);
            let thief = claim_in_child();
            panic!(
                "dup handed back the mouse claim: a second descriptor is live \
                 and another process's claim answered {thief:?} (stale fd {stale:?})"
            );
        }
        Err(SyscallError::PermissionDenied) => {}
        Err(e) => panic!("dup of a device fd: expected PermissionDenied, got {e:?}"),
    }

    // dup2 is the same operation with a caller-chosen slot, and must answer
    // the same. A `Ok` here would put the claim on fd 9 and leave `mouse`
    // closable.
    match syscall::dup2(mouse, toyos_abi::RawHandle(9)) {
        Err(SyscallError::PermissionDenied) => {}
        other => panic!("dup2 of a device fd: expected PermissionDenied, got {other:?}"),
    }

    // A spawn `fd_map` is the third way to get a second descriptor, and it is
    // the one that would put the claim in another process — where releasing it
    // is not even this process's to do.
    match spawn_with_fd_map(mouse) {
        Err(SyscallError::PermissionDenied) => {}
        other => panic!("spawn with a device fd in its fd_map: expected PermissionDenied, got {other:?}"),
    }

    // Three refusals must not have released anything.
    assert_eq!(
        claim_in_child(),
        Some(SyscallError::AlreadyExists),
        "the mouse was released by a refused duplication"
    );

    // The ordinary release still works, and so does an ordinary exit: the
    // child below claims and then exits without closing.
    syscall::close(mouse);
    assert_eq!(claim_in_child(), None, "close did not release the claim");
    let after_exit = syscall::open_device(DeviceType::Mouse)
        .expect("an exited process must give its device claim back");
    syscall::close(after_exit);

    // The path that matters most, and the one no `Drop` on a victim's stack
    // could ever bind: a process killed by another CPU never unwinds, so the
    // claim comes back only because teardown drains the descriptor table.
    let mut holder = Command::new(SELF_PATH)
        .arg("holder")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn holder");
    let mut out = BufReader::new(holder.stdout.take().expect("holder stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("holder ready line");
    assert_eq!(line.trim(), "held", "the holder did not claim the mouse: {line:?}");

    assert_eq!(
        claim_in_child(),
        Some(SyscallError::AlreadyExists),
        "the holder's claim is not exclusive"
    );

    // `Child::kill` is unimplemented in the ToyOS std, so this is the syscall.
    syscall::kill(toyos_abi::Pid(holder.id())).expect("kill the holder");
    holder.wait().expect("reap the holder");

    let reclaimed = syscall::open_device(DeviceType::Mouse)
        .expect("a killed process must give its device claim back");
    syscall::close(reclaimed);

    println!("device claim: dup, dup2 and fd_map refused; close and kill both release it");
}

/// `None` when the child got the claim, `Some(e)` when it was refused.
fn claim_in_child() -> Option<SyscallError> {
    let child = Command::new(SELF_PATH)
        .arg("claimer")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn claimer");
    let out = child.wait_with_output().expect("wait claimer");
    match String::from_utf8_lossy(&out.stdout).trim() {
        "claimed" => None,
        "AlreadyExists" => Some(SyscallError::AlreadyExists),
        "NotFound" => Some(SyscallError::NotFound),
        other => panic!("claimer said {other:?}"),
    }
}

/// `SYS_SPAWN` with `[[3, fd]]` as its fd_map.
///
/// One mmap region for both blobs: `user_bytes` needs the window to be
/// physically contiguous, and a stack buffer that straddled a page would make
/// this pass on `BadAddress` without ever reaching `build_child_fds`.
fn spawn_with_fd_map(fd: toyos_abi::RawHandle) -> Result<toyos_abi::Pid, SyscallError> {
    const REGION: usize = 4096;
    const FD_MAP_OFF: usize = 2048;

    let region = unsafe {
        syscall::mmap(
            core::ptr::null_mut(),
            REGION,
            MmapProt::READ | MmapProt::WRITE,
            MmapFlags::ANONYMOUS | MmapFlags::PRIVATE,
        )
    };
    assert!(!region.is_null(), "mmap failed");

    let argv = format!("{SELF_PATH}\0claimer\0");
    unsafe { core::ptr::copy_nonoverlapping(argv.as_ptr(), region, argv.len()) };

    let pair = [3u32.to_ne_bytes(), fd.0.to_ne_bytes()].concat();
    unsafe {
        core::ptr::copy_nonoverlapping(pair.as_ptr(), region.add(FD_MAP_OFF), pair.len())
    };

    let result = unsafe {
        syscall::spawn(&SpawnArgs {
            argv_ptr: region as u64,
            argv_len: argv.len() as u64,
            fd_map_ptr: region as u64 + FD_MAP_OFF as u64,
            fd_map_count: 1,
            env_ptr: 0,
            env_len: 0,
        })
    };
    unsafe { syscall::munmap(region, REGION) }.expect("munmap");
    result
}

fn claimer() {
    match syscall::open_device(DeviceType::Mouse) {
        Ok(_) => println!("claimed"),
        Err(e) => println!("{e:?}"),
    }
}

fn holder() {
    let _mouse = syscall::open_device(DeviceType::Mouse).expect("holder: claim the mouse");
    println!("held");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
