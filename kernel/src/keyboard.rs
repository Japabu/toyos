use core::cell::UnsafeCell;

// Ring buffer for translated key characters
const BUF_SIZE: usize = 256;

struct KeyBuffer {
    buf: UnsafeCell<[u8; BUF_SIZE]>,
    head: UnsafeCell<usize>,
    tail: UnsafeCell<usize>,
}

unsafe impl Sync for KeyBuffer {}

static KEY_BUF: KeyBuffer = KeyBuffer {
    buf: UnsafeCell::new([0; BUF_SIZE]),
    head: UnsafeCell::new(0),
    tail: UnsafeCell::new(0),
};

/// Called from the USB HID driver with a pre-translated ASCII byte.
pub fn handle_key(ascii: u8) {
    if ascii == 0 {
        return;
    }
    unsafe {
        let buf = &mut *KEY_BUF.buf.get();
        let head = &mut *KEY_BUF.head.get();
        let tail = *KEY_BUF.tail.get();
        let next = (*head + 1) % BUF_SIZE;
        if next != tail {
            buf[*head] = ascii;
            *head = next;
        }
    }
}

/// Non-blocking read of the next character from the keyboard buffer.
pub fn try_read_char() -> Option<u8> {
    unsafe {
        let head = *KEY_BUF.head.get();
        let tail = &mut *KEY_BUF.tail.get();
        if *tail == head {
            return None;
        }
        let ch = (*KEY_BUF.buf.get())[*tail];
        *tail = (*tail + 1) % BUF_SIZE;
        Some(ch)
    }
}
