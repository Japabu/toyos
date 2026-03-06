use core::ffi::c_void;
use std::ptr::addr_of_mut;
use std::time::Instant;
use window::{Event, KeyEvent, Window};

// doomgeneric C interface
extern "C" {
    fn doomgeneric_Create(argc: i32, argv: *const *const u8);
    fn doomgeneric_Tick();
    static mut DG_ScreenBuffer: *mut u32;
}

// WAD / zone memory C interface
extern "C" {
    fn W_CacheLumpNum(lump: i32, tag: i32) -> *mut u8;
    fn W_LumpLength(lump: i32) -> i32;
    fn W_ReleaseLumpNum(lump: i32);
    fn W_GetNumForName(name: *const u8) -> i32;
}

const PU_STATIC: i32 = 1;

// ── Sound module types (matching C structs from i_sound.h) ──

// snddevice_t enum values
const SNDDEVICE_SB: i32 = 3;
const SNDDEVICE_PAS: i32 = 4;
const SNDDEVICE_GUS: i32 = 5;
const SNDDEVICE_WAVEBLASTER: i32 = 6;
const SNDDEVICE_SOUNDCANVAS: i32 = 7;
const SNDDEVICE_AWE32: i32 = 9;

#[repr(C)]
struct SfxInfo {
    tagname: *mut u8,
    name: [u8; 9],
    priority: i32,
    link: *mut SfxInfo,
    pitch: i32,
    volume: i32,
    usefulness: i32,
    lumpnum: i32,
    numchannels: i32,
    driver_data: *mut c_void,
}

#[repr(C)]
pub struct SoundModule {
    sound_devices: *const i32,
    num_sound_devices: i32,
    init: unsafe extern "C" fn(bool) -> bool,
    shutdown: unsafe extern "C" fn(),
    get_sfx_lump_num: unsafe extern "C" fn(*mut SfxInfo) -> i32,
    update: unsafe extern "C" fn(),
    update_sound_params: unsafe extern "C" fn(i32, i32, i32),
    start_sound: unsafe extern "C" fn(*mut SfxInfo, i32, i32, i32) -> i32,
    stop_sound: unsafe extern "C" fn(i32),
    sound_is_playing: unsafe extern "C" fn(i32) -> bool,
    cache_sounds: Option<unsafe extern "C" fn(*mut SfxInfo, i32)>,
}

unsafe impl Sync for SoundModule {}

#[repr(C)]
pub struct MusicModule {
    sound_devices: *const i32,
    num_sound_devices: i32,
    init: unsafe extern "C" fn() -> bool,
    shutdown: unsafe extern "C" fn(),
    set_music_volume: unsafe extern "C" fn(i32),
    pause_music: unsafe extern "C" fn(),
    resume_music: unsafe extern "C" fn(),
    register_song: unsafe extern "C" fn(*mut c_void, i32) -> *mut c_void,
    unregister_song: unsafe extern "C" fn(*mut c_void),
    play_song: unsafe extern "C" fn(*mut c_void, bool),
    stop_song: unsafe extern "C" fn(),
    music_is_playing: unsafe extern "C" fn() -> bool,
    poll: Option<unsafe extern "C" fn()>,
}

unsafe impl Sync for MusicModule {}

// ── Sound module globals ──

#[no_mangle]
pub static mut use_libsamplerate: i32 = 0;
#[no_mangle]
pub static mut libsamplerate_scale: f32 = 0.65;

static SOUND_DEVICES: [i32; 6] = [
    SNDDEVICE_SB,
    SNDDEVICE_PAS,
    SNDDEVICE_GUS,
    SNDDEVICE_WAVEBLASTER,
    SNDDEVICE_SOUNDCANVAS,
    SNDDEVICE_AWE32,
];

static MUSIC_DEVICES: [i32; 1] = [SNDDEVICE_SB];

#[no_mangle]
pub static DG_sound_module: SoundModule = SoundModule {
    sound_devices: SOUND_DEVICES.as_ptr(),
    num_sound_devices: SOUND_DEVICES.len() as i32,
    init: toyos_init_sound,
    shutdown: toyos_shutdown_sound,
    get_sfx_lump_num: toyos_get_sfx_lump_num,
    update: toyos_update_sound,
    update_sound_params: toyos_update_sound_params,
    start_sound: toyos_start_sound,
    stop_sound: toyos_stop_sound,
    sound_is_playing: toyos_sound_is_playing,
    cache_sounds: None,
};

