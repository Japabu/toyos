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

    pub fn available(&self) -> u32 {
        let w = self.write_cursor.load(Ordering::Acquire);
        let r = self.read_cursor.load(Ordering::Acquire);
        w.wrapping_sub(r)
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

        self.read_cursor.store((r + count) as u32, Ordering::Release);
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

        self.write_cursor.store((w + count) as u32, Ordering::Release);
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
