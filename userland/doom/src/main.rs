use core::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::addr_of_mut;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

use softbuffer::Surface;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowAttributes, WindowId};

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

// MUS-to-MIDI conversion (mus2mid.c / memio.c)
extern "C" {
    fn mem_fopen_read(buf: *const u8, buflen: usize) -> *mut c_void;
    fn mem_fopen_write() -> *mut c_void;
    fn mem_get_buf(stream: *mut c_void, buf: *mut *mut u8, buflen: *mut usize);
    fn mem_fclose(stream: *mut c_void);
    fn mus2mid(input: *mut c_void, output: *mut c_void) -> i32;
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

struct CachedSound {
    samples: Vec<i16>,
}

struct Channel {
    sound: *const CachedSound,
    pos: u32,
    vol_left: i32,
    vol_right: i32,
    sfxinfo: *mut SfxInfo,
}

unsafe impl Send for Channel {}

impl Channel {
    const EMPTY: Self = Channel {
        sound: core::ptr::null(),
        pos: 0,
        vol_left: 0,
        vol_right: 0,
        sfxinfo: core::ptr::null_mut(),
    };
}

const RING_FRAMES: usize = 8192;
const RENDER_CHUNK: usize = 1024;

enum MusicCmd {
    Play(Arc<MidiFile>, bool),
    Stop,
}

struct MusicRing {
    buf: Box<[i16]>,
    read: AtomicUsize,
    write: AtomicUsize,
    volume: AtomicU32,
    paused: AtomicBool,
    playing: AtomicBool,
}

unsafe impl Sync for MusicRing {}

impl MusicRing {
    fn new() -> Self {
        MusicRing {
            buf: vec![0i16; RING_FRAMES * 2].into_boxed_slice(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            volume: AtomicU32::new(f32::to_bits(1.0)),
            paused: AtomicBool::new(false),
            playing: AtomicBool::new(false),
        }
    }

    fn free_space(&self) -> usize {
        let used = self.write.load(Ordering::Acquire).wrapping_sub(self.read.load(Ordering::Relaxed));
        RING_FRAMES - used
    }

    fn push(&self, left: &[f32], right: &[f32]) {
        let vol = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let mut w = self.write.load(Ordering::Relaxed);
        let ptr = self.buf.as_ptr() as *mut i16;
        for i in 0..left.len() {
            let idx = (w % RING_FRAMES) * 2;
            unsafe {
                *ptr.add(idx) = (left[i] * vol * 32767.0).clamp(-32768.0, 32767.0) as i16;
                *ptr.add(idx + 1) = (right[i] * vol * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
            w = w.wrapping_add(1);
        }
        self.write.store(w, Ordering::Release);
    }

    fn read_mix(&self, data: &mut [i16]) {
        if self.paused.load(Ordering::Relaxed) || !self.playing.load(Ordering::Relaxed) {
            return;
        }
        let frames = data.len() / 2;
        let mut r = self.read.load(Ordering::Relaxed);
        let w = self.write.load(Ordering::Acquire);
        let avail = w.wrapping_sub(r).min(frames);
        let ptr = self.buf.as_ptr();
        for i in 0..avail {
            let idx = (r % RING_FRAMES) * 2;
            unsafe {
                data[i * 2] = (data[i * 2] as i32 + *ptr.add(idx) as i32).clamp(-32768, 32767) as i16;
                data[i * 2 + 1] = (data[i * 2 + 1] as i32 + *ptr.add(idx + 1) as i32).clamp(-32768, 32767) as i16;
            }
            r = r.wrapping_add(1);
        }
        self.read.store(r, Ordering::Release);
    }

    fn clear(&self) {
        self.read.store(self.write.load(Ordering::Relaxed), Ordering::Release);
    }
}

static SOUNDFONT: OnceLock<Arc<SoundFont>> = OnceLock::new();
static MUSIC_RING: OnceLock<Arc<MusicRing>> = OnceLock::new();
static MUSIC_TX: OnceLock<Mutex<mpsc::Sender<MusicCmd>>> = OnceLock::new();

struct Mixer {
    channels: [Channel; NUM_SFX_CHANNELS],
}

unsafe impl Send for Mixer {}

impl Mixer {
    fn new() -> Self {
        Mixer {
            channels: std::array::from_fn(|_| Channel::EMPTY),
        }
    }

    /// Mix stereo i16 samples directly into the output buffer.
    /// Called from the cpal audio callback at hardware rate.
    fn fill(&mut self, data: &mut [i16]) {
        for s in data.iter_mut() {
            *s = 0;
        }

        let frames = data.len() / 2;

        for ch in &mut self.channels {
            if ch.sound.is_null() {
                continue;
            }

            let snd = unsafe { &*ch.sound };
            let remaining = snd.samples.len() as u32 - ch.pos;
            let to_mix = remaining.min(frames as u32);

            for i in 0..to_mix as usize {
                let sample = snd.samples[ch.pos as usize + i] as i32;
                let left = (sample * ch.vol_left / 255).clamp(-32768, 32767);
                let right = (sample * ch.vol_right / 255).clamp(-32768, 32767);
                data[i * 2] = (data[i * 2] as i32 + left).clamp(-32768, 32767) as i16;
                data[i * 2 + 1] = (data[i * 2 + 1] as i32 + right).clamp(-32768, 32767) as i16;
            }

            ch.pos += to_mix;
            if ch.pos >= snd.samples.len() as u32 {
                ch.sound = core::ptr::null();
                ch.sfxinfo = core::ptr::null_mut();
            }
        }

        // Mix pre-rendered music from the ring buffer (rendered by background thread)
        if let Some(ring) = MUSIC_RING.get() {
            ring.read_mix(data);
        }
    }
}

static mut SND_INITIALIZED: bool = false;
static mut SND_USE_SFX_PREFIX: bool = false;

static mut MIXER: Option<Arc<Mutex<Mixer>>> = None;
// Keep the stream alive — dropping it stops playback.
static mut _AUDIO_STREAM: Option<cpal::Stream> = None;

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
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    SND_USE_SFX_PREFIX = use_sfx_prefix;

    let mixer = Arc::new(Mutex::new(Mixer::new()));
    MIXER = Some(mixer.clone());

    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                let mut mixer = mixer.lock().unwrap();
                mixer.fill(data);
            },
            |err| eprintln!("audio stream error: {err}"),
            None,
        )
        .expect("failed to build audio stream");
    stream.play().expect("failed to start audio stream");
    _AUDIO_STREAM = Some(stream);

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
    // Mixing is done in the cpal audio callback (Mixer::fill).
    // Nothing to do here.
}

unsafe extern "C" fn toyos_update_sound_params(handle: i32, vol: i32, sep: i32) {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    if let Some(mixer) = &*addr_of_mut!(MIXER) {
        let mut mixer = mixer.lock().unwrap();
        let c = &mut mixer.channels[handle as usize];
        c.vol_left = ((254 - sep) * vol / 127).clamp(0, 255);
        c.vol_right = (sep * vol / 127).clamp(0, 255);
    }
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

    let snd = cache_sfx(sfxinfo);
    if snd.is_null() {
        return -1;
    }

    if let Some(mixer) = &*addr_of_mut!(MIXER) {
        let mut mixer = mixer.lock().unwrap();
        let c = &mut mixer.channels[channel as usize];
        c.sound = snd;
        c.pos = 0;
        c.sfxinfo = sfxinfo;
        c.vol_left = ((254 - sep) * vol / 127).clamp(0, 255);
        c.vol_right = (sep * vol / 127).clamp(0, 255);
    }

    channel
}

unsafe extern "C" fn toyos_stop_sound(handle: i32) {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    if let Some(mixer) = &*addr_of_mut!(MIXER) {
        let mut mixer = mixer.lock().unwrap();
        let c = &mut mixer.channels[handle as usize];
        c.sound = core::ptr::null();
        c.sfxinfo = core::ptr::null_mut();
    }
}

unsafe extern "C" fn toyos_sound_is_playing(handle: i32) -> bool {
    if !SND_INITIALIZED || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return false;
    }
    if let Some(mixer) = &*addr_of_mut!(MIXER) {
        let mixer = mixer.lock().unwrap();
        !mixer.channels[handle as usize].sound.is_null()
    } else {
        false
    }
}

