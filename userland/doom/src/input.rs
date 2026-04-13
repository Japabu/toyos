use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{KeyCode, PhysicalKey};

const KEY_QUEUE_SIZE: usize = 64;
static mut KEY_QUEUE: [(i32, u8); KEY_QUEUE_SIZE] = [(0, 0); KEY_QUEUE_SIZE];
static mut KEY_QUEUE_READ: usize = 0;
static mut KEY_QUEUE_WRITE: usize = 0;

// Track which doom_key was sent for each KeyCode so releases match presses.
static mut KEYCODE_TO_DOOM: [u8; 256] = [0; 256];

fn enqueue_key(pressed: bool, doom_key: u8) {
    unsafe {
        let next = (KEY_QUEUE_WRITE + 1) % KEY_QUEUE_SIZE;
        if next != KEY_QUEUE_READ {
            KEY_QUEUE[KEY_QUEUE_WRITE] = (pressed as i32, doom_key);
            KEY_QUEUE_WRITE = next;
        }
    }
}

pub fn dequeue_key(pressed: &mut i32, doom_key: &mut u8) -> bool {
    unsafe {
        if KEY_QUEUE_READ == KEY_QUEUE_WRITE {
            return false;
        }
        let (p, k) = KEY_QUEUE[KEY_QUEUE_READ];
        *pressed = p;
        *doom_key = k;
        KEY_QUEUE_READ = (KEY_QUEUE_READ + 1) % KEY_QUEUE_SIZE;
        true
    }
}

// ── DOOM key constants (from doomkeys.h) ──

const KEY_RIGHTARROW: u8 = 0xae;
const KEY_LEFTARROW: u8 = 0xac;
const KEY_UPARROW: u8 = 0xad;
const KEY_DOWNARROW: u8 = 0xaf;
const KEY_ESCAPE: u8 = 27;
const KEY_ENTER: u8 = 13;
const KEY_TAB: u8 = 9;
const KEY_BACKSPACE: u8 = 0x7f;
const KEY_FIRE: u8 = 0xa3;
const KEY_USE: u8 = 0xa2;
const KEY_RSHIFT: u8 = 0x80 + 0x36;
const KEY_F1: u8 = 0x80 + 0x3b;

fn keycode_to_doom(code: KeyCode) -> Option<u8> {
    match code {
        KeyCode::Enter => Some(KEY_ENTER),
        KeyCode::Escape => Some(KEY_ESCAPE),
        KeyCode::Backspace => Some(KEY_BACKSPACE),
        KeyCode::Tab => Some(KEY_TAB),
        KeyCode::Space => Some(KEY_USE),
        KeyCode::ArrowRight => Some(KEY_RIGHTARROW),
        KeyCode::ArrowLeft => Some(KEY_LEFTARROW),
        KeyCode::ArrowDown => Some(KEY_DOWNARROW),
        KeyCode::ArrowUp => Some(KEY_UPARROW),
        KeyCode::F1 => Some(KEY_F1),
        KeyCode::F2 => Some(KEY_F1 + 1),
        KeyCode::F3 => Some(KEY_F1 + 2),
        KeyCode::F4 => Some(KEY_F1 + 3),
        KeyCode::F5 => Some(KEY_F1 + 4),
        KeyCode::F6 => Some(KEY_F1 + 5),
        KeyCode::F7 => Some(KEY_F1 + 6),
        KeyCode::F8 => Some(KEY_F1 + 7),
        KeyCode::F9 => Some(KEY_F1 + 8),
        KeyCode::F10 => Some(KEY_F1 + 9),
        KeyCode::F11 => Some(KEY_F1 + 10),
        KeyCode::F12 => Some(KEY_F1 + 11),
        KeyCode::ControlLeft | KeyCode::ControlRight => Some(KEY_FIRE),
        KeyCode::ShiftLeft | KeyCode::ShiftRight => Some(KEY_RSHIFT),
        KeyCode::AltLeft | KeyCode::AltRight => Some(KEY_USE),
        KeyCode::KeyA => Some(b'a'),
        KeyCode::KeyB => Some(b'b'),
        KeyCode::KeyC => Some(b'c'),
        KeyCode::KeyD => Some(b'd'),
        KeyCode::KeyE => Some(b'e'),
        KeyCode::KeyF => Some(b'f'),
        KeyCode::KeyG => Some(b'g'),
        KeyCode::KeyH => Some(b'h'),
        KeyCode::KeyI => Some(b'i'),
        KeyCode::KeyJ => Some(b'j'),
        KeyCode::KeyK => Some(b'k'),
        KeyCode::KeyL => Some(b'l'),
        KeyCode::KeyM => Some(b'm'),
        KeyCode::KeyN => Some(b'n'),
        KeyCode::KeyO => Some(b'o'),
        KeyCode::KeyP => Some(b'p'),
        KeyCode::KeyQ => Some(b'q'),
        KeyCode::KeyR => Some(b'r'),
        KeyCode::KeyS => Some(b's'),
        KeyCode::KeyT => Some(b't'),
        KeyCode::KeyU => Some(b'u'),
        KeyCode::KeyV => Some(b'v'),
        KeyCode::KeyW => Some(b'w'),
        KeyCode::KeyX => Some(b'x'),
        KeyCode::KeyY => Some(b'y'),
        KeyCode::KeyZ => Some(b'z'),
        KeyCode::Digit0 => Some(b'0'),
        KeyCode::Digit1 => Some(b'1'),
        KeyCode::Digit2 => Some(b'2'),
        KeyCode::Digit3 => Some(b'3'),
        KeyCode::Digit4 => Some(b'4'),
        KeyCode::Digit5 => Some(b'5'),
        KeyCode::Digit6 => Some(b'6'),
        KeyCode::Digit7 => Some(b'7'),
        KeyCode::Digit8 => Some(b'8'),
        KeyCode::Digit9 => Some(b'9'),
        KeyCode::Minus => Some(b'-'),
        KeyCode::Equal => Some(b'='),
        KeyCode::Comma => Some(b','),
        KeyCode::Period => Some(b'.'),
        _ => None,
    }
}

pub fn handle_winit_key(event: &KeyEvent) {
    let PhysicalKey::Code(code) = event.physical_key else { return };
    let code_idx = code as usize;
    if code_idx >= 256 {
        return;
    }

    unsafe {
        if event.state == ElementState::Pressed {
            if let Some(doom_key) = keycode_to_doom(code) {
                KEYCODE_TO_DOOM[code_idx] = doom_key;
                enqueue_key(true, doom_key);
            }
        } else {
            let doom_key = KEYCODE_TO_DOOM[code_idx];
            if doom_key != 0 {
                KEYCODE_TO_DOOM[code_idx] = 0;
                enqueue_key(false, doom_key);
            }
        }
    }
}
