use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

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

// ── SFX mixer ──
//
// The audio callback runs on a kernel-boosted RT thread with a ~2.9ms deadline.
// It must never block: no locks, no allocation, no syscalls.

// doomgeneric's snd_channels — the engine never allocates more.
const NUM_SFX_CHANNELS: usize = 8;
const OUTPUT_RATE: u32 = 44100;

struct CachedSound {
    samples: Vec<i16>,
}

#[derive(Clone, Copy)]
enum SoundCmd {
    Start {
        channel: usize,
        gen: u32,
        sound: &'static CachedSound,
        vol_left: i32,
        vol_right: i32,
    },
    Stop {
        channel: usize,
    },
    SetParams {
        channel: usize,
        vol_left: i32,
        vol_right: i32,
    },
}

// Doom issues at most ~16 commands per 35Hz tick while the callback drains
// every ~2.9ms; 64 entries cannot fill unless the audio thread is dead.
const CMD_RING_CAP: usize = 64;

struct CmdRing {
    buf: [UnsafeCell<MaybeUninit<SoundCmd>>; CMD_RING_CAP],
    read: AtomicU32,
    write: AtomicU32,
}

// SAFETY: SPSC — only the game thread pushes (writes slot then Release-stores
// `write`), only the audio callback pops (Acquire-loads `write` before reading
// the slot). A slot is never accessed by both sides at once.
unsafe impl Sync for CmdRing {}

static CMD_RING: CmdRing = CmdRing {
    buf: [const { UnsafeCell::new(MaybeUninit::uninit()) }; CMD_RING_CAP],
    read: AtomicU32::new(0),
    write: AtomicU32::new(0),
};

impl CmdRing {
    fn push(&self, cmd: SoundCmd) {
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        assert!(
            w.wrapping_sub(r) < CMD_RING_CAP as u32,
            "sound command ring overflow: audio callback stalled"
        );
        unsafe { (*self.buf[w as usize % CMD_RING_CAP].get()).write(cmd) };
        self.write.store(w.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self) -> Option<SoundCmd> {
        let r = self.read.load(Ordering::Relaxed);
        if r == self.write.load(Ordering::Acquire) {
            return None;
        }
        let cmd = unsafe { (*self.buf[r as usize % CMD_RING_CAP].get()).assume_init() };
        self.read.store(r.wrapping_add(1), Ordering::Release);
        Some(cmd)
    }
}

// Per-channel playing state: bit 0 = playing, bits 1.. = start generation.
// The game thread sets the bit (with a fresh generation) on start and clears it
// on stop; the callback clears it via CAS keyed on the generation when a sound
// finishes. The generation prevents a finish of an OLD sound — racing with a
// new start whose command is still in the ring — from clearing the new sound's
// playing bit.
static CHANNEL_STATE: [AtomicU32; NUM_SFX_CHANNELS] =
    [const { AtomicU32::new(0) }; NUM_SFX_CHANNELS];

struct Channel {
    sound: Option<&'static CachedSound>,
    pos: u32,
    gen: u32,
    vol_left: i32,
    vol_right: i32,
}

struct Mixer {
    channels: [Channel; NUM_SFX_CHANNELS],
}

impl Mixer {
    fn new() -> Self {
        Mixer {
            channels: std::array::from_fn(|_| Channel {
                sound: None,
                pos: 0,
                gen: 0,
                vol_left: 0,
                vol_right: 0,
            }),
        }
    }

    fn apply_commands(&mut self) {
        while let Some(cmd) = CMD_RING.pop() {
            match cmd {
                SoundCmd::Start { channel, gen, sound, vol_left, vol_right } => {
                    self.channels[channel] = Channel { sound: Some(sound), pos: 0, gen, vol_left, vol_right };
                }
                SoundCmd::Stop { channel } => {
                    self.channels[channel].sound = None;
                }
                SoundCmd::SetParams { channel, vol_left, vol_right } => {
                    let c = &mut self.channels[channel];
                    c.vol_left = vol_left;
                    c.vol_right = vol_right;
                }
            }
        }
    }

