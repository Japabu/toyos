//! Lock-free SPSC ring buffer for shared-memory pipes.
//!
//! Layout: a `RingHeader` followed by `capacity` bytes of data.
//!
//! The region is one page the kernel allocates and `SYS_PIPE_MAP` maps into a
//! process **writable**, so the split here is a trust boundary, not a
//! convenience: `Ring` is the kernel-side owner and holds every value the
//! copies are bounded by, in kernel memory; `RingHeader` holds only what
//! userland reads. Nothing in the header is read back by the kernel — same
//! rule `kernel/src/io_uring.rs` states for its own tail.

use core::sync::atomic::{AtomicU32, Ordering};

pub const RING_WRITER_CLOSED: u32 = 1;
pub const RING_READER_CLOSED: u32 = 2;

/// What a process sees at offset 0 of a mapped ring.
///
/// Every field is writable by any process holding the pipe, so nothing the
/// kernel indexes, bounds or divides by may live here. `flags` is a
/// publication netd polls to notice a peer that went away; the kernel only
/// ever stores to it, and answers "is the other end gone?" from its own
/// refcounts.
#[repr(C, align(64))]
pub struct RingHeader {
    pub flags: AtomicU32,
}

impl RingHeader {
    pub fn is_writer_closed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & RING_WRITER_CLOSED != 0
    }

    pub fn is_reader_closed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & RING_READER_CLOSED != 0
    }
}

/// Kernel-side owner of one ring region: the cursors, the capacity, and the
/// base of the region they address.
///
/// Every access is serialized by the caller's lock (`kernel::pipe`'s `PIPES`),
/// which is why the cursors are plain integers rather than atomics — the
/// header's were only ever atomic because they lived in shared memory.
pub struct Ring {
    base: *mut u8,
    capacity: u32,
    write_cursor: u32,
    read_cursor: u32,
}

impl Ring {
    /// Claim `total_size` bytes at `base` as a ring, initializing the header
    /// the mapping process will see.
    ///
    /// # Safety
    /// `base` must be writable and aligned for `RingHeader`, and the region
    /// must stay valid for the life of the `Ring`.
    pub unsafe fn new(base: *mut u8, total_size: usize) -> Self {
        let capacity = total_size - core::mem::size_of::<RingHeader>();
        assert!(
            capacity > 0 && capacity < (1usize << 31),
            "ring capacity {capacity} does not leave room for the cursor modulus"
        );
        unsafe {
            (base as *mut RingHeader).write(RingHeader {
                flags: AtomicU32::new(0),
            })
        };
        Self {
            base,
            capacity: capacity as u32,
            write_cursor: 0,
            read_cursor: 0,
        }
    }

    fn header(&self) -> &RingHeader {
        unsafe { &*(self.base as *const RingHeader) }
    }

    fn data_ptr(&self) -> *mut u8 {
        unsafe { self.base.add(core::mem::size_of::<RingHeader>()) }
    }

    /// The cursors count modulo this, and a stream byte's ring offset is its
    /// cursor value modulo `capacity`.
    ///
    /// Those two are consistent only if `capacity` divides the modulus, which
    /// is why it is `2 * capacity` and not the `u32`'s own `2^32`: `capacity`
    /// is whatever a 2 MiB page leaves after a 64-byte header, and it does not
    /// divide `2^32`. Where the modulus is not a multiple of `capacity`, the
    /// two cursor values naming the same stream byte on either side of the
    /// wrap map to *different* offsets, and an access straddling the wrap
    /// lands on the wrong bytes.
    ///
    /// Twice, rather than once, so that full stays distinguishable from empty:
    /// `available` ranges over `0..=capacity` and both ends are representable.
    fn modulus(&self) -> u64 {
        self.capacity as u64 * 2
    }

    fn advance(&self, cursor: u32, by: usize) -> u32 {
        ((cursor as u64 + by as u64) % self.modulus()) as u32
    }

    pub fn available(&self) -> u32 {
        let w = self.write_cursor as u64;
        let r = self.read_cursor as u64;
        let m = self.modulus();
        ((w + m - r) % m) as u32
    }

    pub fn space(&self) -> u32 {
        self.capacity - self.available()
    }

