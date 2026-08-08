use core::cell::UnsafeCell;
use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

use rustysynth::{MidiFile, MidiFileSequencer, SoundFont, Synthesizer, SynthesizerSettings};

use crate::ffi::boundary;

// WAD / zone memory C interface
extern "C" {
    fn W_CacheLumpNum(lump: i32, tag: i32) -> *mut u8;
    fn W_LumpLength(lump: i32) -> i32;
    fn W_ReleaseLumpNum(lump: i32);
    fn W_GetNumForName(name: *const u8) -> i32;
}

// MUS-to-MIDI conversion (mus2mid.c / memio.c)
extern "C" {
    /// doomgeneric's zone allocator, which `memio.c` allocates out of.
    /// `D_DoomMain` calls it and `music_check` runs before there is one.
    fn Z_Init();
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
// The audio callback has a ~2.9ms deadline and must never block: no locks, no
// allocation, no syscalls. Its RT standing is lent rather than held — soundd
// holds the band with the device claim and every wake it sends passes a window
// along — so on a machine with no audio device the callback is an ordinary
// thread and the deadline is met only by whatever the scheduler decides.

// doomgeneric's snd_channels — the engine never allocates more.
const NUM_SFX_CHANNELS: usize = 8;
const OUTPUT_RATE: u32 = 44100;

struct CachedSound {
    samples: Vec<i16>,
}

// What the game thread has to say about a channel, published for the audio
// callback to read once per period. There is no queue between them, because
// the sound module's interface has no events in it: every call names a channel
// and supersedes every earlier call about that channel. `start_sound` replaces
// the channel outright, `stop_sound` silences whatever it held, and
// `update_sound_params` overwrites a volume that was only ever the current
// value of a state variable. So the whole of what a callback that stopped
// consuming has to catch up on is eight records, and a record is overwritten
// in place: the producer cannot outrun the consumer because there is nothing
// to fill.

// Bit 0 is `playing`; the rest is the start generation, bumped by every
// `start_sound`. The generation is what makes a start a state rather than an
// event — a callback that has not seen this one plays the sound from the
// beginning — and it keeps the finish of an old sound from clearing the
// playing bit of a new one that replaced it.
static CHANNEL_STATE: [AtomicU32; NUM_SFX_CHANNELS] =
    [const { AtomicU32::new(0) }; NUM_SFX_CHANNELS];

// `vol_left | vol_right << 16`, each 0..=255. Its own word, so a volume
// update never fails the callback's finish CAS on `CHANNEL_STATE`.
static CHANNEL_VOLUME: [AtomicU32; NUM_SFX_CHANNELS] =
    [const { AtomicU32::new(0) }; NUM_SFX_CHANNELS];

// The sound the current generation names, published before the generation is.
static CHANNEL_SOUND: [AtomicPtr<CachedSound>; NUM_SFX_CHANNELS] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; NUM_SFX_CHANNELS];

/// Periods the callback has mixed: the consumer's own progress, and the only
/// clock a producer's rate means anything against.
static MIXED_PERIODS: AtomicU32 = AtomicU32::new(0);