    fn fill(&mut self, data: &mut [i16]) {
        // cpal does not pre-zero the buffer — every sample must be written here.
        data.fill(0);

        let frames = data.len() / 2;

        for (i, ch) in self.channels.iter_mut().enumerate() {
            let Some(snd) = ch.sound else { continue };

            let remaining = snd.samples.len() as u32 - ch.pos;
            let to_mix = remaining.min(frames as u32);

            for f in 0..to_mix as usize {
                let sample = snd.samples[ch.pos as usize + f] as i32;
                let left = sample * ch.vol_left / 255;
                let right = sample * ch.vol_right / 255;
                data[f * 2] = (data[f * 2] as i32 + left).clamp(-32768, 32767) as i16;
                data[f * 2 + 1] = (data[f * 2 + 1] as i32 + right).clamp(-32768, 32767) as i16;
            }

            ch.pos += to_mix;
            if ch.pos >= snd.samples.len() as u32 {
                ch.sound = None;
                let s = &CHANNEL_STATE[i];
                let _ = s.compare_exchange(ch.gen << 1 | 1, ch.gen << 1, Ordering::Relaxed, Ordering::Relaxed);
            }
        }

        if let Some(ring) = MUSIC_RING.get() {
            ring.read_mix(data);
        }
    }
}

static SND_INITIALIZED: AtomicBool = AtomicBool::new(false);
static SND_USE_SFX_PREFIX: AtomicBool = AtomicBool::new(false);
static AUDIO_STREAM: Mutex<Option<cpal::Stream>> = Mutex::new(None);

unsafe fn cache_sfx(sfxinfo: *mut SfxInfo) -> Option<&'static CachedSound> {
    if !(*sfxinfo).driver_data.is_null() {
        return Some(&*((*sfxinfo).driver_data as *const CachedSound));
    }

    let lumpnum = (*sfxinfo).lumpnum;
    let data = W_CacheLumpNum(lumpnum, PU_STATIC);
    let lumplen = W_LumpLength(lumpnum) as u32;

    // Doom SFX header: format(u16)=3, samplerate(u16), num_samples(u32)
    if lumplen < 8 || *data != 0x03 || *data.add(1) != 0x00 {
        return None;
    }

    let samplerate = (*data.add(2) as u32) | ((*data.add(3) as u32) << 8);
    let length = (*data.add(4) as u32)
        | ((*data.add(5) as u32) << 8)
        | ((*data.add(6) as u32) << 16)
        | ((*data.add(7) as u32) << 24);

    if length > lumplen - 8 || length <= 48 {
        return None;
    }

    // Skip 8-byte header + 16-byte DMX padding at start
    let pcm_data = data.add(24);
    let pcm_len = length - 32; // also skip 16-byte DMX padding at end

    let samplerate = if samplerate == 0 { 11025 } else { samplerate };

    // Resample to OUTPUT_RATE with linear interpolation
    let out_len = (pcm_len as u64 * OUTPUT_RATE as u64 / samplerate as u64) as u32;
    if out_len == 0 {
        return None;
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

    // Leaked: cached for the process lifetime, referenced by the audio callback.
    let cached = Box::leak(Box::new(CachedSound { samples }));
    (*sfxinfo).driver_data = cached as *mut CachedSound as *mut c_void;
    Some(cached)
}

unsafe extern "C" fn toyos_init_sound(use_sfx_prefix: bool) -> bool {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    SND_USE_SFX_PREFIX.store(use_sfx_prefix, Ordering::Relaxed);

    let mut mixer = Mixer::new();

    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device");
    let config = device.default_output_config().expect("no audio config");
    let stream = device
        .build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                mixer.apply_commands();
                mixer.fill(data);
            },
            |err| {
                eprintln!("[doom-sound] audio stream error: {err}");
                // The audio thread exits after this callback, so nothing
                // drains CMD_RING anymore — stop the producers, or their
                // pushes trip the ring's overflow assert and abort the game.
                SND_INITIALIZED.store(false, Ordering::Relaxed);
            },
            None,
        )
        .expect("failed to build audio stream");
    stream.play().expect("failed to start audio stream");
    *AUDIO_STREAM.lock().unwrap() = Some(stream);

    SND_INITIALIZED.store(true, Ordering::Relaxed);
    true
}

unsafe extern "C" fn toyos_shutdown_sound() {
    SND_INITIALIZED.store(false, Ordering::Relaxed);
    drop(AUDIO_STREAM.lock().unwrap().take());
}

