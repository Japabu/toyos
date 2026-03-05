use std::ptr::addr_of_mut;
use std::time::Instant;
use window::{Event, KeyEvent, Window};

// doomgeneric C interface
extern "C" {
    fn doomgeneric_Create(argc: i32, argv: *const *const u8);
    fn doomgeneric_Tick();
    static mut DG_ScreenBuffer: *mut u32;
}

static mut WINDOW: Option<Window> = None;
static mut START_TIME: Option<Instant> = None;

// Key event ring buffer
const KEY_QUEUE_SIZE: usize = 64;
static mut KEY_QUEUE: [(i32, u8); KEY_QUEUE_SIZE] = [(0, 0); KEY_QUEUE_SIZE];
static mut KEY_QUEUE_READ: usize = 0;
static mut KEY_QUEUE_WRITE: usize = 0;

// Track which doom_key was sent for each HID keycode so releases match presses.
// Key releases have no translated character data, so we replay the press mapping.
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

fn poll_input() {
    unsafe {
        let window = (*addr_of_mut!(WINDOW)).as_mut().unwrap();
        loop {
            match window.poll_event(1) {
                Some(Event::KeyInput(ev)) => handle_key(ev),
                Some(Event::Close) => std::process::exit(0),
                Some(Event::Frame) => {}
                Some(_) => {}
                None => break,
            }
        }
    }
}

fn handle_key(ev: KeyEvent) {
    unsafe {
        if ev.pressed() {
            if let Some(doom_key) = to_doom_key(&ev) {
                KEYCODE_TO_DOOM[ev.keycode as usize] = doom_key;
                enqueue_key(true, doom_key);
            }
        } else {
            let doom_key = KEYCODE_TO_DOOM[ev.keycode as usize];
            if doom_key != 0 {
                KEYCODE_TO_DOOM[ev.keycode as usize] = 0;
                enqueue_key(false, doom_key);
            }
        }
    }
}

// DOOM key constants (from doomkeys.h)
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

fn to_doom_key(ev: &KeyEvent) -> Option<u8> {
    // Special keys by HID keycode (not affected by layout)
    match ev.keycode {
        0x28 => return Some(KEY_ENTER),
        0x29 => return Some(KEY_ESCAPE),
        0x2A => return Some(KEY_BACKSPACE),
        0x2B => return Some(KEY_TAB),
        0x2C => return Some(b' '),
        // Arrow keys
        0x4F => return Some(KEY_RIGHTARROW),
        0x50 => return Some(KEY_LEFTARROW),
        0x51 => return Some(KEY_DOWNARROW),
        0x52 => return Some(KEY_UPARROW),
        // F keys (HID 0x3A-0x45 -> F1-F12)
        0x3A..=0x45 => return Some(KEY_F1 + (ev.keycode - 0x3A)),
        // Modifier keys: map to DOOM action keys (like other doomgeneric backends)
        0xE0 | 0xE4 => return Some(KEY_FIRE),   // Ctrl = fire
        0xE1 | 0xE5 => return Some(KEY_RSHIFT),  // Shift = run
        0xE2 | 0xE6 => return Some(KEY_USE),     // Alt = use/open
        _ => {}
    }
    // Use the keyboard-layout-translated character for printable keys.
    // DOOM expects lowercase ASCII for letter keys.
    if ev.len > 0 {
        let ch = ev.translated[0];
        match ch {
            b'A'..=b'Z' => return Some(ch - b'A' + b'a'),
            b'a'..=b'z' | b'0'..=b'9' | b' '..=b'/' | b':'..=b'@' | b'['..=b'`' | b'{'..=b'~' => {
                return Some(ch);
            }
            _ => {}
        }
    }
    None
}

// ── DG_* implementations ──

const SRC_W: usize = 640;
const SRC_H: usize = 400;

#[no_mangle]
pub extern "C" fn DG_Init() {
    let window = Window::create_with_title(960, 600, "DOOM");
    unsafe {
        WINDOW = Some(window);
        START_TIME = Some(Instant::now());
    }
}

#[no_mangle]
pub extern "C" fn DG_DrawFrame() {
    poll_input();

    unsafe {
        let window = (*addr_of_mut!(WINDOW)).as_mut().unwrap();
        let fb = window.framebuffer();
        let src = DG_ScreenBuffer;
        if src.is_null() {
            return;
        }

        let dst_w = fb.width();
        let dst_h = fb.height();
        let dst_stride = fb.stride();
        let dst_ptr = fb.ptr();

        // Nearest-neighbor scale DG_ScreenBuffer (640x400 XRGB8888) to framebuffer.
        // DOOM's XRGB8888 (0x00RRGGBB) matches the BGR framebuffer layout in
        // little-endian, so we can copy u32s directly without channel swizzling.
        let dst_pixels = dst_ptr as *mut u32;

        // Precompute X mapping table to avoid per-pixel division.
        let mut x_map = [0usize; 2560]; // max width we support
        let map = &mut x_map[..dst_w];
        for dx in 0..dst_w {
            map[dx] = dx * SRC_W / dst_w;
        }

        let mut prev_sy = usize::MAX;
        let mut prev_dst_row: *mut u32 = core::ptr::null_mut();
        for dy in 0..dst_h {
            let sy = dy * SRC_H / dst_h;
            let dst_row = dst_pixels.add(dy * dst_stride);

            if sy == prev_sy && !prev_dst_row.is_null() {
                // Same source row as previous — memcpy the already-scaled row.
                core::ptr::copy_nonoverlapping(prev_dst_row, dst_row, dst_w);
            } else {
                let src_row = src.add(sy * SRC_W);
                for dx in 0..dst_w {
                    *dst_row.add(dx) = *src_row.add(*map.get_unchecked(dx));
                }
            }
            prev_sy = sy;
            prev_dst_row = dst_row;
        }

        window.present();
    }
}

#[no_mangle]
pub extern "C" fn DG_SleepMs(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}

#[no_mangle]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    unsafe { (*addr_of_mut!(START_TIME)).as_ref().unwrap().elapsed().as_millis() as u32 }
}

#[no_mangle]
pub extern "C" fn DG_GetKey(pressed: *mut i32, doom_key: *mut u8) -> i32 {
    unsafe {
        if KEY_QUEUE_READ == KEY_QUEUE_WRITE {
            return 0;
        }
        let (p, k) = KEY_QUEUE[KEY_QUEUE_READ];
        *pressed = p;
        *doom_key = k;
        KEY_QUEUE_READ = (KEY_QUEUE_READ + 1) % KEY_QUEUE_SIZE;
        1
    }
}

#[no_mangle]
pub extern "C" fn DG_SetWindowTitle(_title: *const u8) {
    // No-op: title is set at window creation
}

fn main() {
    // Force toyos-libc symbols to be linked in
    toyos_libc::_libc_ctype_init();
    toyos_libc::_libc_math_init();
    toyos_libc::_libc_memory_init();
    toyos_libc::_libc_printf_init();
    toyos_libc::_libc_stdio_init();
    toyos_libc::_libc_string_init();

    // Build argv with default WAD path
    let args: Vec<&[u8]> = vec![
        b"doom\0",
        b"-iwad\0",
        b"/initrd/doom1.wad\0",
    ];
    let argv: Vec<*const u8> = args.iter().map(|a| a.as_ptr()).collect();

    unsafe {
        doomgeneric_Create(argv.len() as i32, argv.as_ptr());

        loop {
            doomgeneric_Tick();
        }
    }
}
