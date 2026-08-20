//! A `SYS_PIPE_MAP` window must not outlive the descriptors that justified it.
//!
//! Three syscalls: `SYS_PIPE`, `SYS_PIPE_MAP` on either end, then close both.
//! The last `PipeReader`/`PipeWriter` drop takes the refcount to zero,
//! `free_pipe` drops the `PhysPage`, and the PMM has the 2 MiB page back —
//! while the caller's mapping of it is still live and still writable. Whatever
//! the PMM hands that page to next, another process's pipe or a kernel heap
//! region or a DMA buffer, was then readable and writable by a process that
//! owned nothing.
//!
//! Run as `abuse_pipe_map child`, this binary is the attack: it maps, closes,
//! and writes. The child has to die for this test to pass — a child that
//! reaches its last line has just written into memory it does not own, and
//! says so.

use std::process::{Command, Stdio};

use toyos_abi::syscall;

/// Past the 64-byte `RingHeader` and into the ring's data, so nothing here
/// depends on where the header ends.
const OFFSET: usize = 4096;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("child") {
        return child();
    }

    let victim = Command::new("/bin/test_rs_abuse_pipe_map")
        .arg("child")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the child");
    let out = victim.wait_with_output().expect("wait for the child");
    let said = String::from_utf8_lossy(&out.stdout);

    // The premise. Without it a child that failed at `pipe_map` would satisfy
    // every assertion below by dying for the wrong reason.
    assert!(
        said.contains("mapped and wrote"),
        "the child never got a working mapping, so it proved nothing:\n{said}"
    );
    assert!(
        !said.contains("STILL WRITABLE"),
        "the child wrote through its pipe mapping after closing every descriptor \
         for the pipe — the page is back in the PMM and userland can still write it:\n{said}"
    );
    assert!(
        !out.status.success(),
        "the child survived writing to a revoked pipe mapping (exit={:?})",
        out.status.code()
    );
    println!("  PASS: a pipe mapping is revoked with the last descriptor, and the write faults");

    // The kernel took a fault in a page it had handed back. It must still be
    // running, and running well enough to spawn.
    let echo = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("run echo after the child faulted");
    assert!(echo.status.success());
    assert_eq!(String::from_utf8_lossy(&echo.stdout).trim(), "still alive");
    println!("  PASS: the kernel is still running afterwards");

    println!("all pipe mapping revocation tests passed");
}

fn child() {
    let p = syscall::pipe().expect("a pipe to map");
    let base = syscall::pipe_map(p.write).expect("pipe_map on a pipe we hold") as *mut u8;

    // One window per pipe, not one per call: a second map is what bounds the
    // kernel's record of them, so it has to be the same address.
    let again = syscall::pipe_map(p.write).expect("a second pipe_map") as *mut u8;
    assert_eq!(base, again, "a second pipe_map handed out a second window");

    unsafe { base.add(OFFSET).write_volatile(0x5A) };
    println!("mapped and wrote at {:p}", unsafe { base.add(OFFSET) });

    syscall::close(p.read);
    syscall::close(p.write);

    unsafe { base.add(OFFSET).write_volatile(0xA5) };
    println!("STILL WRITABLE");
}