    /// Read up to `want` bytes, handing `sink` each contiguous run of them with
    /// its offset in the destination. Returns the number of bytes read.
    ///
    /// The destination is described rather than passed because the kernel's is
    /// user memory, and a `&mut [u8]` over a page userland can write is the
    /// aliasing claim `kernel::user_ptr` exists to stop making. One or two runs,
    /// never more: the second is the ring's own wrap.
    pub fn read(&mut self, want: usize, mut sink: impl FnMut(usize, &[u8])) -> usize {
        let avail = self.available() as usize;
        if avail == 0 {
            return 0;
        }
        let count = want.min(avail);
        let cap = self.capacity as usize;
        let offset = self.read_cursor as usize % cap;
        let data = self.data_ptr();

        let first = count.min(cap - offset);
        unsafe {
            sink(0, core::slice::from_raw_parts(data.add(offset), first));
            if first < count {
                sink(first, core::slice::from_raw_parts(data, count - first));
            }
        }

        self.read_cursor = self.advance(self.read_cursor, count);
        count
    }

    /// Write up to `want` bytes, asking `fill` for each contiguous run of them
    /// by its offset in the source. Returns the number of bytes written.
    ///
    /// The mirror of [`Self::read`], and the same reason.
    pub fn write(&mut self, want: usize, mut fill: impl FnMut(usize, &mut [u8])) -> usize {
        let free = self.space() as usize;
        if free == 0 {
            return 0;
        }
        let count = want.min(free);
        let cap = self.capacity as usize;
        let offset = self.write_cursor as usize % cap;
        let data = self.data_ptr();

        let first = count.min(cap - offset);
        unsafe {
            fill(0, core::slice::from_raw_parts_mut(data.add(offset), first));
            if first < count {
                fill(first, core::slice::from_raw_parts_mut(data, count - first));
            }
        }

        self.write_cursor = self.advance(self.write_cursor, count);
        count
    }

    pub fn open_writer(&self) {
        self.header().flags.fetch_and(!RING_WRITER_CLOSED, Ordering::Release);
    }

    pub fn open_reader(&self) {
        self.header().flags.fetch_and(!RING_READER_CLOSED, Ordering::Release);
    }

    pub fn close_writer(&self) {
        self.header().flags.fetch_or(RING_WRITER_CLOSED, Ordering::Release);
    }

