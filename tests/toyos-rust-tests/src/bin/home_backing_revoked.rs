//! A file backing must not outlive the file it reads.
//!
//! `/home` hands every open file an `NvmeBacking` holding the blocks its data
//! lives in. Unlink the file and those blocks go back to bcachefs's allocator;
//! the next file takes them. A backing that still names them reads that file's
//! contents — an information disclosure through `open`, `rm` and `cp`, with
//! nothing crafted about it and no privilege needed.
//!
//! Staged here rather than reasoned about: the victim's blocks are freed and
//! then deliberately handed to a file whose bytes are nothing like the
//! victim's, and the still-open descriptor is read afterwards.

use std::fs;
use std::io::{Read, Write};

const VICTIM: &str = "/home/revoke_victim.bin";
const ATTACKER: &str = "/home/revoke_attacker.bin";
const CONTROL: &str = "/home/revoke_control.bin";

/// Eight pages. More than one so the read crosses pages, and small enough that
/// the harness's `/home` has room for two of them at once.
const LEN: usize = 8 * 4096;

const VICTIM_BYTE: u8 = 0xA7;
const ATTACKER_BYTE: u8 = 0x5C;

fn write_file(path: &str, byte: u8) {
    let mut f = fs::File::create(path).unwrap_or_else(|e| panic!("create {path}: {e}"));
    f.write_all(&vec![byte; LEN]).unwrap_or_else(|e| panic!("write {path}: {e}"));
    f.sync_all().unwrap_or_else(|e| panic!("fsync {path}: {e}"));
}

fn read_all(f: &mut fs::File) -> Vec<u8> {
    let mut got = Vec::new();
    f.read_to_end(&mut got).expect("read the open descriptor");
    got
}

fn main() {
    // The control. Closing the file drops its cached pages, so this open is
    // served by the backing and not by anything left over from the write — if
    // it were not, the attack below would prove nothing about that path.
    write_file(CONTROL, VICTIM_BYTE);
    let control = read_all(&mut fs::File::open(CONTROL).expect("open the control"));
    assert_eq!(control.len(), LEN, "the control read short");
    assert!(
        control.iter().all(|&b| b == VICTIM_BYTE),
        "the backing did not serve the control file's own bytes",
    );

    write_file(VICTIM, VICTIM_BYTE);

    // Held open, and deliberately not read: every page is absent from the file
    // cache, so each one is a fault the backing has to answer.
    let mut held = fs::File::open(VICTIM).expect("open the victim");

    fs::remove_file(VICTIM).expect("unlink the victim");

    // The victim's blocks are the lowest free ones now, so this takes them.
    write_file(ATTACKER, ATTACKER_BYTE);

    let got = read_all(&mut held);
    assert_eq!(got.len(), LEN, "the read through the deleted file came up short");
    if let Some(at) = got.iter().position(|&b| b == ATTACKER_BYTE) {
        panic!(
            "byte {at} read through the deleted file's descriptor is {ATTACKER_BYTE:#04x} — \
             the backing served another file's data",
        );
    }

    // Zeros, not the victim's own bytes: the backing was *revoked*, so the
    // read failed and the file cache had nothing to put in the buffer. Asked
    // this way the test does not depend on the attacker actually landing on
    // the victim's blocks — a backing that still resolves reads either
    // {ATTACKER_BYTE:#04x} or {VICTIM_BYTE:#04x}, and neither is zero.
    if let Some(at) = got.iter().position(|&b| b != 0) {
        panic!(
            "byte {at} read through the deleted file's descriptor is {:#04x}, not zero — \
             the backing still resolves blocks the allocator has taken back",
            got[at],
        );
    }

    let _ = fs::remove_file(ATTACKER);
    let _ = fs::remove_file(CONTROL);
    println!("a backing whose file was deleted served none of the next file's {LEN} bytes");
}