#[no_mangle]
pub static DG_music_module: MusicModule = MusicModule {
    sound_devices: MUSIC_DEVICES.as_ptr(),
    num_sound_devices: 1,
    init: toyos_music_init,
    shutdown: toyos_music_shutdown,
    set_music_volume: toyos_set_music_volume,
    pause_music: toyos_pause_music,
    resume_music: toyos_resume_music,
    register_song: toyos_register_song,
    unregister_song: toyos_unregister_song,
    play_song: toyos_play_song,
    stop_song: toyos_stop_song,
    music_is_playing: toyos_music_is_playing,
    poll: None,
};

// ── Sound mixer implementation ──

const NUM_SFX_CHANNELS: usize = 16;
const OUTPUT_RATE: u32 = 44100;
const SAMPLES_PER_TICK: usize = OUTPUT_RATE as usize / 35; // ~1260
const MIX_BUF_SAMPLES: usize = SAMPLES_PER_TICK * 2; // stereo

struct CachedSound {
    samples: Vec<i16>,
}

#[derive(Clone, Copy)]
struct Channel {
    sound: *const CachedSound,
    pos: u32,
    vol_left: i32,
    vol_right: i32,
    sfxinfo: *mut SfxInfo,
}

const EMPTY_CHANNEL: Channel = Channel {
    sound: core::ptr::null(),
    pos: 0,
    vol_left: 0,
    vol_right: 0,
    sfxinfo: core::ptr::null_mut(),
};

static mut SND_CHANNELS: [Channel; NUM_SFX_CHANNELS] = [EMPTY_CHANNEL; NUM_SFX_CHANNELS];
static mut SND_INITIALIZED: bool = false;
static mut SND_USE_SFX_PREFIX: bool = false;
static mut MIX_BUF: [i32; MIX_BUF_SAMPLES] = [0; MIX_BUF_SAMPLES];
static mut OUT_BUF: [i16; MIX_BUF_SAMPLES] = [0; MIX_BUF_SAMPLES];

unsafe fn cache_sfx(sfxinfo: *mut SfxInfo) -> *const CachedSound {
    if !(*sfxinfo).driver_data.is_null() {
        return (*sfxinfo).driver_data as *const CachedSound;
    }

    let lumpnum = (*sfxinfo).lumpnum;
    let data = W_CacheLumpNum(lumpnum, PU_STATIC);
    let lumplen = W_LumpLength(lumpnum) as u32;

    // Doom SFX header: format(u16)=3, samplerate(u16), num_samples(u32)
    if lumplen < 8 || *data != 0x03 || *data.add(1) != 0x00 {
        return core::ptr::null();
    }

    let samplerate = (*data.add(2) as u32) | ((*data.add(3) as u32) << 8);
    let length = (*data.add(4) as u32)
        | ((*data.add(5) as u32) << 8)
        | ((*data.add(6) as u32) << 16)
        | ((*data.add(7) as u32) << 24);

    if length > lumplen - 8 || length <= 48 {
        return core::ptr::null();
    }

    // Skip 8-byte header + 16-byte DMX padding at start
    let pcm_data = data.add(24);
    let pcm_len = length - 32; // also skip 16-byte DMX padding at end

    let samplerate = if samplerate == 0 { 11025 } else { samplerate };

    // Resample to OUTPUT_RATE with linear interpolation
    let out_len = (pcm_len as u64 * OUTPUT_RATE as u64 / samplerate as u64) as u32;
    if out_len == 0 {
        return core::ptr::null();
    }

    let mut samples = Vec::with_capacity(out_len as usize);
    for i in 0..out_len {
        let src_fixed = i as u64 * samplerate as u64 * 256 / OUTPUT_RATE as u64;
        let src_idx = (src_fixed >> 8) as u32;
        let frac = (src_fixed & 0xFF) as i32;

        let idx = src_idx.min(pcm_len - 1) as usize;
        let s0 = (*pcm_data.add(idx) as i32 - 128) * 256;
        let s1 = if idx + 1 < pcm_len as usize {
            (*pcm_data.add(idx + 1) as i32 - 128) * 256
        } else {
            s0
        };

        let val = s0 + (s1 - s0) * frac / 256;
        samples.push(val as i16);
    }

    W_ReleaseLumpNum(lumpnum);

    let cached = Box::into_raw(Box::new(CachedSound { samples }));
    (*sfxinfo).driver_data = cached as *mut c_void;
    cached
}

unsafe extern "C" fn toyos_init_sound(use_sfx_prefix: bool) -> bool {
    SND_USE_SFX_PREFIX = use_sfx_prefix;
    SND_CHANNELS = [EMPTY_CHANNEL; NUM_SFX_CHANNELS];
    SND_INITIALIZED = true;
    true
}

unsafe extern "C" fn toyos_shutdown_sound() {
    SND_INITIALIZED = false;
}