    pub fn close_reader(&self) {
        self.header().flags.fetch_or(RING_READER_CLOSED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{alloc_zeroed, dealloc, Layout};

    /// The ring `kernel::pipe` builds: one 2 MiB page, header at offset 0.
    const PIPE_TOTAL: usize = 2 * 1024 * 1024;

    struct Backing {
        ptr: *mut u8,
        layout: Layout,
    }

    impl Backing {
        fn new(total: usize) -> Self {
            let layout = Layout::from_size_align(total, core::mem::align_of::<RingHeader>()).unwrap();
            let ptr = unsafe { alloc_zeroed(layout) };
            assert!(!ptr.is_null(), "test backing allocation failed");
            Self { ptr, layout }
        }

        fn ring(&self) -> Ring {
            unsafe { Ring::new(self.ptr, self.layout.size()) }
        }
    }

    impl Drop for Backing {
        fn drop(&mut self) {
            unsafe { dealloc(self.ptr, self.layout) }
        }
    }

    /// The slice forms the kernel cannot have: here the destination is an
    /// ordinary allocation, so describing it buys nothing.
    fn read_slice(ring: &mut Ring, buf: &mut [u8]) -> usize {
        let want = buf.len();
        ring.read(want, |off, src| buf[off..off + src.len()].copy_from_slice(src))
    }

    fn write_slice(ring: &mut Ring, buf: &[u8]) -> usize {
        ring.write(buf.len(), |off, dst| dst.copy_from_slice(&buf[off..off + dst.len()]))
    }

    /// Byte at absolute stream position `pos`. Every aligned 16-byte group
    /// carries its own index, so a misread shows up whether it is off by a
    /// multiple of 16 (wrong stamp) or not (wrong filler).
    fn stream_byte(pos: u64) -> u8 {
        let group = (pos >> 4) as u32;
        match pos & 15 {
            k @ 0..=3 => (group >> (8 * k)) as u8,
            _ => 0xC3,
        }
    }

    /// `stream_byte` over a range, touching only the four stamped bytes of
    /// each group: the test moves gibibytes and runs unoptimised, so a
    /// per-byte closure is most of its runtime.
    fn fill(buf: &mut [u8], start: u64) {
        buf.fill(0xC3);
        let end = start + buf.len() as u64;
        let mut pos = start;
        while pos < end {
            let k = pos & 15;
            if k < 4 {
                buf[(pos - start) as usize] = stream_byte(pos);
                pos += 1;
            } else {
                pos += 16 - k;
            }
        }
    }

    /// The premise, computed rather than asserted from memory: `capacity` is
    /// whatever the header's alignment leaves of a 2 MiB page, and it does not
    /// divide the `u32` cursor space.
    #[test]
    fn the_pipe_capacity_does_not_divide_the_u32_cursor_space() {
        let header = core::mem::size_of::<RingHeader>();
        let capacity = (PIPE_TOTAL - header) as u64;
        let remainder = (1u64 << 32) % capacity;
        assert_ne!(
            remainder, 0,
            "header {header} B, capacity {capacity} B: a cursor wrapping at 2^32 \
             would be sound only if capacity divided it"
        );
        assert_eq!(
            (header, capacity, remainder),
            (64, 2_097_088, 131_072),
            "the pipe ring's shape moved — re-derive the wrap argument before \
             changing this"
        );
    }

    /// The fast gate: a read split across the ring's own wrap point must
    /// return what was written. Seeded, because reaching the wrap honestly
    /// costs the gibibytes the next test spends.
    #[test]
    fn a_read_split_across_the_cursor_wrap_returns_what_was_written() {
        let backing = Backing::new(PIPE_TOTAL);
        let mut ring = backing.ring();

        let seed = (ring.modulus() - 16) as u32;
        ring.write_cursor = seed;
        ring.read_cursor = seed;

        let sent: [u8; 32] = core::array::from_fn(|i| stream_byte(i as u64));
        assert_eq!(write_slice(&mut ring, &sent), 32);

        let mut got = [0u8; 32];
        assert_eq!(read_slice(&mut ring, &mut got[..16]), 16);
        assert_eq!(
            ring.read_cursor, 0,
            "the seed did not put the wrap between the two halves"
        );
        assert_eq!(read_slice(&mut ring, &mut got[16..]), 16);
        assert_eq!(
            got, sent,
            "a read straddling the cursor wrap did not return what was written"
        );
    }

    /// That the seeded state above is reachable at all: a long-lived pipe
    /// carries a cursor there by ordinary traffic. Slow on purpose — this is
    /// the proof the fast gate is guarding a state a real pipe reaches, and
    /// with the modulus at 2*capacity it also crosses ~33k wraps on the way.
    #[test]
    fn a_stream_survives_the_cursor_reaching_two_to_the_thirty_two() {
        const TOTAL: usize = 64 * 1024;
        let backing = Backing::new(TOTAL);
        let mut ring = backing.ring();
        let cap = ring.capacity as u64;
        let target = (1u64 << 32) + 4 * cap;

        // Coprime-ish with the capacity, so accesses straddle the buffer end
        // and the cursor wrap rather than lining up on either.
        let mut wbuf = std::vec![0u8; 4093];
        let mut rbuf = std::vec![0u8; 3571];
        let mut expect = std::vec![0u8; 3571];

        let mut written = 0u64;
        let mut read = 0u64;
        while read < target {
            while written < target {
                let n = wbuf.len().min((target - written) as usize);
                fill(&mut wbuf[..n], written);
                let put = write_slice(&mut ring, &wbuf[..n]);
                written += put as u64;
                if put < n {
                    break;
                }
            }
            let got = read_slice(&mut ring, &mut rbuf);
            assert!(got > 0, "ring stalled at {read} of {target}");
            fill(&mut expect[..got], read);
            if rbuf[..got] != expect[..got] {
                let off = (0..got).position(|i| rbuf[i] != expect[i]).unwrap();
                panic!(
                    "stream byte {} came back {:#04x}, want {:#04x} — this read \
                     spans {}..{}, and the cursor modulus is {}",
                    read + off as u64,
                    rbuf[off],
                    expect[off],
                    read,
                    read + got as u64,
                    ring.modulus(),
                );
            }
            read += got as u64;
        }
        assert_eq!(read, target);
    }

    /// The header is in the page `SYS_PIPE_MAP` maps writable. Whatever a
    /// process puts there, the stream must be unaffected — nothing the copies
    /// are bounded by is in that page.
    #[test]
    fn a_scribbled_header_leaves_the_stream_exact() {
        const TOTAL: usize = 64 * 1024;
        let backing = Backing::new(TOTAL);
        let mut ring = backing.ring();
        let mut pos = 0u64;
        // The third value is where `capacity` used to sit; zero was the
        // divisor, and 0xFF.. the 4 GiB pointer offset.
        for pattern in [0x00u8, 0xFF, 0xA5] {
            unsafe { core::ptr::write_bytes(backing.ptr, pattern, core::mem::size_of::<RingHeader>()) };
            let mut sent = std::vec![0u8; 3571];
            fill(&mut sent, pos);
            assert_eq!(write_slice(&mut ring, &sent), sent.len());
            let mut got = std::vec![0u8; sent.len()];
            assert_eq!(read_slice(&mut ring, &mut got), sent.len());
            assert_eq!(got, sent, "header pattern {pattern:#04x} changed the stream");
            pos += sent.len() as u64;
        }
    }
}