unsafe extern "C" fn toyos_get_sfx_lump_num(sfx: *mut SfxInfo) -> i32 {
    let sfx = if (*sfx).link.is_null() { sfx } else { (*sfx).link };
    let mut namebuf = [0u8; 10];

    if SND_USE_SFX_PREFIX.load(Ordering::Relaxed) {
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

unsafe extern "C" fn toyos_update_sound() {}

unsafe extern "C" fn toyos_update_sound_params(handle: i32, vol: i32, sep: i32) {
    if !SND_INITIALIZED.load(Ordering::Relaxed) || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    CMD_RING.push(SoundCmd::SetParams {
        channel: handle as usize,
        vol_left: ((254 - sep) * vol / 127).clamp(0, 255),
        vol_right: (sep * vol / 127).clamp(0, 255),
    });
}

unsafe extern "C" fn toyos_start_sound(
    sfxinfo: *mut SfxInfo,
    channel: i32,
    vol: i32,
    sep: i32,
) -> i32 {
    if !SND_INITIALIZED.load(Ordering::Relaxed) || channel < 0 || channel >= NUM_SFX_CHANNELS as i32 {
        return -1;
    }

    let Some(sound) = cache_sfx(sfxinfo) else {
        return -1;
    };

    let ch = channel as usize;
    let state = &CHANNEL_STATE[ch];
    let gen = (state.load(Ordering::Relaxed) >> 1).wrapping_add(1);
    state.store(gen << 1 | 1, Ordering::Relaxed);
    CMD_RING.push(SoundCmd::Start {
        channel: ch,
        gen,
        sound,
        vol_left: ((254 - sep) * vol / 127).clamp(0, 255),
        vol_right: (sep * vol / 127).clamp(0, 255),
    });

    channel
}

unsafe extern "C" fn toyos_stop_sound(handle: i32) {
    if !SND_INITIALIZED.load(Ordering::Relaxed) || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return;
    }
    let state = &CHANNEL_STATE[handle as usize];
    state.store(state.load(Ordering::Relaxed) & !1, Ordering::Relaxed);
    CMD_RING.push(SoundCmd::Stop { channel: handle as usize });
}

unsafe extern "C" fn toyos_sound_is_playing(handle: i32) -> bool {
    if !SND_INITIALIZED.load(Ordering::Relaxed) || handle < 0 || handle >= NUM_SFX_CHANNELS as i32 {
        return false;
    }
    CHANNEL_STATE[handle as usize].load(Ordering::Relaxed) & 1 != 0
}

// ── Music ──

// TimGM6mb by Tim Brechbill (GPL-2.0, like doomgeneric itself): a compact ~6MB
// General MIDI soundfont. Downloaded into assets/ by build.rs and shipped in
// the initrd rather than embedded, so the binary stays small.
const SOUNDFONT_PATH: &str = "/share/timgm6mb.sf2";

// ~3s of render-ahead at 44100Hz. On a saturated single core the game thread
// starves the midi-synth thread for hundreds of ms at a time, and a ring this
// deep lets the producer bank audio in idle moments and coast through those
// windows. Power of two so the wrapping-counter slot math is a mask, not a
// per-frame division in the RT callback.
const RING_FRAMES: usize = 131072;
const RENDER_CHUNK: usize = 1024;

// Disabling this skips rustysynth's whole effects pass and roughly halves
// synthesis cost, at an audible quality cost. Do not flip it to buy CPU
// without asking.
const ENABLE_REVERB_AND_CHORUS: bool = true;

enum MusicCmd {
    Play(Arc<MidiFile>, bool),
    Stop,
}

// SPSC ring of pre-rendered stereo i16 frames: the midi-synth thread pushes,
// the audio callback drains. Volume is applied at read time, and clear() marks
// buffered frames droppable rather than waiting for them — so neither a volume
// change nor a song switch is delayed by the ring's ~3s of depth.
struct MusicRing {
    buf: Box<[UnsafeCell<i16>]>,
    read: AtomicUsize,
    write: AtomicUsize,
    // Producer-requested drop of buffered frames; applied by the consumer
    // (which owns `read`) at the start of the next read_mix.
    clear_to: AtomicUsize,
    clear_pending: AtomicBool,
    // Fixed-point volume, 0..=256 == 0.0..=1.0.
    volume: AtomicU32,
    paused: AtomicBool,
    playing: AtomicBool,
}

// SAFETY: SPSC — the producer writes only slots in the free region before
// Release-publishing `write`; the consumer Acquire-loads `write` before
// reading and only ever advances `read` (a clear jumps it forward, never back).
unsafe impl Sync for MusicRing {}

impl MusicRing {
    fn new() -> Self {
        MusicRing {
            buf: (0..RING_FRAMES * 2).map(|_| UnsafeCell::new(0)).collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            clear_to: AtomicUsize::new(0),
            clear_pending: AtomicBool::new(false),
            volume: AtomicU32::new(256),
            paused: AtomicBool::new(false),
            playing: AtomicBool::new(false),
        }
    }

    fn free_space(&self) -> usize {
        let used = self.write.load(Ordering::Relaxed).wrapping_sub(self.read.load(Ordering::Acquire));
        RING_FRAMES - used
    }

    fn push(&self, left: &[f32], right: &[f32]) {
        let mut w = self.write.load(Ordering::Relaxed);
        for i in 0..left.len() {
            let idx = (w % RING_FRAMES) * 2;
            unsafe {
                *self.buf[idx].get() = (left[i] * 32767.0).clamp(-32768.0, 32767.0) as i16;
                *self.buf[idx + 1].get() = (right[i] * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
            w = w.wrapping_add(1);
        }
        self.write.store(w, Ordering::Release);
    }

    fn read_mix(&self, data: &mut [i16]) {
        let mut r = self.read.load(Ordering::Relaxed);
        // Load `write` only after consuming the clear flag: the Acquire swap
        // pairs with clear()'s Release store, so the snapshot can never be
        // older than clear_to — a stale snapshot would make the jump below
        // look like a rewind and silently drop the clear request.
        let clear = self.clear_pending.swap(false, Ordering::Acquire);
        let w = self.write.load(Ordering::Acquire);

        if clear {
            // Jump forward only: if the request became visible after frames past
            // the marker were already consumed, jumping back would replay them.
            let ct = self.clear_to.load(Ordering::Relaxed);
            if w.wrapping_sub(ct) <= w.wrapping_sub(r) {
                r = ct;
            }
        }

        if self.paused.load(Ordering::Relaxed) || !self.playing.load(Ordering::Relaxed) {
            self.read.store(r, Ordering::Release);
            return;
        }

        let vol = self.volume.load(Ordering::Relaxed) as i32;
        let frames = data.len() / 2;
        let avail = w.wrapping_sub(r).min(frames);
        for i in 0..avail {
            let idx = (r % RING_FRAMES) * 2;
            let (l, right) = unsafe { (*self.buf[idx].get() as i32, *self.buf[idx + 1].get() as i32) };
            data[i * 2] = (data[i * 2] as i32 + ((l * vol) >> 8)).clamp(-32768, 32767) as i16;
            data[i * 2 + 1] = (data[i * 2 + 1] as i32 + ((right * vol) >> 8)).clamp(-32768, 32767) as i16;
            r = r.wrapping_add(1);
        }
        self.read.store(r, Ordering::Release);
    }

    fn clear(&self) {
        self.clear_to.store(self.write.load(Ordering::Relaxed), Ordering::Relaxed);
        self.clear_pending.store(true, Ordering::Release);
    }

    fn is_empty(&self) -> bool {
        self.write.load(Ordering::Relaxed) == self.read.load(Ordering::Acquire)
    }
}

static MUSIC_RING: OnceLock<Arc<MusicRing>> = OnceLock::new();
static MUSIC_TX: Mutex<Option<mpsc::Sender<MusicCmd>>> = Mutex::new(None);
static MUSIC_THREAD: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

fn handle_music_cmd(
    cmd: MusicCmd,
    sequencer: &mut Option<MidiFileSequencer>,
    ring: &MusicRing,
    sf: &Arc<SoundFont>,
) {
    match cmd {
        MusicCmd::Play(midi_file, looping) => {
            let mut settings = SynthesizerSettings::new(OUTPUT_RATE as i32);
            settings.enable_reverb_and_chorus = ENABLE_REVERB_AND_CHORUS;
            let synth = Synthesizer::new(sf, &settings).expect("failed to create synthesizer");
            let mut seq = MidiFileSequencer::new(synth);
            seq.play(&midi_file, looping);
            *sequencer = Some(seq);
            ring.clear();
            ring.playing.store(true, Ordering::Relaxed);
        }
        MusicCmd::Stop => {
            *sequencer = None;
            ring.playing.store(false, Ordering::Relaxed);
            ring.clear();
        }
    }
}

/// Reports the real-time factor: CPU cost of rendering one second of audio.
/// Buffering bridges spikes, but a sustained rt >= 1.0 cannot be hidden.
fn music_telemetry(ring: &MusicRing, render_cost: std::time::Duration) {
    use std::sync::Mutex;
    use std::time::Instant;
    struct Tel { window_start: Instant, cpu: std::time::Duration, chunks: u32 }
    static TEL: Mutex<Option<Tel>> = Mutex::new(None);
    let mut g = TEL.lock().unwrap();
    let t = g.get_or_insert_with(|| Tel { window_start: Instant::now(), cpu: Default::default(), chunks: 0 });
    t.cpu += render_cost;
    t.chunks += 1;
    let wall = t.window_start.elapsed();
    if wall.as_secs() >= 5 {
        let audio_s = t.chunks as f64 * RENDER_CHUNK as f64 / OUTPUT_RATE as f64;
        let rt = t.cpu.as_secs_f64() / audio_s;
        let fill = 100 * (RING_FRAMES - ring.free_space()) / RING_FRAMES;
        eprintln!("[music] rt_factor={rt:.2} rendered={audio_s:.1}s/{:.1}s ring={fill}%", wall.as_secs_f64());
        *t = Tel { window_start: Instant::now(), cpu: Default::default(), chunks: 0 };
    }
}

fn music_thread(ring: Arc<MusicRing>, rx: mpsc::Receiver<MusicCmd>, sf: Arc<SoundFont>) {
    let mut sequencer: Option<MidiFileSequencer> = None;
    // A finished non-looping song leaves up to a full ring (~3s) of rendered
    // frames buffered; `playing` must stay true until the callback consumes
    // them or the tail is cut off.
    let mut draining = false;
    let mut left_buf = vec![0.0f32; RENDER_CHUNK];
    let mut right_buf = vec![0.0f32; RENDER_CHUNK];

    loop {
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    handle_music_cmd(cmd, &mut sequencer, &ring, &sf);
                    draining = false;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        let Some(seq) = &mut sequencer else {
            if draining {
                if ring.is_empty() {
                    ring.playing.store(false, Ordering::Relaxed);
                    draining = false;
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                continue;
            }
            match rx.recv() {
                Ok(cmd) => {
                    handle_music_cmd(cmd, &mut sequencer, &ring, &sf);
                    draining = false;
                }
                Err(_) => return,
            }
            continue;
        };

        if ring.paused.load(Ordering::Relaxed) {
            // Pause/resume arrive via atomic flags, not the channel, so the
            // paused state has to be polled.
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }

        // Render whenever a chunk fits rather than waiting for the ring to
        // drain to a low-water mark: topping the bank up in every idle moment
        // is what gives playback its full depth to coast on.
        if ring.free_space() >= RENDER_CHUNK {
            let t0 = std::time::Instant::now();
            seq.render(&mut left_buf, &mut right_buf);
            music_telemetry(&ring, t0.elapsed());
            ring.push(&left_buf, &right_buf);
            if seq.end_of_sequence() {
                sequencer = None;
                draining = true;
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

unsafe extern "C" fn toyos_music_init() -> bool {
    let sf2 = std::fs::read(SOUNDFONT_PATH)
        .unwrap_or_else(|e| panic!("failed to read {SOUNDFONT_PATH}: {e}"));
    let sf = Arc::new(
        SoundFont::new(&mut std::io::Cursor::new(sf2))
            .unwrap_or_else(|e| panic!("failed to parse {SOUNDFONT_PATH}: {e:?}")),
    );

    let ring = Arc::new(MusicRing::new());
    MUSIC_RING.set(ring.clone()).unwrap_or_else(|_| panic!("music initialized twice"));

    let (tx, rx) = mpsc::channel();
    *MUSIC_TX.lock().unwrap() = Some(tx);

    let handle = std::thread::Builder::new()
        .name("midi-synth".into())
        .spawn(move || music_thread(ring, rx, sf))
        .expect("failed to spawn music thread");
    *MUSIC_THREAD.lock().unwrap() = Some(handle);

    true
}

unsafe extern "C" fn toyos_music_shutdown() {
    // Dropping the sender disconnects the channel; the thread exits on Disconnected.
    drop(MUSIC_TX.lock().unwrap().take());
    if let Some(handle) = MUSIC_THREAD.lock().unwrap().take() {
        handle.join().expect("music thread panicked");
    }
    if let Some(ring) = MUSIC_RING.get() {
        ring.playing.store(false, Ordering::Relaxed);
    }
}

unsafe extern "C" fn toyos_set_music_volume(volume: i32) {
    // DOOM music volume is 0–15
    let vol = volume.clamp(0, 15) as u32 * 256 / 15;
    if let Some(ring) = MUSIC_RING.get() {
        ring.volume.store(vol, Ordering::Relaxed);
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
    if let Some(tx) = MUSIC_TX.lock().unwrap().as_ref() {
        tx.send(MusicCmd::Play(midi_file.clone(), looping)).expect("music thread gone");
    }
}

unsafe extern "C" fn toyos_stop_song() {
    if let Some(tx) = MUSIC_TX.lock().unwrap().as_ref() {
        tx.send(MusicCmd::Stop).expect("music thread gone");
    }
}

unsafe extern "C" fn toyos_music_is_playing() -> bool {
    if let Some(ring) = MUSIC_RING.get() {
        ring.playing.load(Ordering::Relaxed) && !ring.paused.load(Ordering::Relaxed)
    } else {
        false
    }
}