fn pack_volume(vol: i32, sep: i32) -> u32 {
    let left = ((254 - sep) * vol / 127).clamp(0, 255) as u32;
    let right = (sep * vol / 127).clamp(0, 255) as u32;
    left | right << 16
}

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

    fn sync_channels(&mut self) {
        for (i, ch) in self.channels.iter_mut().enumerate() {
            let state = CHANNEL_STATE[i].load(Ordering::Acquire);
            if state & 1 == 0 {
                ch.sound = None;
                continue;
            }
            let gen = state >> 1;
            if gen != ch.gen {
                let sound = CHANNEL_SOUND[i].load(Ordering::Acquire);
                if CHANNEL_STATE[i].load(Ordering::Acquire) != state {
                    // A start landed inside this read, so the pointer belongs
                    // to a generation this one does not name. Take the record
                    // whole next period instead — a sound started twice from
                    // the beginning is 2.9 ms of its onset played twice.
                    continue;
                }
                // SAFETY: `toyos_start_sound` publishes the pointer before the
                // generation that names it, and the re-read above establishes
                // that this pointer is the one that generation published. A
                // `CachedSound` is leaked at cache time and never freed.
                ch.sound = Some(unsafe { &*sound });
                ch.pos = 0;
                ch.gen = gen;
            }
            let volume = CHANNEL_VOLUME[i].load(Ordering::Relaxed);
            ch.vol_left = (volume & 0xffff) as i32;
            ch.vol_right = (volume >> 16) as i32;
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
    boundary("I_InitSound", false, || {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        SND_USE_SFX_PREFIX.store(use_sfx_prefix, Ordering::Relaxed);

        let mut mixer = Mixer::new();

        let host = cpal::default_host();
        let Some(device) = host.default_output_device() else {
            eprintln!("[doom-sound] no audio output device; playing silent");
            return false;
        };
        let config = match device.default_output_config() {
            Ok(config) => config,
            Err(e) => {
                eprintln!("[doom-sound] no audio config: {e}; playing silent");
                return false;
            }
        };
        let stream = match device.build_output_stream(
            config.into(),
            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                mixer.sync_channels();
                mixer.fill(data);
                MIXED_PERIODS.fetch_add(1, Ordering::Relaxed);
            },
            |err| {
                eprintln!("[doom-sound] audio stream error: {err}");
                // The audio thread exits after this callback, so nothing will
                // clear a channel's playing bit again. Without this the engine
                // holds every channel it ever started and stops asking for new
                // ones; with it, `sound_is_playing` answers false and the
                // channels come back.
                SND_INITIALIZED.store(false, Ordering::Relaxed);
            },
            None,
        ) {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("[doom-sound] failed to build audio stream: {e}; playing silent");
                return false;
            }
        };
        if let Err(e) = stream.play() {
            eprintln!("[doom-sound] failed to start audio stream: {e}; playing silent");
            return false;
        }
        *AUDIO_STREAM.lock().unwrap() = Some(stream);

        SND_INITIALIZED.store(true, Ordering::Relaxed);
        true
    })
}

unsafe extern "C" fn toyos_shutdown_sound() {
    boundary("I_ShutdownSound", (), || {
        SND_INITIALIZED.store(false, Ordering::Relaxed);
        drop(AUDIO_STREAM.lock().unwrap().take());
    })
}

unsafe extern "C" fn toyos_get_sfx_lump_num(sfx: *mut SfxInfo) -> i32 {
    boundary("I_GetSfxLumpNum", -1, || {
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
    })
}

unsafe extern "C" fn toyos_update_sound() {}

unsafe extern "C" fn toyos_update_sound_params(handle: i32, vol: i32, sep: i32) {
    boundary("I_UpdateSoundParams", (), || {
        if !SND_INITIALIZED.load(Ordering::Relaxed)
            || handle < 0
            || handle >= NUM_SFX_CHANNELS as i32
        {
            return;
        }
        CHANNEL_VOLUME[handle as usize].store(pack_volume(vol, sep), Ordering::Relaxed);
    })
}

unsafe extern "C" fn toyos_start_sound(
    sfxinfo: *mut SfxInfo,
    channel: i32,
    vol: i32,
    sep: i32,
) -> i32 {
    boundary("I_StartSound", -1, || {
        if !SND_INITIALIZED.load(Ordering::Relaxed)
            || channel < 0
            || channel >= NUM_SFX_CHANNELS as i32
        {
            return -1;
        }

        let Some(sound) = cache_sfx(sfxinfo) else {
            return -1;
        };

        let ch = channel as usize;
        CHANNEL_SOUND[ch].store(sound as *const CachedSound as *mut CachedSound, Ordering::Relaxed);
        CHANNEL_VOLUME[ch].store(pack_volume(vol, sep), Ordering::Relaxed);
        // Release: the callback reads the pointer and the volume only after an
        // Acquire load of this word tells it the generation changed.
        let state = &CHANNEL_STATE[ch];
        let gen = (state.load(Ordering::Relaxed) >> 1).wrapping_add(1);
        state.store(gen << 1 | 1, Ordering::Release);

        channel
    })
}

