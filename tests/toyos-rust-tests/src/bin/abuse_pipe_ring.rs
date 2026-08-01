//! The kernel must not read a pipe ring's bounds back out of the page the
//! owning process maps and writes.
//!
//! `SYS_PIPE_MAP` hands the caller its own pipe's 2 MiB ring page, writable,
//! with the ring header at offset 0. While the kernel read its bounds from
//! that header, the bytes in it were a divisor, a kernel pointer offset and a
//! copy length: a zeroed header plus one `write` divided by zero inside the
//! `PIPES` lock, and a large capacity with a matching cursor turned the
//! caller's own buffer into a kernel write at an offset the caller chose.
//!
//! `kernel/src/io_uring.rs` states the rule this gates — the kernel must not
//! read its own bounds back out of a page the process can write.

use std::mem::size_of;

use toyos_abi::ring::RingHeader;
use toyos_abi::syscall;

/// The u32 slots the kernel used to trust, in the order it read them:
/// `write_cursor`, `read_cursor`, `capacity`. Naming them reproduces the
/// original attack; the remaining bytes of the header are covered too, so the
/// case still means something if a later layout moves a field.
fn header_image(write_cursor: u32, read_cursor: u32, capacity: u32) -> Vec<u8> {
    let mut img = vec![0u8; size_of::<RingHeader>()];
    img[0..4].copy_from_slice(&write_cursor.to_le_bytes());
    img[4..8].copy_from_slice(&read_cursor.to_le_bytes());
    img[8..12].copy_from_slice(&capacity.to_le_bytes());
    img
}

/// A stream whose every byte is determined by its absolute position, so a copy
/// landing at the wrong ring offset comes back as a shifted sequence rather
/// than as plausible data.
fn fill(buf: &mut [u8], start: u64) {
    let mut x = (start % 251) as u8;
    for b in buf.iter_mut() {
        *b = x;
        x = if x == 250 { 0 } else { x + 1 };
    }
}

fn main() {
    let p = syscall::pipe();
    let base = syscall::pipe_map(p.write).expect("pipe_map") as *mut u8;
    let header_len = size_of::<RingHeader>();

    // Without this, every assertion below would also pass on a page the
    // process cannot reach at all.
    unsafe { base.write_volatile(0x5A) };
    assert_eq!(
        unsafe { base.read_volatile() },
        0x5A,
        "the SYS_PIPE_MAP page is not writable from here — the test is vacuous"
    );

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("capacity zero", header_image(0, 0, 0)),
        ("capacity 4 GiB with the cursor 1 GiB into it", header_image(0x4000_0000, 0, 0xFFFF_FFFF)),
        ("cursors further apart than the capacity", header_image(0, 0x7FFF_FFFF, 0x10)),
        ("every bit set", vec![0xFFu8; header_len]),
        ("every bit clear", vec![0x00u8; header_len]),
    ];

    let payload = b"ring header abuse";
    for (what, image) in &cases {
        unsafe { std::ptr::copy_nonoverlapping(image.as_ptr(), base, header_len) };

        let n = syscall::write_nonblock(p.write, payload).unwrap_or_else(|e| {
            panic!(
                "{what}: write refused with {e:?} — the ring's bounds are the kernel's, so \
                 what a process puts in the header must not change what a write does"
            )
        });
        assert_eq!(n, payload.len(), "{what}: short write");

        let mut got = [0u8; 64];
        let n = syscall::read_nonblock(p.read, &mut got)
            .unwrap_or_else(|e| panic!("{what}: read refused with {e:?}"));
        assert_eq!(&got[..n], payload, "{what}: the stream came back changed");
    }

    // Past one full lap of the ring, header still scribbled. The wrap is where
    // the corrupted capacity was both the modulus and the split point of the
    // two copies; the pipe ring is one 2 MiB page.
    unsafe { std::ptr::write_bytes(base, 0xFF, header_len) };
    const TARGET: u64 = 3 * 1024 * 1024;
    let mut wbuf = vec![0u8; 61 * 1024];
    let mut rbuf = vec![0u8; 37 * 1024];
    let mut expect = vec![0u8; 37 * 1024];
    let mut sent: u64 = 0;
    let mut recv: u64 = 0;
    while recv < TARGET {
        while sent < TARGET {
            let n = wbuf.len().min((TARGET - sent) as usize);
            fill(&mut wbuf[..n], sent);
            match syscall::write_nonblock(p.write, &wbuf[..n]) {
                Ok(k) => {
                    sent += k as u64;
                    if k < n {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let k = syscall::read_nonblock(p.read, &mut rbuf).expect("read during the lap");
        assert!(k > 0, "ring stalled at {recv} of {TARGET} (written {sent})");
        fill(&mut expect[..k], recv);
        if rbuf[..k] != expect[..k] {
            let off = (0..k).position(|i| rbuf[i] != expect[i]).expect("mismatch position");
            panic!(
                "stream byte {} came back {:#04x}, want {:#04x} — this read spans {}..{}",
                recv + off as u64,
                rbuf[off],
                expect[off],
                recv,
                recv + k as u64
            );
        }
        recv += k as u64;
    }

    println!("pipe ring header abuse ignored, {recv} bytes exact across the wrap");
}
