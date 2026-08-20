//! Memory protection, which the kernel used to discard in both directions.
//!
//! `sys_mmap`'s third argument was named `_prot`. Every mapping came back
//! readable and writable whatever the caller asked for, so
//! `userland/libc`'s translation of POSIX `PROT_NONE` produced a writable
//! guard page and the stack-overflow detection a C program builds on it
//! silently did not exist.
//!
//! **And every mapping came back executable**, because `EFER.NXE` was set
//! nowhere and no paging entry in the machine carried the `XD` bit — while
//! `.text` came back *writable* wherever it shared a 2 MiB window with `.data`,
//! which is 14 of the 20 binaries in the boot set. So this file also holds the
//! two halves of W^X: what may be written may not be executed, and what may be
//! executed may not be written.
//!
//! Each refusal is checked in a child, because the whole point is that the
//! access kills the process — and the parent then asks whether the machine is
//! still there.

use std::process::{Command, Stdio};

use toyos_abi::syscall::{mmap, MmapFlags, MmapProt};

const SIZE: usize = 4096;
/// Well inside the 2 MiB page every mapping is rounded up to.
const OFFSET: usize = 64;

/// `ret`. The whole of a function that returns immediately, so a jump to a page
/// that *is* executable comes back and one to a page that is not dies on the
/// instruction fetch rather than on anything it would have gone on to do.
const RET: u8 = 0xC3;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("write-none") => return touch(MmapProt::NONE, Access::Write),
        Some("read-none") => return touch(MmapProt::NONE, Access::Read),
        Some("write-ro") => return touch(MmapProt::READ, Access::Write),
        Some("exec-heap") => return exec_heap(),
        Some("exec-stack") => return exec_stack(),
        Some("write-text") => return write_text(),
        _ => {}
    }

    readwrite_is_readable_and_writable();
    readonly_is_readable();
    none_is_a_mapping_and_not_an_error();
    text_is_readable_and_executable();

    dies("write-none", "a store to a PROT_NONE mapping");
    dies("read-none", "a load from a PROT_NONE mapping");
    dies("write-ro", "a store to a PROT_READ mapping");
    dies("exec-heap", "a jump into an anonymous mapping");
    dies("exec-stack", "a jump into the stack");
    dies("write-text", "a store into .text");

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

/// The positive control for `write-text`: making `.text` unwritable must not
/// have made it unreadable, and splitting the window it lives in must not have
/// made it unrunnable.
///
/// A kernel that mapped `.text` `PROT_NONE` would satisfy the write refusal and
/// fail here — and one that lost the executable bit in the split would not have
/// got this program as far as saying so, which is the other half of the
/// control.
fn text_is_readable_and_executable() {
    assert_eq!(returns_a_marker(), MARKER, "a call into .text did not return");
    let first = unsafe { (returns_a_marker as *const u8).read_volatile() };
    println!("  PASS: .text is readable ({first:#04x}) and executable");
}

const MARKER: u32 = 0x5A5A_A5A5;

/// Called through `.text` in one mode and written to through `.text` in
/// another. `#[inline(never)]` so there is a body at that address in both
/// roles, and `#[unsafe(no_mangle)]` so nothing folds it into an identical
/// function somewhere else.
#[inline(never)]
#[unsafe(no_mangle)]
extern "C" fn returns_a_marker() -> u32 {
    std::hint::black_box(MARKER)
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
        said.contains("armed"),
        "the {mode} child never reached the access, so it proved nothing:\n{said}"
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
        .expect("run echo after six protection faults");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "still alive");
    println!("  PASS: the kernel is still running after six protection faults");
}

enum Access {
    Read,
    Write,
}

fn touch(prot: MmapProt, access: Access) {
    let p = map(prot);
    println!("armed at {p:p}");
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

/// Call an address the caller has just written [`RET`] to.
///
/// # Safety
/// Only ever pointed at such a byte. Where the kernel is correct the fetch
/// faults before the byte matters; where it is not, `ret` is the one
/// instruction that returns to a Rust caller unharmed and lets the child print
/// `SURVIVED` instead of dying of something else.
unsafe fn call(code: *const u8) {
    let f: extern "C" fn() = unsafe { core::mem::transmute(code) };
    f();
}

/// The heap: what a `PROT_READ | PROT_WRITE` mapping is, and where every
/// allocation a program makes lives.
fn exec_heap() {
    let p = map(MmapProt::READ | MmapProt::WRITE);
    unsafe { p.add(OFFSET).write_volatile(RET) };
    println!("armed at {:p}", unsafe { p.add(OFFSET) });
    unsafe { call(p.add(OFFSET)) };
    println!("SURVIVED, ran a byte out of an anonymous mapping");
}

/// The stack: eight megabytes at a fixed address, and the page a stack-smashing
/// payload is written onto.
fn exec_stack() {
    let mut code = [0u8; 16];
    // Volatile, so the array is a real object on the stack rather than a
    // constant the compiler folds into `.rodata` — a different page with a
    // different protection, which would prove something else.
    unsafe { code.as_mut_ptr().write_volatile(RET) };
    let p: *const u8 = code.as_ptr();
    println!("armed at {p:p}");
    unsafe { call(p) };
    println!("SURVIVED, ran a byte off the stack");
}

/// `.text`: the half of W^X 2 MiB pages could not give, because in 14 of the
/// boot set's 20 binaries every byte of text shares a 2 MiB window with
/// `.data`.
fn write_text() {
    let p = returns_a_marker as *const u8 as *mut u8;
    println!("armed at {p:p}");
    // A `nop` and not a random byte: if the kernel permits the store, the
    // function is still callable and the child reports `SURVIVED` rather than
    // dying of the corruption it just caused, which would look like a pass.
    unsafe { p.write_volatile(0x90) };
    println!("SURVIVED, wrote {:#04x} into .text", unsafe { p.read_volatile() });
}