unsafe extern "C" fn toyos_stop_sound(handle: i32) {
    boundary("I_StopSound", (), || {
        if !SND_INITIALIZED.load(Ordering::Relaxed)
            || handle < 0
            || handle >= NUM_SFX_CHANNELS as i32
        {
            return;
        }
        CHANNEL_STATE[handle as usize].fetch_and(!1, Ordering::Relaxed);
    })
}

unsafe extern "C" fn toyos_sound_is_playing(handle: i32) -> bool {
    boundary("I_SoundIsPlaying", false, || {
        if !SND_INITIALIZED.load(Ordering::Relaxed)
            || handle < 0
            || handle >= NUM_SFX_CHANNELS as i32
        {
            return false;
        }
        CHANNEL_STATE[handle as usize].load(Ordering::Relaxed) & 1 != 0
    })
}

// ── Music ──

// A General MIDI SoundFont. The image ships one — `src/soundfont.rs` cuts
// GeneralUser GS down to the instruments this WAD's MUS lumps select — and the
// name is the role rather than that bank, so a build carrying another one plays
// through it unchanged. `toyos_music_init` says which it opened and says so
// again when there is none, because an image without music must not be an image
// that is merely quiet.
const SOUNDFONT_PATH: &str = "/share/soundfont.sf2";

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
    boundary("I_InitMusic", false, || {
        let sf2 = match std::fs::read(SOUNDFONT_PATH) {
            Ok(sf2) => sf2,
            Err(e) => {
                eprintln!("[doom-sound] {SOUNDFONT_PATH}: {e}; playing without music");
                return false;
            }
        };
        let bytes = sf2.len();
        let sf = match SoundFont::new(&mut std::io::Cursor::new(sf2)) {
            Ok(sf) => Arc::new(sf),
            Err(e) => {
                eprintln!("[doom-sound] {SOUNDFONT_PATH}: {e:?}; playing without music");
                return false;
            }
        };
        // Said out loud, because "no line about music" and "music is playing"
        // must not be the same observation: the last SoundFont left the image
        // and nothing anywhere noticed for a cycle.
        println!(
            "[doom-sound] {SOUNDFONT_PATH}: {bytes} bytes, {} presets; music enabled",
            sf.get_presets().len()
        );

        let ring = Arc::new(MusicRing::new());
        MUSIC_RING.set(ring.clone()).unwrap_or_else(|_| panic!("music initialized twice"));

        let (tx, rx) = mpsc::channel();
        *MUSIC_TX.lock().unwrap() = Some(tx);

        let handle = match std::thread::Builder::new()
            .name("midi-synth".into())
            .spawn(move || music_thread(ring, rx, sf))
        {
            Ok(handle) => handle,
            Err(e) => {
                eprintln!("[doom-sound] failed to spawn music thread: {e}; playing without music");
                *MUSIC_TX.lock().unwrap() = None;
                return false;
            }
        };
        *MUSIC_THREAD.lock().unwrap() = Some(handle);

        true
    })
}

unsafe extern "C" fn toyos_music_shutdown() {
    boundary("I_ShutdownMusic", (), || {
        // Dropping the sender disconnects the channel; the thread exits on Disconnected.
        drop(MUSIC_TX.lock().unwrap().take());
        if let Some(handle) = MUSIC_THREAD.lock().unwrap().take() {
            if handle.join().is_err() {
                eprintln!("[doom-sound] music thread panicked");
            }
        }
        if let Some(ring) = MUSIC_RING.get() {
            ring.playing.store(false, Ordering::Relaxed);
        }
    })
}

unsafe extern "C" fn toyos_set_music_volume(volume: i32) {
    boundary("I_SetMusicVolume", (), || {
        // DOOM music volume is 0–15
        let vol = volume.clamp(0, 15) as u32 * 256 / 15;
        if let Some(ring) = MUSIC_RING.get() {
            ring.volume.store(vol, Ordering::Relaxed);
        }
    })
}