unsafe extern "C" fn toyos_get_sfx_lump_num(sfx: *mut SfxInfo) -> i32 {
    let sfx = if (*sfx).link.is_null() { sfx } else { (*sfx).link };
    let mut namebuf = [0u8; 10];

    if SND_USE_SFX_PREFIX {
        namebuf[0] = b'd';
        namebuf[1] = b's';
        let mut i = 0;
        while i < 7 && (*sfx).name[i] != 0 {
            namebuf[i + 2] = (*sfx).name[i];
            i += 1;
        }
    } else {
        let mut i = 0;
        while i < 9 && (*sfx).name[i] != 0 {
            namebuf[i] = (*sfx).name[i];
            i += 1;
        }
    }

    W_GetNumForName(namebuf.as_ptr())
}

unsafe extern "C" fn toyos_update_sound() {
    if !SND_INITIALIZED {
        return;
    }

    MIX_BUF = [0; MIX_BUF_SAMPLES];

    for ch in 0..NUM_SFX_CHANNELS {
        let c = &mut SND_CHANNELS[ch];
        if c.sound.is_null() {
            continue;
        }

        let snd = &*c.sound;
        let remaining = snd.samples.len() as u32 - c.pos;
        let to_mix = remaining.min(SAMPLES_PER_TICK as u32);

        for i in 0..to_mix as usize {
            let sample = snd.samples[c.pos as usize + i] as i32;
            MIX_BUF[i * 2] += sample * c.vol_left / 255;
            MIX_BUF[i * 2 + 1] += sample * c.vol_right / 255;
        }

        c.pos += to_mix;
        if c.pos >= snd.samples.len() as u32 {
            c.sound = core::ptr::null();
            c.sfxinfo = core::ptr::null_mut();
        }
    }

    for i in 0..MIX_BUF_SAMPLES {
        OUT_BUF[i] = MIX_BUF[i].clamp(-32768, 32767) as i16;
    }

    let bytes = core::slice::from_raw_parts(
        core::ptr::addr_of!(OUT_BUF) as *const u8,
        MIX_BUF_SAMPLES * 2,
    );
    toyos_abi::syscall::audio_write(bytes);
}

unsafe extern "C" fn toyos_update_sound_params(handle: i32, vol: i32, sep: i32) {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    let c = &mut SND_CHANNELS[handle as usize];
    c.vol_left = ((254 - sep) * vol / 127).clamp(0, 255);
    c.vol_right = (sep * vol / 127).clamp(0, 255);
}

unsafe extern "C" fn toyos_start_sound(
    sfxinfo: *mut SfxInfo,
    channel: i32,
    vol: i32,
    sep: i32,
) -> i32 {
    if !SND_INITIALIZED || channel < 0 || channel >= NUM_SFX_CHANNELS as i32 {
        return -1;
    }

    let c = &mut SND_CHANNELS[channel as usize];
    c.sound = core::ptr::null();
    c.sfxinfo = core::ptr::null_mut();

    let snd = cache_sfx(sfxinfo);
    if snd.is_null() {
        return -1;
    }

    c.sound = snd;
    c.pos = 0;
    c.sfxinfo = sfxinfo;
    toyos_update_sound_params(channel, vol, sep);

    channel
}

unsafe extern "C" fn toyos_stop_sound(handle: i32) {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    let c = &mut SND_CHANNELS[handle as usize];
    c.sound = core::ptr::null();
    c.sfxinfo = core::ptr::null_mut();
}

unsafe extern "C" fn toyos_sound_is_playing(handle: i32) -> bool {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return false;
    }
    !SND_CHANNELS[handle as usize].sound.is_null()
}

// Stub music module (no music playback yet)
unsafe extern "C" fn toyos_music_init() -> bool { true }
unsafe extern "C" fn toyos_music_shutdown() {}
unsafe extern "C" fn toyos_set_music_volume(_volume: i32) {}
unsafe extern "C" fn toyos_pause_music() {}
unsafe extern "C" fn toyos_resume_music() {}
unsafe extern "C" fn toyos_register_song(_data: *mut c_void, _len: i32) -> *mut c_void {
    core::ptr::null_mut()
}
unsafe extern "C" fn toyos_unregister_song(_handle: *mut c_void) {}
unsafe extern "C" fn toyos_play_song(_handle: *mut c_void, _looping: bool) {}
unsafe extern "C" fn toyos_stop_song() {}
unsafe extern "C" fn toyos_music_is_playing() -> bool { false }

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
        0x2C => return Some(KEY_USE),
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

#[no_mangle]
pub extern "C" fn DG_AudioWrite(buf: *const u8, len: u32) {
    if buf.is_null() || len == 0 {
        return;
    }
    let samples = unsafe { core::slice::from_raw_parts(buf, len as usize) };
    toyos_abi::syscall::audio_write(samples);
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
