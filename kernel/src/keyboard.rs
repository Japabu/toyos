use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

// Scan code set 1 -> ASCII (unshifted)
static SCANCODE_MAP: [u8; 128] = {
    let mut map = [0u8; 128];
    map[0x02] = b'1'; map[0x03] = b'2'; map[0x04] = b'3'; map[0x05] = b'4';
    map[0x06] = b'5'; map[0x07] = b'6'; map[0x08] = b'7'; map[0x09] = b'8';
    map[0x0A] = b'9'; map[0x0B] = b'0'; map[0x0C] = b'-'; map[0x0D] = b'=';
    map[0x0E] = 0x08; // Backspace
    map[0x0F] = b'\t';
    map[0x10] = b'q'; map[0x11] = b'w'; map[0x12] = b'e'; map[0x13] = b'r';
    map[0x14] = b't'; map[0x15] = b'y'; map[0x16] = b'u'; map[0x17] = b'i';
    map[0x18] = b'o'; map[0x19] = b'p'; map[0x1A] = b'['; map[0x1B] = b']';
    map[0x1C] = b'\n'; // Enter
    map[0x1E] = b'a'; map[0x1F] = b's'; map[0x20] = b'd'; map[0x21] = b'f';
    map[0x22] = b'g'; map[0x23] = b'h'; map[0x24] = b'j'; map[0x25] = b'k';
    map[0x26] = b'l'; map[0x27] = b';'; map[0x28] = b'\''; map[0x29] = b'`';
    map[0x2B] = b'\\';
    map[0x2C] = b'z'; map[0x2D] = b'x'; map[0x2E] = b'c'; map[0x2F] = b'v';
    map[0x30] = b'b'; map[0x31] = b'n'; map[0x32] = b'm'; map[0x33] = b',';
    map[0x34] = b'.'; map[0x35] = b'/';
    map[0x39] = b' ';
    map
};

// Scan code set 1 -> ASCII (shifted)
static SCANCODE_MAP_SHIFT: [u8; 128] = {
    let mut map = [0u8; 128];
    map[0x02] = b'!'; map[0x03] = b'@'; map[0x04] = b'#'; map[0x05] = b'$';
    map[0x06] = b'%'; map[0x07] = b'^'; map[0x08] = b'&'; map[0x09] = b'*';
    map[0x0A] = b'('; map[0x0B] = b')'; map[0x0C] = b'_'; map[0x0D] = b'+';
    map[0x0E] = 0x08; map[0x0F] = b'\t';
    map[0x10] = b'Q'; map[0x11] = b'W'; map[0x12] = b'E'; map[0x13] = b'R';
    map[0x14] = b'T'; map[0x15] = b'Y'; map[0x16] = b'U'; map[0x17] = b'I';
    map[0x18] = b'O'; map[0x19] = b'P'; map[0x1A] = b'{'; map[0x1B] = b'}';
    map[0x1C] = b'\n';
    map[0x1E] = b'A'; map[0x1F] = b'S'; map[0x20] = b'D'; map[0x21] = b'F';
    map[0x22] = b'G'; map[0x23] = b'H'; map[0x24] = b'J'; map[0x25] = b'K';
    map[0x26] = b'L'; map[0x27] = b':'; map[0x28] = b'"'; map[0x29] = b'~';
    map[0x2B] = b'|';
    map[0x2C] = b'Z'; map[0x2D] = b'X'; map[0x2E] = b'C'; map[0x2F] = b'V';
    map[0x30] = b'B'; map[0x31] = b'N'; map[0x32] = b'M'; map[0x33] = b'<';
    map[0x34] = b'>'; map[0x35] = b'?';
    map[0x39] = b' ';
    map
};

// Ring buffer for translated key characters
const BUF_SIZE: usize = 64;

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

static SHIFT_HELD: AtomicBool = AtomicBool::new(false);

/// Called from the IRQ1 handler in interrupts.rs.
pub fn handle_scancode(scancode: u8) {
    // Track shift key state
    match scancode {
        0x2A | 0x36 => { SHIFT_HELD.store(true, Ordering::Relaxed); return; }
        0xAA | 0xB6 => { SHIFT_HELD.store(false, Ordering::Relaxed); return; }
        _ => {}
    }

    // Ignore break codes (key release, bit 7 set)
    if scancode & 0x80 != 0 {
        return;
    }

    let ascii = if SHIFT_HELD.load(Ordering::Relaxed) {
        SCANCODE_MAP_SHIFT[scancode as usize]
    } else {
        SCANCODE_MAP[scancode as usize]
    };

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