unsafe extern "C" fn toyos_pause_music() {
    boundary("I_PauseSong", (), || {
        if let Some(ring) = MUSIC_RING.get() {
            ring.paused.store(true, Ordering::Relaxed);
        }
    })
}

unsafe extern "C" fn toyos_resume_music() {
    boundary("I_ResumeSong", (), || {
        if let Some(ring) = MUSIC_RING.get() {
            ring.paused.store(false, Ordering::Relaxed);
        }
    })
}

unsafe extern "C" fn toyos_register_song(data: *mut c_void, len: i32) -> *mut c_void {
    boundary("I_RegisterSong", core::ptr::null_mut(), || {
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
    })
}

unsafe extern "C" fn toyos_unregister_song(handle: *mut c_void) {
    boundary("I_UnRegisterSong", (), || {
        if !handle.is_null() {
            drop(Box::from_raw(handle as *mut Arc<MidiFile>));
        }
    })
}

unsafe extern "C" fn toyos_play_song(handle: *mut c_void, looping: bool) {
    boundary("I_PlaySong", (), || {
        if handle.is_null() {
            return;
        }
        let midi_file = &*(handle as *const Arc<MidiFile>);
        if let Some(tx) = MUSIC_TX.lock().unwrap().as_ref() {
            if tx.send(MusicCmd::Play(midi_file.clone(), looping)).is_err() {
                eprintln!("[doom-sound] music thread gone; playing without music");
            }
        }
    })
}

unsafe extern "C" fn toyos_stop_song() {
    boundary("I_StopSong", (), || {
        if let Some(tx) = MUSIC_TX.lock().unwrap().as_ref() {
            if tx.send(MusicCmd::Stop).is_err() {
                eprintln!("[doom-sound] music thread gone; playing without music");
            }
        }
    })
}

unsafe extern "C" fn toyos_music_is_playing() -> bool {
    boundary("I_MusicIsPlaying", false, || {
        if let Some(ring) = MUSIC_RING.get() {
            ring.playing.load(Ordering::Relaxed) && !ring.paused.load(Ordering::Relaxed)
        } else {
            false
        }
    })
}

// ── The stalled-consumer actuator ──

const TONE_CHANNEL: i32 = 3;
const PROBE_CHANNEL: i32 = 5;
/// 0.5 s, played at full volume: the only signal this run puts on the wire.
const TONE_FRAMES: usize = OUTPUT_RATE as usize / 2;
/// 0.1 s, played at volume 0. Reaching its end is how the game thread observes
/// that the callback picked a command up, without adding a second signal region
/// to a capture that must contain exactly one.
const PROBE_FRAMES: usize = OUTPUT_RATE as usize / 10;
const TONE_HZ: f64 = 440.0;
/// Loud enough that the capture cannot mistake it for the dither floor, with
/// headroom left so soundd's mix cannot clip.
const TONE_AMPLITUDE: f64 = 16000.0;
/// Sixty-four times the capacity of the ring this replaced, so a tree that
/// still has the ring aborts here rather than merely getting close.
const BURST: u32 = 4096;
const FULL_VOLUME: i32 = 127;
/// The volume every superseded update carries. Far enough below `FULL_VOLUME`
/// that the capture separates them: at `TONE_AMPLITUDE` this mixes to 251 LSB
/// and full volume to 7968, so a capture in which any update but the last one
/// won has no signal in it at all.
const QUIET_VOLUME: i32 = 4;
/// Centred: `pack_volume` gives both channels `vol` at this separation.
const CENTRE: i32 = 127;

fn sine(frames: usize, amplitude: f64) -> &'static CachedSound {
    let samples = (0..frames)
        .map(|i| {
            let phase = 2.0 * std::f64::consts::PI * TONE_HZ * i as f64 / OUTPUT_RATE as f64;
            (amplitude * phase.sin()) as i16
        })
        .collect();
    Box::leak(Box::new(CachedSound { samples }))
}

