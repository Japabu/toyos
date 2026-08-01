//! Lock-free SPSC ring buffer for shared-memory pipes.
//!
//! Layout: a `RingHeader` followed by `capacity` bytes of data.
//! One producer, one consumer. No locks needed — only atomic cursors.

use core::sync::atomic::{AtomicU32, Ordering};

pub const RING_WRITER_CLOSED: u32 = 1;
pub const RING_READER_CLOSED: u32 = 2;

#[repr(C, align(64))]
pub struct RingHeader {
    pub write_cursor: AtomicU32,
    pub read_cursor: AtomicU32,
    pub capacity: u32,
    pub flags: AtomicU32,
}

impl RingHeader {
    /// Initialize a ring header for a region of `total_size` bytes.
    /// Data starts immediately after the header.
    pub fn init(ptr: *mut u8, total_size: usize) {
        let capacity = total_size - core::mem::size_of::<Self>();
        assert!(
            capacity > 0 && capacity < (1usize << 31),
            "ring capacity {capacity} does not leave room for the cursor modulus"
        );
        let header = ptr as *mut Self;
        unsafe {
            (*header).write_cursor = AtomicU32::new(0);
            (*header).read_cursor = AtomicU32::new(0);
            (*header).capacity = capacity as u32;
            (*header).flags = AtomicU32::new(0);
        }
    }

    fn data_ptr(&self) -> *mut u8 {
        let base = self as *const Self as *mut u8;
        unsafe { base.add(core::mem::size_of::<Self>()) }
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
        let w = self.write_cursor.load(Ordering::Acquire) as u64;
        let r = self.read_cursor.load(Ordering::Acquire) as u64;
        let m = self.modulus();
        ((w + m - r) % m) as u32
    }

    pub fn space(&self) -> u32 {
        self.capacity - self.available()
    }

    /// Read up to `buf.len()` bytes. Returns number of bytes read.
    pub fn read(&self, buf: &mut [u8]) -> usize {
        let avail = self.available() as usize;
        if avail == 0 {
            return 0;
        }
        let count = buf.len().min(avail);
        let cap = self.capacity as usize;
        let r = self.read_cursor.load(Ordering::Relaxed) as usize;
        let offset = r % cap;
        let data = self.data_ptr();

        // May need two copies if wrapping around the buffer end
        let first = count.min(cap - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(data.add(offset), buf.as_mut_ptr(), first);
            if first < count {
                core::ptr::copy_nonoverlapping(data, buf.as_mut_ptr().add(first), count - first);
            }
        }

        self.read_cursor.store(self.advance(r as u32, count), Ordering::Release);
        count
    }

    /// Write up to `buf.len()` bytes. Returns number of bytes written.
    pub fn write(&self, buf: &[u8]) -> usize {
        let free = self.space() as usize;
        if free == 0 {
            return 0;
        }
        let count = buf.len().min(free);
        let cap = self.capacity as usize;
        let w = self.write_cursor.load(Ordering::Relaxed) as usize;
        let offset = w % cap;
        let data = self.data_ptr();

        let first = count.min(cap - offset);
        unsafe {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), data.add(offset), first);
            if first < count {
                core::ptr::copy_nonoverlapping(buf.as_ptr().add(first), data, count - first);
            }
        }

        self.write_cursor.store(self.advance(w as u32, count), Ordering::Release);
        count
    }

    pub fn is_writer_closed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & RING_WRITER_CLOSED != 0
    }

    pub fn is_reader_closed(&self) -> bool {
        self.flags.load(Ordering::Acquire) & RING_READER_CLOSED != 0
    }

    pub fn close_writer(&self) {
        self.flags.fetch_or(RING_WRITER_CLOSED, Ordering::Release);
    }

    pub fn close_reader(&self) {
        self.flags.fetch_or(RING_READER_CLOSED, Ordering::Release);
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
            RingHeader::init(ptr, total);
            Self { ptr, layout }
        }

        fn header(&self) -> &RingHeader {
            unsafe { &*(self.ptr as *const RingHeader) }
        }
    }

    impl Drop for Backing {
        fn drop(&mut self) {
            unsafe { dealloc(self.ptr, self.layout) }
        }
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
        let ring = Backing::new(PIPE_TOTAL);
        let h = ring.header();

        let seed = (h.modulus() - 16) as u32;
        h.write_cursor.store(seed, Ordering::Relaxed);
        h.read_cursor.store(seed, Ordering::Relaxed);

        let sent: [u8; 32] = core::array::from_fn(|i| stream_byte(i as u64));
        assert_eq!(h.write(&sent), 32);

        let mut got = [0u8; 32];
        assert_eq!(h.read(&mut got[..16]), 16);
        assert_eq!(
            h.read_cursor.load(Ordering::Relaxed),
            0,
            "the seed did not put the wrap between the two halves"
        );
        assert_eq!(h.read(&mut got[16..]), 16);
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
        let ring = Backing::new(TOTAL);
        let h = ring.header();
        let cap = h.capacity as u64;
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
                let put = h.write(&wbuf[..n]);
                written += put as u64;
                if put < n {
                    break;
                }
            }
            let got = h.read(&mut rbuf);
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
                    h.modulus(),
                );
            }
            read += got as u64;
        }
        assert_eq!(read, target);
    }
}
