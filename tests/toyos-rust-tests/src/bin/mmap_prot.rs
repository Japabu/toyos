//! `mmap` protection, which the kernel used to discard.
//!
//! `sys_mmap`'s third argument was named `_prot`. Every mapping came back
//! readable and writable whatever the caller asked for, so
//! `userland/libc`'s translation of POSIX `PROT_NONE` produced a writable
//! guard page and the stack-overflow detection a C program builds on it
//! silently did not exist.
//!
//! Each refusal is checked in a child, because the whole point is that the
//! access kills the process — and the parent then asks whether the machine is
//! still there.

use std::process::{Command, Stdio};

use toyos_abi::syscall::{mmap, MmapFlags, MmapProt};

const SIZE: usize = 4096;
/// Well inside the 2 MiB page every mapping is rounded up to.
const OFFSET: usize = 64;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("write-none") => return touch(MmapProt::NONE, Access::Write),
        Some("read-none") => return touch(MmapProt::NONE, Access::Read),
        Some("write-ro") => return touch(MmapProt::READ, Access::Write),
        _ => {}
    }

    readwrite_is_readable_and_writable();
    readonly_is_readable();
    none_is_a_mapping_and_not_an_error();

    dies("write-none", "a store to a PROT_NONE mapping");
    dies("read-none", "a load from a PROT_NONE mapping");
    dies("write-ro", "a store to a PROT_READ mapping");

    still_alive();
    println!("all mmap protection tests passed");
}

fn map(prot: MmapProt) -> *mut u8 {
    let p = unsafe {
        mmap(core::ptr::null_mut(), SIZE, prot, MmapFlags::ANONYMOUS | MmapFlags::PRIVATE)
    };
    assert!(
        !p.is_null() && (p as u64) < u64::MAX - 255,
        "mmap(prot={:#x}) refused: {p:p}",
        prot.0
    );
    p
}

/// The positive control for every refusal below: the ordinary mapping every
/// allocator in the system asks for still works.
fn readwrite_is_readable_and_writable() {
    let p = map(MmapProt::READ | MmapProt::WRITE);
    unsafe {
        p.add(OFFSET).write_volatile(0x5A);
        assert_eq!(p.add(OFFSET).read_volatile(), 0x5A, "a RW mapping lost a byte");
    }
    println!("  PASS: PROT_READ|PROT_WRITE reads back what it wrote");
}

/// Read-only means readable, not merely unwritable. A kernel that refused the
/// mapping outright would satisfy the write test and fail this one.
fn readonly_is_readable() {
    let p = map(MmapProt::READ);
    assert_eq!(unsafe { p.add(OFFSET).read_volatile() }, 0, "a fresh mapping is not zeroed");
    println!("  PASS: PROT_READ is readable");
}

/// `PROT_NONE` is a request for address space that faults, not a bad argument.
/// The range has to be reserved — two of them in a row must not overlap.
fn none_is_a_mapping_and_not_an_error() {
    let a = map(MmapProt::NONE);
    let b = map(MmapProt::NONE);
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    assert!(
        (hi as usize) - (lo as usize) >= SIZE,
        "two PROT_NONE mappings overlap: {a:p} and {b:p}"
    );
    println!("  PASS: PROT_NONE reserves address space and returns it");
}

fn dies(mode: &str, what: &str) {
    let child = Command::new("/bin/test_rs_mmap_prot")
        .arg(mode)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {mode}: {e}"));
    let out = child.wait_with_output().unwrap_or_else(|e| panic!("wait for {mode}: {e}"));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("mapped"),
        "the {mode} child never got its mapping, so it proved nothing:\n{said}"
    );
    assert!(
        !said.contains("SURVIVED"),
        "{what} was permitted:\n{said}"
    );
    assert!(!out.status.success(), "{what} did not kill the process (exit={:?})", out.status.code());
    println!("  PASS: {what} kills the process");
}

/// The other half of every refusal: the kernel is unharmed by a fault it
/// delivered.
fn still_alive() {
    let out = Command::new("/bin/echo")
        .arg("still alive")
        .output()
        .expect("run echo after three protection faults");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "still alive");
    println!("  PASS: the kernel is still running after three protection faults");
}

enum Access {
    Read,
    Write,
}

fn touch(prot: MmapProt, access: Access) {
    let p = map(prot);
    println!("mapped at {p:p}");
    match access {
        Access::Read => {
            let v = unsafe { p.add(OFFSET).read_volatile() };
            println!("SURVIVED, read {v:#x}");
        }
        Access::Write => {
            unsafe { p.add(OFFSET).write_volatile(0xA5) };
            println!("SURVIVED, wrote");
        }
    }
}