/// A synthetic sound with no WAD behind it: `cache_sfx` hands `driver_data`
/// straight back when it is already set.
fn synthetic_sfx(sound: &'static CachedSound) -> SfxInfo {
    SfxInfo {
        tagname: core::ptr::null_mut(),
        name: [0; 9],
        priority: 0,
        link: core::ptr::null_mut(),
        pitch: 0,
        volume: 0,
        usefulness: 0,
        lumpnum: -1,
        numchannels: 0,
        driver_data: sound as *const CachedSound as *mut c_void,
    }
}

/// Blocks until `channel` stops playing, and answers how many periods the
/// callback mixed in the meantime.
///
/// `Err` is a callback that never picked the sound up — the failure a lost or
/// dropped `start_sound` produces, and the reason the wait is bounded by its
/// own iteration count as well as by periods: a callback that died stops the
/// period clock too.
fn periods_until_silent(channel: i32) -> Result<u32, String> {
    let start = MIXED_PERIODS.load(Ordering::Relaxed);
    for _ in 0..10_000 {
        if !unsafe { toyos_sound_is_playing(channel) } {
            return Ok(MIXED_PERIODS.load(Ordering::Relaxed).wrapping_sub(start));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    Err(format!(
        "channel {channel} never finished: {} periods mixed while it was playing",
        MIXED_PERIODS.load(Ordering::Relaxed).wrapping_sub(start)
    ))
}

fn churn(sfx: &mut SfxInfo, count: u32, vol: i32) {
    for i in 0..count {
        let channel = (i % NUM_SFX_CHANNELS as u32) as i32;
        match i % 3 {
            0 => {
                unsafe { toyos_start_sound(sfx, channel, vol, (i % 255) as i32) };
            }
            1 => unsafe { toyos_update_sound_params(channel, vol, (i % 255) as i32) },
            _ => unsafe { toyos_stop_sound(channel) },
        }
    }
}

/// Floods the sound module while the audio callback is provably not draining,
/// then requires the callback to converge on the last thing the game said.
///
/// The producer is the game thread inside `S_UpdateSounds`, which nothing
/// outside this process can drive — hence an actuator in the binary that owns
/// it. What killed the game on the T14 was not a rate but a callback that
/// stopped consuming, and `cpal::Stream::pause` stages that exactly: the audio
/// thread parks on its futex, so the burst below is asserted against the
/// callback's own period counter standing still. A wall-clock burst would
/// instead be satisfied by any host that stopped both sides at once.
pub fn sound_stress() -> i32 {
    use cpal::traits::StreamTrait;

    if !unsafe { toyos_init_sound(false) } {
        eprintln!("[sound-stress] no audio output; there is no consumer to outrun");
        return 1;
    }

    let mut tone = synthetic_sfx(sine(TONE_FRAMES, TONE_AMPLITUDE));
    let mut probe = synthetic_sfx(sine(PROBE_FRAMES, TONE_AMPLITUDE));

    let running = MIXED_PERIODS.load(Ordering::Relaxed) + 4;
    for _ in 0..5_000 {
        if MIXED_PERIODS.load(Ordering::Relaxed) >= running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if MIXED_PERIODS.load(Ordering::Relaxed) < running {
        eprintln!("[sound-stress] the audio callback never started mixing");
        return 1;
    }

    // The stall, staged: the audio thread parks and drains nothing until it is
    // played again.
    {
        let stream = AUDIO_STREAM.lock().unwrap();
        if let Err(e) = stream.as_ref().expect("stream built").pause() {
            eprintln!("[sound-stress] could not pause the audio callback: {e}");
            return 1;
        }
    }

    // Retried because the callback may still have had a period in flight when
    // it was told to pause; the burst only means something if it is entirely
    // inside a stretch the callback did not advance through.
    let mut stalled_burst = 0;
    for _ in 0..8 {
        let before = MIXED_PERIODS.load(Ordering::Relaxed);
        churn(&mut probe, BURST, FULL_VOLUME);
        if MIXED_PERIODS.load(Ordering::Relaxed) == before {
            stalled_burst = BURST;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if stalled_burst == 0 {
        eprintln!("[sound-stress] the callback kept mixing through a paused stream");
        return 1;
    }

    // The state every one of those calls was superseded by, still with nothing
    // consuming: one sound on one channel, at the volume named by the last of
    // another `BURST` of updates. The capture is the verdict on that last one —
    // every update before it is inaudible by construction.
    for channel in 0..NUM_SFX_CHANNELS as i32 {
        unsafe { toyos_stop_sound(channel) };
    }
    unsafe { toyos_start_sound(&mut tone, TONE_CHANNEL, QUIET_VOLUME, CENTRE) };
    for _ in 0..BURST {
        unsafe { toyos_update_sound_params(TONE_CHANNEL, QUIET_VOLUME, CENTRE) };
    }
    unsafe { toyos_update_sound_params(TONE_CHANNEL, FULL_VOLUME, CENTRE) };

    {
        let stream = AUDIO_STREAM.lock().unwrap();
        if let Err(e) = stream.as_ref().expect("stream built").play() {
            eprintln!("[sound-stress] could not resume the audio callback: {e}");
            return 1;
        }
    }

    let tone_periods = match periods_until_silent(TONE_CHANNEL) {
        Ok(periods) => periods,
        Err(e) => {
            eprintln!("[sound-stress] {e}");
            return 1;
        }
    };

    // The same flood against a callback that is running — the concurrent case,
    // where a record is overwritten while the callback is reading it. Silent:
    // every call is at volume 0, so nothing here reaches the capture and the
    // tone above stays the only signal in it.
    let concurrent_start = MIXED_PERIODS.load(Ordering::Relaxed);
    let mut concurrent = 0u32;
    while MIXED_PERIODS.load(Ordering::Relaxed).wrapping_sub(concurrent_start) < 32 {
        churn(&mut probe, 256, 0);
        concurrent += 256;
    }
    for channel in 0..NUM_SFX_CHANNELS as i32 {
        unsafe { toyos_stop_sound(channel) };
    }
    unsafe { toyos_start_sound(&mut probe, PROBE_CHANNEL, 0, CENTRE) };
    let probe_periods = match periods_until_silent(PROBE_CHANNEL) {
        Ok(periods) => periods,
        Err(e) => {
            eprintln!("[sound-stress] {e}");
            return 1;
        }
    };

    unsafe { toyos_shutdown_sound() };

    println!(
        "[sound-stress] stalled_burst={stalled_burst} tone_periods={tone_periods} \
         tone_frames={TONE_FRAMES} concurrent_cmds={concurrent} probe_periods={probe_periods} \
         probe_frames={PROBE_FRAMES}"
    );
    0
}

// ── The music actuator ──

/// The track `--music-check` plays. E1M1's, and it is the right one for two
/// reasons: it is the first thing a person hears, and its instrument list
/// exercises melodic programs and the drum kit together, so a bank subset that
/// lost either fails here.
const MUSIC_CHECK_LUMP: &[u8; 6] = b"D_E1M1";

/// Where the WAD is in the image. `main.rs` passes the same path to doomgeneric.
const WAD_PATH: &str = "/share/doom1.wad";

/// How much of the track to put on the wire. Long enough that the host's
/// capture has a stretch of music in it and not just an onset, short enough
/// that the whole check is a few seconds — a period is 128 frames.
const MUSIC_CHECK_PERIODS: u32 = 3 * OUTPUT_RATE / 128;

/// The lump named `want` in a WAD, if it has one.
///
/// A whole WAD reader is `w_wad.c`'s job and it is in this binary — but it is
/// only reachable after `doomgeneric_Create`, which needs a window, an event
/// loop and a compositor. Fifteen lines of directory walk here is what makes
/// the music path answerable without any of that.
fn wad_lump<'a>(wad: &'a [u8], want: &[u8; 6]) -> Option<&'a [u8]> {
    fn u32le(wad: &[u8], at: usize) -> Option<usize> {
        let bytes: [u8; 4] = wad.get(at..at + 4)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes) as usize)
    }
    if wad.len() < 12 || (&wad[0..4] != b"IWAD" && &wad[0..4] != b"PWAD") {
        return None;
    }
    let count = u32le(wad, 4)?;
    let directory = u32le(wad, 8)?;
    (0..count).find_map(|i| {
        let entry = directory.checked_add(i.checked_mul(16)?)?;
        let name = wad.get(entry + 8..entry + 16)?;
        if &name[..want.len()] != want || name[want.len()] != 0 {
            return None;
        }
        let start = u32le(wad, entry)?;
        let len = u32le(wad, entry + 4)?;
        wad.get(start..start.checked_add(len)?)
    })
}

/// Plays one of doom's own MUS lumps through the shipped SoundFont and the
/// real audio output, then says what happened.
///
/// **This is the wiring, and the wiring is the only part the soundfont
/// investigation could not measure.** That all 13 tracks render bit-exact
/// against the full bank was established on the host, through this same
/// `mus2mid.c` and this same rustysynth; what no host render can answer is
/// whether the file reaches the image, whether doom finds it, and whether the
/// result gets as far as the device. Every one of those is a fact about a guest.
///
/// Driven by `tests/toyos-rust-tests/src/bin/doom_music.rs`; the verdict on the
/// sound itself is the host's capture, not anything printed here.
pub fn music_check() -> i32 {
    let wad = match std::fs::read(WAD_PATH) {
        Ok(wad) => wad,
        Err(e) => {
            eprintln!("[music-check] {WAD_PATH}: {e}");
            return 1;
        }
    };
    let Some(lump) = wad_lump(&wad, MUSIC_CHECK_LUMP) else {
        eprintln!("[music-check] {WAD_PATH} has no {} lump", String::from_utf8_lossy(MUSIC_CHECK_LUMP));
        return 1;
    };

    if !unsafe { toyos_init_sound(false) } {
        eprintln!("[music-check] no audio output; nothing can carry the music");
        return 1;
    }
    if !unsafe { toyos_music_init() } {
        eprintln!("[music-check] the music module did not come up");
        return 1;
    }

    // `mus2mid` writes into a `memio` stream and `memio` allocates out of the
    // zone, which `D_DoomMain` sets up on the path this one bypasses.
    unsafe { Z_Init() };

    let song = unsafe { toyos_register_song(lump.as_ptr() as *mut c_void, lump.len() as i32) };
    if song.is_null() {
        eprintln!("[music-check] {} did not convert to a MIDI file", String::from_utf8_lossy(MUSIC_CHECK_LUMP));
        return 1;
    }
    unsafe { toyos_play_song(song, true) };

    // Bounded by the callback's own period count rather than by wall clock: a
    // host that stopped this guest for a second must not shorten the music the
    // capture is judged on. The iteration cap is the liveness half — a callback
    // that died stops the period clock too.
    let start = MIXED_PERIODS.load(Ordering::Relaxed);
    let mut periods = 0;
    for _ in 0..30_000 {
        periods = MIXED_PERIODS.load(Ordering::Relaxed).wrapping_sub(start);
        if periods >= MUSIC_CHECK_PERIODS {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let rendered = MUSIC_RING.get().map_or(0, |ring| ring.write.load(Ordering::Relaxed));
    let playing = unsafe { toyos_music_is_playing() };

    unsafe { toyos_stop_song() };
    unsafe { toyos_music_shutdown() };
    unsafe { toyos_shutdown_sound() };

    if periods < MUSIC_CHECK_PERIODS {
        eprintln!("[music-check] the audio callback mixed {periods} periods of {MUSIC_CHECK_PERIODS}");
        return 1;
    }
    if !playing {
        eprintln!("[music-check] the music stopped before the check did");
        return 1;
    }

    println!(
        "[music-check] lump={} midi_bytes={} periods={periods} rendered_frames={rendered}",
        String::from_utf8_lossy(MUSIC_CHECK_LUMP),
        lump.len(),
    );
    0
}