static SF2_DATA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/FluidR3_GM.sf2"));

fn music_thread(ring: Arc<MusicRing>, rx: mpsc::Receiver<MusicCmd>, sf: Arc<SoundFont>) {
    let mut sequencer: Option<MidiFileSequencer> = None;
    let mut left_buf = vec![0.0f32; RENDER_CHUNK];
    let mut right_buf = vec![0.0f32; RENDER_CHUNK];

    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                MusicCmd::Play(midi_file, looping) => {
                    let settings = SynthesizerSettings::new(OUTPUT_RATE as i32);
                    let synth = Synthesizer::new(&sf, &settings).expect("failed to create synthesizer");
                    let mut seq = MidiFileSequencer::new(synth);
                    seq.play(&midi_file, looping);
                    sequencer = Some(seq);
                    ring.clear();
                    ring.playing.store(true, Ordering::Release);
                }
                MusicCmd::Stop => {
                    sequencer = None;
                    ring.playing.store(false, Ordering::Release);
                    ring.clear();
                }
            }
        }

        if let Some(seq) = &mut sequencer {
            if ring.paused.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            let buffered = RING_FRAMES - ring.free_space();
            if buffered >= RING_FRAMES / 2 {
                // Enough audio buffered (~93ms) — sleep until it drains
                std::thread::sleep(std::time::Duration::from_millis(20));
            } else if ring.free_space() >= RENDER_CHUNK {
                seq.render(&mut left_buf, &mut right_buf);
                ring.push(&left_buf, &right_buf);
                if seq.end_of_sequence() {
                    sequencer = None;
                    ring.playing.store(false, Ordering::Release);
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

unsafe extern "C" fn toyos_music_init() -> bool {
    let sf = match SoundFont::new(&mut std::io::Cursor::new(SF2_DATA)) {
        Ok(sf) => sf,
        Err(e) => {
            eprintln!("failed to load soundfont: {e:?}");
            return false;
        }
    };
    let sf = Arc::new(sf);
    SOUNDFONT.set(sf.clone()).ok();

    let ring = Arc::new(MusicRing::new());
    MUSIC_RING.set(ring.clone()).ok();

    let (tx, rx) = mpsc::channel();
    MUSIC_TX.set(Mutex::new(tx)).ok();

    std::thread::Builder::new()
        .name("midi-synth".into())
        .spawn(move || music_thread(ring, rx, sf))
        .expect("failed to spawn music thread");

    true
}

unsafe extern "C" fn toyos_music_shutdown() {
    if let Some(tx) = MUSIC_TX.get() {
        let _ = tx.lock().unwrap().send(MusicCmd::Stop);
    }
}

unsafe extern "C" fn toyos_set_music_volume(volume: i32) {
    // DOOM music volume is 0–15
    let vol = (volume as f32 / 15.0).clamp(0.0, 1.0);
    if let Some(ring) = MUSIC_RING.get() {
        ring.volume.store(vol.to_bits(), Ordering::Relaxed);
    }
}

unsafe extern "C" fn toyos_pause_music() {
    if let Some(ring) = MUSIC_RING.get() {
        ring.paused.store(true, Ordering::Relaxed);
    }
}

unsafe extern "C" fn toyos_resume_music() {
    if let Some(ring) = MUSIC_RING.get() {
        ring.paused.store(false, Ordering::Relaxed);
    }
}

unsafe extern "C" fn toyos_register_song(data: *mut c_void, len: i32) -> *mut c_void {
    if data.is_null() || len < 4 {
        return core::ptr::null_mut();
    }

    let raw = core::slice::from_raw_parts(data as *const u8, len as usize);

    // MUS format starts with "MUS\x1A", MIDI starts with "MThd"
    let midi_data = if raw.starts_with(b"MUS\x1a") {
        let input = mem_fopen_read(data as *const u8, len as usize);
        let output = mem_fopen_write();
        mus2mid(input, output);

        let mut buf: *mut u8 = core::ptr::null_mut();
        let mut buflen: usize = 0;
        mem_get_buf(output, &mut buf, &mut buflen);

        let midi = if !buf.is_null() && buflen > 0 {
            core::slice::from_raw_parts(buf, buflen).to_vec()
        } else {
            mem_fclose(input);
            mem_fclose(output);
            return core::ptr::null_mut();
        };

        mem_fclose(input);
        mem_fclose(output);
        midi
    } else {
        raw.to_vec()
    };

    let midi_file = match MidiFile::new(&mut std::io::Cursor::new(&midi_data)) {
        Ok(mf) => mf,
        Err(e) => {
            eprintln!("failed to parse MIDI: {e:?}");
            return core::ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(Arc::new(midi_file))) as *mut c_void
}

unsafe extern "C" fn toyos_unregister_song(handle: *mut c_void) {
    if !handle.is_null() {
        drop(Box::from_raw(handle as *mut Arc<MidiFile>));
    }
}

unsafe extern "C" fn toyos_play_song(handle: *mut c_void, looping: bool) {
    if handle.is_null() {
        return;
    }
    let midi_file = &*(handle as *const Arc<MidiFile>);
    if let Some(tx) = MUSIC_TX.get() {
        let _ = tx.lock().unwrap().send(MusicCmd::Play(midi_file.clone(), looping));
    }
}

unsafe extern "C" fn toyos_stop_song() {
    if let Some(tx) = MUSIC_TX.get() {
        let _ = tx.lock().unwrap().send(MusicCmd::Stop);
    }
}

unsafe extern "C" fn toyos_music_is_playing() -> bool {
    if let Some(ring) = MUSIC_RING.get() {
        ring.playing.load(Ordering::Relaxed) && !ring.paused.load(Ordering::Relaxed)
    } else {
        false
    }
}

// ── Globals for C callback access ──

static mut START_TIME: Option<Instant> = None;

// Key event ring buffer
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
        // Letter keys (QWERTY physical layout)
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
        // Number keys
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

// ── Winit application ──

const SRC_W: usize = 640;
const SRC_H: usize = 400;

struct DoomApp {
    window: Option<Arc<dyn Window>>,
    surface: Option<Surface<winit::event_loop::OwnedDisplayHandle, Arc<dyn Window>>>,
    context: Option<softbuffer::Context<winit::event_loop::OwnedDisplayHandle>>,
}

impl ApplicationHandler for DoomApp {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attrs = WindowAttributes::default()
            .with_title("DOOM")
            .with_surface_size(winit::dpi::LogicalSize::new(960, 600));
        let window: Arc<dyn Window> = event_loop.create_window(attrs).expect("failed to create window").into();

        let display = event_loop.owned_display_handle();
        let context = softbuffer::Context::new(display).expect("failed to create softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("failed to create surface");

        self.window = Some(window);
        self.surface = Some(surface);
        self.context = Some(context);

        // Initialize doom — this calls DG_Init() which just sets START_TIME
        // Leak: doom stores myargv globally and reads it on every tick.
        let argv: Vec<*const u8> = vec![
            b"doom\0".as_ptr(),
            b"-iwad\0".as_ptr(),
            b"/share/doom1.wad\0".as_ptr(),
        ];
        let argv = argv.leak();
        unsafe {
            doomgeneric_Create(argv.len() as i32, argv.as_ptr());
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &dyn ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => std::process::exit(0),
            WindowEvent::KeyboardInput { event, .. } => {
                handle_winit_key(&event);
            }
            WindowEvent::RedrawRequested => {
                self.draw_frame();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.window.is_some() {
            unsafe { doomgeneric_Tick(); }
            if let Some(window) = &self.window {
                window.request_redraw();
            }
            // Doom paces itself via DG_SleepMs. Between ticks, block on events
            // (input, compositor frames) instead of spinning.
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

impl DoomApp {
    fn draw_frame(&mut self) {
        let surface = match self.surface.as_mut() {
            Some(s) => s,
            None => return,
        };

        let window = self.window.as_ref().unwrap();
        let size = window.surface_size();
        let dst_w = size.width as usize;
        let dst_h = size.height as usize;
        if dst_w == 0 || dst_h == 0 {
            return;
        }

        surface
            .resize(NonZeroU32::new(size.width).unwrap(), NonZeroU32::new(size.height).unwrap())
            .expect("failed to resize surface");

        let mut buffer = surface.next_buffer().expect("failed to get buffer");
        let stride = buffer.byte_stride().get() as usize / 4;

        unsafe {
            let src = DG_ScreenBuffer;
            if src.is_null() {
                return;
            }

            // Precompute X mapping table for nearest-neighbor scaling
            let mut x_map = [0usize; 2560];
            let map = &mut x_map[..dst_w];
            for dx in 0..dst_w {
                map[dx] = dx * SRC_W / dst_w;
            }

            let dst = buffer.pixels().as_mut_ptr() as *mut u32;
            let mut prev_sy = usize::MAX;
            for dy in 0..dst_h {
                let sy = dy * SRC_H / dst_h;
                let dst_row = dst.add(dy * stride);

                if sy == prev_sy && dy > 0 {
                    // Same source row — memcpy the already-scaled row
                    core::ptr::copy_nonoverlapping(dst.add((dy - 1) * stride), dst_row, dst_w);
                } else {
                    let src_row = src.add(sy * SRC_W);
                    for dx in 0..dst_w {
                        // DOOM's XRGB (0x00RRGGBB) → softbuffer pixel with alpha=0xFF
                        *dst_row.add(dx) = *src_row.add(*map.get_unchecked(dx)) | 0xFF000000;
                    }
                }
                prev_sy = sy;
            }
        }

        window.pre_present_notify();
        buffer.present().expect("failed to present buffer");
    }
}

fn handle_winit_key(event: &KeyEvent) {
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

// ── DG_* implementations (called by C code) ──

#[no_mangle]
pub extern "C" fn DG_Init() {
    unsafe {
        START_TIME = Some(Instant::now());
    }
}

#[no_mangle]
pub extern "C" fn DG_DrawFrame() {
    // Drawing is handled by the winit event loop (RedrawRequested).
    // This C callback is a no-op — the actual frame blit happens in DoomApp::draw_frame().
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
pub extern "C" fn DG_AudioWrite(_buf: *const u8, _len: u32) {
    // Audio mixing is handled directly in the cpal callback via the shared Mixer.
}

fn main() {
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let app = DoomApp {
        window: None,
        surface: None,
        context: None,
    };
    event_loop.run_app(app).expect("event loop error");
}
