use crate::sync::SyncCell;

// Ring buffer for translated key characters
const BUF_SIZE: usize = 256;

struct KeyBuffer {
    buf: [u8; BUF_SIZE],
    head: usize,
    tail: usize,
}

static KEY_BUF: SyncCell<KeyBuffer> = SyncCell::new(KeyBuffer {
    buf: [0; BUF_SIZE],
    head: 0,
    tail: 0,
});

/// Called from the USB HID driver with a pre-translated ASCII byte.
pub fn handle_key(ascii: u8) {
    if ascii == 0 {
        return;
    }
    let kb = KEY_BUF.get_mut();
    let next = (kb.head + 1) % BUF_SIZE;
    if next != kb.tail {
        kb.buf[kb.head] = ascii;
        kb.head = next;
    }
}

/// Non-blocking read of the next character from the keyboard buffer.
pub fn try_read_char() -> Option<u8> {
    let kb = KEY_BUF.get_mut();
    if kb.tail == kb.head {
        return None;
    }
    let ch = kb.buf[kb.tail];
    kb.tail = (kb.tail + 1) % BUF_SIZE;
    Some(ch)
}
