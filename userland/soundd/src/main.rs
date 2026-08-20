use toyos_abi::audio::{AudioCompletionRecord, AudioSlotHeader};
use toyos_abi::RawHandle;
use toyos::audio::{
    AudioSlotReader, StreamOpenRequest, StreamOpenResponse, StreamSetVolume, FORMAT_S16LE,
    MSG_STREAM_OPEN, MSG_STREAM_OPENED, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE, MSG_STREAM_ERROR,
};
use toyos::endow::{self, Endowments};
use toyos::ipc::{self, RxStep};
use toyos::poller::{Poller, READABLE};
use toyos::port::Acceptor;
use toyos::shm::SharedMemory;
use toyos::syscap::SysCap;
use toyos::{AsHandle, Connection, HdaDev, VirtioSoundDev};
use toyos_abi::syscall::{self, DeviceType};
use toyos_hda::stream;
use toyos_mixer::{
    accumulate, append_planar, client_period_frames, decode_i16_to_f32, deferral_floor_nanos,
    interleave, mix_interleaved, period_frames, period_nanos, quantize_period, ramp_frames,
    scratch_frames, Dll, Gain, GainRamp, MixStats, Xorshift32, MAX_CLIENT_RATE,
    MIN_CLIENT_RATE,
};

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// One line, one `write`.
///
/// **`eprintln!` is not one write and `println!` is not either.** Stderr is
/// unbuffered by design, so `write_fmt` issues a syscall per format fragment;
/// stdout's `LineWriter` makes it two, one flushing what it had buffered and
/// one for the rest. Every gap between two of those is somewhere the kernel's
/// own log can land, because on this machine the console and the log ring are
/// one stream — and `soundd: client ` came back with four `exit:` accounting
/// lines inside it and `1 removed` under them, on CI run `31271983043`. The
/// collision is systematic and not unlucky: this daemon prints a client's
/// removal exactly when the kernel is printing that client's exit.
///
/// **Fixed for everyone at the kernel now**: a `ConsoleObject` per holder
/// buffers a line and emits it whole under one `BackendGuard`, so this macro is
/// about the *count* of syscalls now rather than about atomicity.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let mut line = format!($($arg)*);
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }};
}

mod hda;
mod virtio;

/// Who owns a period soundd has been given back and has not refilled.
///
/// The mix loop's free list was written for [`Pipeline::Queue`] throughout, and
/// three of its rules are that model showing: it holds a period back while a
/// client is mid-refill, it drains by not submitting, and it takes the lowest
/// free index first because the device plays what it is given in the order it
/// is given.
///
/// [`Pipeline::Ring`] breaks all three, which is what killed soundd on the T14.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pipeline {
    /// virtio-sound: a period soundd has not submitted is a period the device
    /// does not have. Holding one costs nothing, indefinitely, and the play
    /// order is the submit order.
    Queue,
    /// HDA: the engine owns every period for as long as it runs. It returns to
    /// buffer `i` exactly `num_buffers` periods after completing it and plays
    /// whatever is there, so a period soundd holds back is played as the
    /// silence `released` left in it *and* completed a second time — a
    /// completion for a buffer soundd still holds. The play order is the
    /// ring's, which is the lowest free index only while a batch does not wrap.
    Ring,
}

/// The device half of the mix loop.
///
/// Two implementations and no more: a framework before there are three is an
/// abstraction with no evidence behind it. Both are drivers in this process
/// now, and what differs between the two devices is exactly the methods below;
/// the mixer, the ramps, the DLL, the underrun accounting and the
/// suspend/resume structure are one body of code either way, which is what
/// makes gate A one instrument for both.
trait Backend {
    /// Who owns a freed period soundd does not refill.
    fn pipeline(&self) -> Pipeline;

    /// The handle a completion arrives on.
    fn handle(&self) -> RawHandle;

    /// Where period `idx`'s samples go. Device memory this process may write,
    /// mapped once at claim.
    fn buffer(&self, idx: usize) -> *mut u8;

    /// Completion records, oldest first, into `out`. `0` is nothing pending.
    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize;

    /// Period `idx` has played and is soundd's again.
    ///
    /// HDA's engine is cyclic and never stops on its own, so a period nobody
    /// refills is *replayed* — audible harm gate A's gap detector cannot see.
    /// Zeroing it as it frees makes a late soundd cost silence instead, which
    /// is exactly what virtio-sound's device does when it runs dry, so one
    /// instrument certifies both (§2.4). virtio's own implementation is empty
    /// for that reason and not by omission: nothing is published for a period
    /// that was not filled, so the device is never given it again.
    fn released(&mut self, idx: usize);

    /// Period `idx` holds `bytes` of PCM and is the device's to play.
    ///
    /// On a [`Pipeline::Ring`] that is a statement about the *contents*: the
    /// period was already the device's and this only says it now holds audio.
    fn submit(&mut self, idx: usize, bytes: usize);

    /// Stop the stream. Idempotent, and cheap when it is already stopped.
    fn stop(&mut self);
}

struct VirtioBackend {
    virtio: virtio::Virtio,
}

impl Backend for VirtioBackend {
    fn pipeline(&self) -> Pipeline {
        Pipeline::Queue
    }

    fn handle(&self) -> RawHandle {
        self.virtio.dev().as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.virtio.buffer(idx)
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        // Where the kernel used to service the event queue inside the same
        // syscall: the device's own view of an underrun, which this process's
        // counters cannot see.
        self.virtio.poll_events();
        self.virtio.completions(out)
    }

    fn released(&mut self, _idx: usize) {}

    fn submit(&mut self, idx: usize, bytes: usize) {
        self.virtio.submit(idx, bytes);
    }

    fn stop(&mut self) {
        self.virtio.stop();
    }
}

struct HdaBackend {
    hda: hda::Hda,
    buffers: Vec<*mut u8>,
    period_bytes: usize,
}

impl Backend for HdaBackend {
    fn pipeline(&self) -> Pipeline {
        Pipeline::Ring
    }

    fn handle(&self) -> RawHandle {
        self.hda.dev().as_handle()
    }

    fn buffer(&self, idx: usize) -> *mut u8 {
        self.buffers[idx]
    }

    fn completions(&mut self, out: &mut [AudioCompletionRecord]) -> usize {
        match self.hda.dev().completions() {
            Ok(record) => {
                out[0] = record;
                1
            }
            Err(syscall::SyscallError::WouldBlock) => 0,
            Err(e) => panic!("soundd: hda completions failed: {e:?}"),
        }
    }

    fn released(&mut self, idx: usize) {
        unsafe { core::ptr::write_bytes(self.buffers[idx], 0, self.period_bytes) };
    }

    fn submit(&mut self, _idx: usize, _bytes: usize) {
        self.hda.start();
    }

    fn stop(&mut self) {
        self.hda.stop();
    }
}

use rubato::{Resampler, SincFixedOut, SincInterpolationParameters, SincInterpolationType, WindowFunction};

const STATS_INTERVAL_NANOS: u64 = 2_000_000_000;

struct ClientResampler {
    resampler: SincFixedOut<f32>,
    /// Planar (per-channel) client audio awaiting resampling. SincFixedOut
    /// consumes a varying `input_frames_next()` per call, so slots are pulled
    /// into this buffer on demand instead of fed one fixed chunk per cycle.
    accum: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

/// How a stream ended, as far as soundd can honestly tell.
///
/// Two things witness a client leaving and they race: the control thread reads
/// the peer, and the mix loop finds the signal pipe gone on its next write.
/// Both start the same ramp, so no audio differs — but only the first of them
/// *knows* anything, and soundd used to report the second as a death. A clean
/// exit and a crash tear down the same descriptors the same way; the kernel's
/// `exit:` line carries the code and nothing on this side can tell them apart,
/// so `died` was a false positive at 11% of ordinary disconnects (5 of 44
/// runs). Each variant below is something soundd observed rather than inferred,
/// and the cause is left to the log that has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Departure {
    /// `MSG_STREAM_CLOSE`: the client said so itself, which is the one reason
    /// nothing can improve on.
    Closed,
    /// soundd ended it — a protocol violation, or a volume that is not a
    /// number. The only departure soundd itself caused.
    Refused,
    /// The control connection ended without a close. The client's process is
    /// gone; whether it exited or crashed is not knowable here.
    Disconnected,
    /// The signal pipe broke, which says the client's descriptor table is gone
    /// and nothing about why. The weakest of the four, and the only one the
    /// others replace.
    SignalPipeGone,
}

impl Departure {
    /// The stronger of two witnesses, so the two may arrive in either order and
    /// land on the same word.
    ///
    /// What the control thread read beats what the mix loop found broken —
    /// it read the peer, where the mix loop only found a descriptor missing —
    /// and nothing beats the client's own close. Idempotent, and a witness
    /// never weakens what is already known.
    fn refine(self, other: Departure) -> Departure {
        if other.rank() < self.rank() { other } else { self }
    }

    /// How much this witness knows. Lower is stronger.
    fn rank(self) -> u8 {
        match self {
            Departure::Closed => 0,
            Departure::Refused => 1,
            Departure::Disconnected => 2,
            Departure::SignalPipeGone => 3,
        }
    }
}

impl core::fmt::Display for Departure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Departure::Closed => "closed",
            Departure::Refused => "refused",
            Departure::Disconnected => "disconnected",
            Departure::SignalPipeGone => "signal pipe gone",
        })
    }
}

struct ClientStream {
    client_id: usize,
    slot_reader: AudioSlotReader,
    /// The write end of the signal pipe. soundd makes both ends and sends the
    /// read end to the client, so §5.7's crash detection is by construction:
    /// the moment the client's table goes, the read end goes with it and the
    /// next signal breaks.
    signal_write: RawHandle,
    gain: GainRamp,
    client_channels: u16,
    client_period_frames: u32,
    resampler: Option<ClientResampler>,
    /// Latched by the first period this client supplies.
    delivered: bool,
    /// How this stream ended, once anything has witnessed it. `None` while it
    /// is live: the ramp-out starts when this is first set, and the stream is
    /// dropped when that ramp reaches idle.
    departure: Option<Departure>,
}

impl ClientStream {
    /// The window in which a period this client failed to cover is starvation
    /// rather than protocol: from the first period it delivered until it asks
    /// to close.
    ///
    /// Outside it, silence is the design working. `MSG_STREAM_OPEN` arrives
    /// before the client has any audio — it still has to spawn its callback
    /// thread — and after a close §5.5's ramp is deliberately fading it out,
    /// so it is entitled to stop filling.
    fn is_streaming(&self) -> bool {
        self.delivered && self.departure.is_none()
    }

    /// Record a departure, and start §7.4's ramp-out the first time one is
    /// known.
    ///
    /// A later witness refines the word and leaves the ramp alone: it is
    /// already aimed at silence, and re-targeting it recomputes the step from
    /// the gain reached so far, which stretches a 5 ms fade by however much of
    /// it had already run.
    fn depart(&mut self, how: Departure, ramp_frames: u32) {
        match self.departure {
            None => {
                self.gain.set_target(Gain::SILENT, ramp_frames);
                self.departure = Some(how);
            }
            Some(known) => self.departure = Some(known.refine(how)),
        }
    }
}

/// Control connections soundd will hold at once.
///
/// The control thread watches one handle per client plus the acceptor in a
/// single poller and io_uring rings are powers of two, so the limit is a ring
/// size minus one. 64 costs the same 2 MiB page as 32; 63 simultaneous streams
/// is already past what the mixer renders inside one 2.9 ms period, and costs
/// 189 of the kernel's 4096 handle slots (a control connection plus both signal
/// pipe ends per client).
const MAX_CONTROL_CLIENTS: usize = 63;

/// Deep enough that one pass of the control loop can never fill it: a pass
/// pushes at most one `AddClient` (there is one accept per wait) plus, per
/// connected client, one coalesced `SetVolume` and one `RemoveClient`.
const CMD_RING_SIZE: u32 = 256;
const _: () = assert!(CMD_RING_SIZE as usize >= 1 + 2 * MAX_CONTROL_CLIENTS);

enum MixCommand {
    AddClient(Box<ClientStream>),
    RemoveClient { client_id: usize, departure: Departure },
    SetVolume { client_id: usize, target: Gain },
}

struct CommandRing {
    slots: std::cell::UnsafeCell<[Option<MixCommand>; CMD_RING_SIZE as usize]>,
    write_idx: AtomicU32,
    read_idx: AtomicU32,
}

unsafe impl Send for CommandRing {}
unsafe impl Sync for CommandRing {}

impl CommandRing {
    fn new() -> Self {
        Self {
            slots: std::cell::UnsafeCell::new(std::array::from_fn(|_| None)),
            write_idx: AtomicU32::new(0),
            read_idx: AtomicU32::new(0),
        }
    }

    /// Hands the command back when the ring is full rather than dropping it (a
    /// ghost client: leaked shm, an app waiting on a stream nothing mixes) or
    /// asserting — a client chooses the load, since the control thread drains
    /// everything it has written before yielding. See `submit`, which waits.
    #[must_use]
    fn try_push(&self, cmd: MixCommand) -> Result<(), MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= CMD_RING_SIZE {
            return Err(cmd);
        }
        let idx = (w % CMD_RING_SIZE) as usize;
        unsafe { (*self.slots.get())[idx] = Some(cmd); }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    fn pop(&self) -> Option<MixCommand> {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w == r { return None; }
        let idx = (r % CMD_RING_SIZE) as usize;
        let cmd = unsafe { (*self.slots.get())[idx].take() };
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        cmd
    }
}

/// One reporting window on the console. One line, one `write`.
///
/// The counters are `toyos_mixer::MixStats`, and what they mean is documented
/// there beside the decision that fills them; this is the emission, which is an
/// effect and stays here. `#106`'s status tool reads one shape, so the null
/// sink prints the same line.
fn report(stats: &MixStats, clients: usize) {
    say!("soundd: wakes={} completions={} submitted={} underruns={} drains={} max_wake_lat_us={} max_batch={} clients={} deferred={} starve_max={}",
        stats.wakes, stats.completions, stats.submitted, stats.underruns, stats.drains,
        stats.max_wake_lat_ns / 1_000, stats.max_batch, clients, stats.deferred,
        stats.starve_max);
}

fn open_stream(
    client_id: usize,
    req: &StreamOpenRequest,
    control: &Connection,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) -> Option<ClientStream> {
    let client_period_frames =
        client_period_frames(device_period_frames, req.sample_rate, device_sample_rate);

    let sample_size: u32 = 2; // FORMAT_S16LE, validated before open_stream
    let client_frame_size = req.channels as u32 * sample_size;
    let client_period_bytes = client_period_frames * client_frame_size;

    let shm_size = AudioSlotHeader::SIZE as u32 + slot_count * client_period_bytes;
    // The ring is this client's and no stream exists without it, so both
    // refusals end the open rather than the daemon: a client that exited
    // between asking and being served cannot be granted memory, and neither
    // can one that asked while the machine had none.
    let shm = match SharedMemory::create(shm_size as usize) {
        Ok(shm) => shm,
        Err(e) => {
            say!("soundd: no {shm_size}-byte ring for client {client_id} ({e:?})");
            return None;
        }
    };
    let client_shm = match shm.share() {
        Ok(h) => h,
        Err(e) => {
            say!("soundd: cannot share the ring with client {client_id} ({e:?})");
            return None;
        }
    };

    unsafe {
        let hdr = &*(shm.as_ptr() as *const AudioSlotHeader);
        hdr.write_idx.store(0, core::sync::atomic::Ordering::Relaxed);
        hdr.read_idx.store(0, core::sync::atomic::Ordering::Relaxed);
    }

    // **soundd makes the pipe and keeps the write end.** That is what makes a
    // dead client detectable without bookkeeping: the read end it is sent goes
    // when its table does, and the next signal answers `Gone`. The client used
    // to make the pipe and name it by an id, because an id was only openable
    // by a peer of its creator and the peer relation ran one way.
    let (signal_read, signal_write) = match toyos::pipe_pair() {
        Ok(ends) => ends,
        Err(e) => {
            syscall::close(client_shm);
            say!("soundd: no signal pipe for client {client_id} ({e:?})");
            return None;
        }
    };

    let slot_reader = AudioSlotReader::new(shm, client_period_bytes, slot_count);

    // Handles first, then the frame that announces them — `send_with_handles`
    // is that order, and a client reading the frame is guaranteed to find
    // them. Both are moved whether or not this succeeds.
    if control.send_with_handles(
        &[client_shm, signal_read.into_raw()],
        MSG_STREAM_OPENED,
        &StreamOpenResponse {
            client_period_frames,
            client_period_bytes,
            device_sample_rate,
            device_channels,
            slot_count: slot_count as u16,
        },
    ).is_err() {
        // Client died mid-open; the dropped control connection removes it.
        say!("soundd: client {client_id} vanished during stream open");
    }

    let resampler = if req.sample_rate != device_sample_rate {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let resample_ratio = device_sample_rate as f64 / req.sample_rate as f64;
        let resampler = SincFixedOut::<f32>::new(
            resample_ratio,
            2.0,
            params,
            device_period_frames as usize,
            device_channels as usize,
        ).expect("failed to create resampler");
        // The pull loop tops accum up to input_frames_next() one slot at a
        // time, so it peaks below input_frames_max + one client period.
        let accum_capacity = resampler.input_frames_max() + client_period_frames as usize;
        let accum = (0..device_channels as usize)
            .map(|_| Vec::with_capacity(accum_capacity))
            .collect();
        let output = resampler.output_buffer_allocate(true);
        Some(ClientResampler { resampler, accum, output })
    } else {
        None
    };

    let mut gain = GainRamp::new(Gain::SILENT);
    gain.set_target(Gain::UNITY, ramp_frames);

    Some(ClientStream {
        client_id,
        slot_reader,
        signal_write: signal_write.into_raw(),
        gain,
        client_channels: req.channels,
        client_period_frames,
        resampler,
        delivered: false,
        departure: None,
    })
}

/// Mix one period of `stream` into the bus. Returns false when the client's
/// ring could not supply a full period (silence mixed instead).
///
/// Slots are consumed peek→copy-out→advance: read_idx is published only after
/// the slot data has been decoded out of shared memory, so a concurrently
/// filling client can never overwrite a slot soundd is still reading.
fn mix_client(
    stream: &mut ClientStream,
    mix_f32: &mut [f32],
    decode_buf: &mut [f32],
    convert_buf: &mut [f32],
    device_channels: usize,
    device_period_frames: usize,
) -> bool {
    let client_frames = stream.client_period_frames as usize;
    let client_channels = stream.client_channels as usize;
    let client_samples = client_frames * client_channels;
    assert!(client_samples <= decode_buf.len());

    if let Some(rs) = stream.resampler.as_mut() {
        // Pull slots until the resampler's varying input requirement is met.
        // Consuming on demand (instead of one slot per cycle) keeps the
        // accumulation bounded: surplus frames from the ceil() slot sizing
        // simply delay the next slot consumption.
        loop {
            let needed = rs.resampler.input_frames_next();
            if rs.accum[0].len() >= needed {
                break;
            }
            let Some(slot) = stream.slot_reader.peek() else {
                stream.gain.advance_frames(device_period_frames as u32);
                return false;
            };
            decode_i16_to_f32(slot.data(), &mut decode_buf[..client_samples]);
            slot.advance();
            append_planar(&decode_buf[..client_samples], client_channels, &mut rs.accum);
        }

        let (consumed, produced) = rs.resampler
            .process_into_buffer(&rs.accum, &mut rs.output, None)
            .expect("resampler process failed");
        assert_eq!(produced, device_period_frames);
        for ch in rs.accum.iter_mut() {
            ch.drain(..consumed);
        }

        let out_samples = produced * device_channels;
        assert!(out_samples <= convert_buf.len());
        interleave(&rs.output, produced, &mut convert_buf[..out_samples]);
        accumulate(mix_f32, &convert_buf[..out_samples], device_channels, &mut stream.gain);
        return true;
    }

    let Some(slot) = stream.slot_reader.peek() else {
        stream.gain.advance_frames(device_period_frames as u32);
        return false;
    };
    decode_i16_to_f32(slot.data(), &mut decode_buf[..client_samples]);
    slot.advance();

    mix_interleaved(
        mix_f32,
        &decode_buf[..client_samples],
        convert_buf,
        client_channels,
        device_channels,
        &mut stream.gain,
    );
    true
}

/// Signal every client before the wait so priority inheritance can fill their
/// rings while soundd blocks, and reap the ones that died doing it.
///
/// §5.7/§7.3: a broken pipe here is the client's departure, caught here rather
/// than left to the control connection — a client that goes mid-stream would
/// otherwise stay `is_streaming()` and keep the loop deferring buffers for a
/// producer that no longer exists. Departure is exactly `Err(NotFound)`, the
/// kernel's broken-pipe error; a full pipe is `Err(WouldBlock)` and means the
/// client is merely behind on consuming signals, which must leave it untouched
/// — a §6.4-paused client stops reading its pipe indefinitely and is alive.
///
/// It says nothing on its own: [`Departure::SignalPipeGone`] is the weakest of
/// the four witnesses and the control thread's is on the way, so the word waits
/// for `retain_active` and the strongest witness by then wins.
fn signal_clients(streams: &mut [ClientStream], ramp_frames: u32) {
    for stream in streams.iter_mut() {
        let gone = matches!(
            syscall::write_nonblock(stream.signal_write, &[1]),
            Err(syscall::SyscallError::NotFound)
        );
        if gone {
            stream.depart(Departure::SignalPipeGone, ramp_frames);
        }
    }
}

/// Drain the command ring the control thread fills: connects, disconnects, and
/// volume changes. Shared by both sinks — a client's lifecycle is the same
/// whether its audio reaches hardware or a discard.
fn apply_commands(cmd_ring: &CommandRing, streams: &mut Vec<ClientStream>, ramp_frames: u32) {
    while let Some(cmd) = cmd_ring.pop() {
        match cmd {
            MixCommand::AddClient(client) => {
                say!("soundd: client {} connected (id={})", streams.len(), client.client_id);
                let _ = syscall::write_nonblock(client.signal_write, &[1]);
                streams.push(*client);
            }
            MixCommand::RemoveClient { client_id, departure } => {
                if let Some(s) = streams.iter_mut().find(|s| s.client_id == client_id) {
                    s.depart(departure, ramp_frames);
                }
            }
            MixCommand::SetVolume { client_id, target } => {
                if let Some(s) = streams.iter_mut().find(|s| s.client_id == client_id) {
                    s.gain.set_target(target, ramp_frames);
                }
            }
        }
    }
}

/// Drop clients whose disconnect ramp has finished. Paused clients (§6.4) mix
/// silence and are never removed here; a disconnecting client leaves only after
/// its §5.5 ramp-out reaches idle, so its tail plays out first.
///
/// This is where a departure is finally worded, and the last moment at which it
/// can be: both witnesses have had the whole ramp to arrive, and the removal is
/// the one line per stream that names how it ended.
fn retain_active(streams: &mut Vec<ClientStream>) {
    streams.retain(|s| match s.departure {
        Some(how) if s.gain.is_idle() => {
            say!("soundd: client {} removed ({how})", s.client_id);
            syscall::close(s.signal_write);
            false
        }
        _ => true,
    });
}

fn mix_thread(
    backend: &mut dyn Backend,
    cmd_ring: &CommandRing,
    cmd_pipe_read: RawHandle,
    num_buffers: usize,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_bytes: usize,
    device_period_frames: usize,
    ramp_frames: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = period_nanos(device_period_frames as u64, device_sample_rate as u64);
    let pipeline = backend.pipeline();
    // The device plays one period per `period_nanos`, so the wall-clock cost of
    // emptying the pipeline is bounded from below. Every buffer is in flight
    // the moment the mix loop finishes submitting, and the head one is only
    // part-played, so more than `(num_buffers - 1)` periods of audio are still
    // unplayed at that instant. See the drain count site.
    let min_drain_nanos = (num_buffers as u64 - 1) * period_nanos;
    let refill_floor_nanos = deferral_floor_nanos(num_buffers, period_nanos);

    let mut streams: Vec<ClientStream> = Vec::new();
    // Boot starts SUSPENDED (§5.8): every buffer free, nothing submitted, the
    // PCM stream never started. There is no unconditional silence prime — the
    // first client's ordinary refill fills the whole pipeline through the
    // dithering mix path, and the kernel starts the stream on that submit.
    let mut free_mask: u32 = (1u32 << num_buffers) - 1;
    // Periods soundd has filled that the device has not finished playing.
    //
    // `num_buffers - free_mask.count_ones()` on a [`Pipeline::Queue`], which is
    // what the free list used to be read as at the two sites below. It is not
    // that on a [`Pipeline::Ring`]: there the free list is empty at every wake,
    // because a period the engine hands back is given up the same cycle
    // whatever soundd has to put in it, so it cannot say when the pipeline has
    // played out. This can, and says the same thing on both.
    let mut unplayed: usize = 0;
    // The period a [`Pipeline::Ring`]'s engine is playing now, read off every
    // completion mask by `stream::decode`.
    //
    // The kernel's `stream::completed` walks the ring forward from where the
    // engine was and hands back a *set*; the engine plays a *sequence*. This is
    // the driver's half of that, and it is what the fill order has to come from:
    // a batch that wraps (`{6, 7, 0, 1}`) is played 6, 7, 0, 1, and filling it
    // lowest-index-first writes the later audio into the buffer the engine
    // reaches soonest. It is kept between wakes for the two moments no mask can
    // place the engine: a whole lap, and a stop — which freezes it inside the
    // period after the last one it completed, and is where the next stream
    // primes from.
    let mut ring_cursor: usize = 0;
    // Whether the device stream is running, i.e. soundd has submitted since
    // the last stop. Owned here: the kernel's own started flag is not
    // readable, and the two agree because every submit starts a stopped
    // stream and only the suspend block below stops it.
    //
    // Establish that agreement instead of assuming it. `false` is a claim
    // about kernel state, and it is only true if no soundd ran before this
    // one: the audio claim is released on descriptor close, so a soundd that
    // died inside the drain window — last completion drained, STOP not yet
    // issued — leaves the stream STARTED with an empty queue, and a successor
    // that merely believed it stopped would park forever with the host voice
    // open in permanent underrun, at exactly zero CPU. One STOP makes the
    // belief true. It costs nothing on an ordinary boot: a backend's own `stop`
    // returns without a control round trip or a log line when the stream is
    // already stopped.
    backend.stop();
    let mut started = false;
    // Wall clock at the last instant the pipeline was known full. Re-stamped
    // after every refill; read only by the drain count site.
    let mut pipeline_filled_ns = syscall::clock_nanos();
    // Wall clock at which everything submitted will have finished playing. The
    // device plays one period per `period_nanos` and cannot play faster, so
    // this is the only honest measure of how much audio is still on the wire.
    // The free list is not: QEMU retires a whole pipeline in a few ms, so
    // "free" says nothing about what has been heard. Nothing is on the wire
    // at boot.
    let mut playout_until_ns = pipeline_filled_ns;

    // **The band is a privilege now, not a side effect of holding a card.**
    // Until this branch it was gated on the audio claim, which the dispatch's
    // own comment called out as not a privilege at all: whoever won the
    // first-come race for the sound card got the RT band with it. This is the
    // `RT`-only capability the manifest's `syscap = ["rt"]` row asks init for,
    // and soundd is the only program in the tree that has one. Mixing on
    // without the band would show up only as glitches, so a refusal is loud.
    let rt: SysCap = Endowments::get()
        .take(toyos_abi::syscall::SYSCAP_LABEL)
        .expect("the manifest declares this program `syscap = [\"rt\"]`");
    rt.enter_rt().expect("an RT capability refused the band it names");

    let poller = Poller::new(64);
    let mut mix_f32 = vec![0.0f32; device_period_samples];
    // Sized for the highest client rate accepted at stream open, so the mix
    // path never allocates.
    let max_client_frames = scratch_frames(device_period_frames, device_sample_rate as usize);
    let mut decode_buf = vec![0.0f32; max_client_frames * 2];
    let mut convert_buf = vec![0.0f32; max_client_frames * 2];
    let mut dither_rng = Xorshift32::new(syscall::clock_nanos() as u32);
    let mut dll = Dll::new(period_nanos as f64);
    let mut records = [AudioCompletionRecord { mask: 0, _pad: 0, timestamp_nanos: 0 }; 16];

    const TOKEN_AUDIO: u64 = u64::MAX - 1;
    const TOKEN_CMD: u64 = u64::MAX - 2;

    // Buffers the previous cycle deliberately left unfilled. Read by the drain
    // site, which must not mistake soundd's own restraint for a device stall.
    let mut deferred_last: u32 = 0;
    let mut stats = MixStats::default();
    let mut next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;

    // Exactly one emission of one of these markers is gate-asserted: the
    // `soundd: suspended` printed by the suspend block below, which
    // `check_suspend_structure` (tests/common/audio.rs) requires after the
    // last client removal on every audio run. That one must stay a single
    // format piece so it lands contiguously on the shared console.
    //
    // This boot emission is not asserted — the gate's capture opens at
    // ===TEST_START, long after soundd starts — and `soundd: resumed` is read
    // by no test at all. Renaming either of those two breaks nothing that
    // would tell you; they are diagnostics.
    say!("soundd: suspended");

    loop {
        let was_streaming = !streams.is_empty();

        // Signal all clients BEFORE the io_uring wait, so priority inheritance
        // fills their ring slots while soundd is blocked in the poller below.
        signal_clients(&mut streams, ramp_frames);

        // The prediction this wait is armed against, when there is one.
        // Lateness is only defined relative to an instant soundd asked to be
        // woken at, and two waits name none: the idle path (§5.8) arms no timer
        // at all, and before the DLL locks there is no prediction to arm on.
        let mut armed_on: Option<f64> = None;

        let timeout = if streams.is_empty() {
            u64::MAX
        } else {
            match dll.t_estimated {
                None => period_nanos,
                Some(t_est) => {
                    let now = syscall::clock_nanos() as f64;
                    let target = if t_est > now {
                        t_est
                    } else {
                        // Past due: arm for the next future grid point, not a
                        // blind full period from now.
                        let k = ((now - t_est) / dll.period).floor() + 1.0;
                        t_est + k * dll.period
                    };
                    armed_on = Some(t_est);
                    // timeout 0 is the kernel's non-blocking sentinel
                    ((target - now) as u64).max(1)
                }
            }
        };

        poller.watch_raw(backend.handle(), READABLE, TOKEN_AUDIO);
        poller.watch_raw(cmd_pipe_read, READABLE, TOKEN_CMD);

        let mut cmd_ready = false;
        poller.wait(1, timeout, |token| match token {
            TOKEN_AUDIO => {}
            TOKEN_CMD => cmd_ready = true,
            other => panic!("soundd: unexpected poll token {other}"),
        });

        if was_streaming {
            stats.wakes += 1;
        }

        if cmd_ready {
            let mut drain = [0u8; 64];
            while matches!(syscall::read_nonblock(cmd_pipe_read, &mut drain), Ok(n) if n == drain.len()) {}
        }
        apply_commands(cmd_ring, &mut streams, ramp_frames);

        if !was_streaming && !streams.is_empty() {
            stats = MixStats::default();
            next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;
        }

        let n_records = backend.completions(&mut records);
        if n_records > 0 {
            // Measured against the prediction this wait was *armed* on, not
            // against whatever the DLL holds when the wait returns. They differ
            // on a window's first wake, armed while soundd was still idle and
            // asking for no wake time at all — reading the estimate directly
            // scores that sleep as a missed deadline. Nothing is hidden:
            // whenever soundd armed a timer the distance from that prediction
            // is the sample, however large.
            if let Some(t_est) = armed_on {
                let lateness = syscall::clock_nanos().saturating_sub(t_est as u64);
                stats.max_wake_lat_ns = stats.max_wake_lat_ns.max(lateness);
            }
            let mut wake_completions = 0u32;
            for rec in &records[..n_records] {
                let n = rec.mask.count_ones();
                assert!(n > 0, "soundd: completion record with empty mask");
                assert_eq!(free_mask & rec.mask, 0, "soundd: repeated completion for free buffer");
                // Where the engine is, taken from what it reported rather than
                // predicted: a mask a driver reads late is the OR of every
                // `completed` since it last looked, so it can name a whole lap
                // — which places the engine nowhere — and a cursor soundd
                // stepped itself would have to be right about how many laps
                // that was. Re-deriving it per record cannot drift.
                if pipeline == Pipeline::Ring {
                    match stream::decode(rec.mask, num_buffers) {
                        Some(stream::Completed::Run { first, count }) => {
                            ring_cursor = (first + count) % num_buffers;
                        }
                        // Every period played and the mask says no more than
                        // that. The cursor stays where it was: the fill order
                        // from here is a guess either way, and a lap of silence
                        // has already gone out — §5.9 counts it as the drain it
                        // is, and the next record re-anchors.
                        Some(stream::Completed::Lapped) => {}
                        None => panic!(
                            "soundd: the engine completed {:#x}, which is no walk of a \
                             {num_buffers}-period ring",
                            rec.mask
                        ),
                    }
                }
                unplayed = unplayed.saturating_sub(n as usize);
                free_mask |= rec.mask;
                // §2.4's zero-on-complete, before anything can decide to leave
                // this buffer unfilled: the engine returns to it in
                // `num_buffers` periods whatever soundd does.
                for idx in 0..num_buffers {
                    if rec.mask & (1 << idx) != 0 {
                        backend.released(idx);
                    }
                }
                wake_completions += n;
                dll.update(rec.timestamp_nanos as f64, n);
            }
            if !streams.is_empty() {
                stats.completions += wake_completions;
                stats.max_batch = stats.max_batch.max(wake_completions);
            }
        }

        // §5.9: nothing unplayed left means the pipeline drained. What died with
        // it is the *clock*, not the audio — the device restarts its period grid
        // from whatever we submit next, so the DLL estimate must be dropped or
        // the next update reads the discontinuity as drift and drags the
        // period. The buffers themselves are refilled by the ordinary mix loop
        // below: submitting a full pipeline of silence instead would cost
        // `num_buffers` periods of audible dropout for a stall of any length.
        //
        // Counting a drain is narrower than detecting one. `drains` means
        // "soundd was late enough that the device ran out of audio", so the
        // three ways to see an empty pipeline without being late must not raise
        // it: the idle path (§5.8) empties the pipeline by design and is the
        // only wake with `was_streaming` false; a device retiring faster than
        // it plays is rejected arithmetically by `min_drain_nanos`, which no
        // device playing at its own rate can beat; and a previous cycle's
        // deferral is soundd's own restraint, not a stall, so it suppresses the
        // DLL reset too.
        if unplayed == 0 && deferred_last == 0 {
            let since_filled = syscall::clock_nanos().saturating_sub(pipeline_filled_ns);
            if was_streaming && since_filled >= min_drain_nanos {
                stats.drains += 1;
            }
            dll.reset();
        }

        // On a ring the free list is the engine's to state, never a tally
        // soundd keeps across a wake.
        //
        // While the engine runs, the periods soundd may write are the ones this
        // wake was handed back and no others: one it does not fill is played
        // anyway, as the silence `released` left in it, and completed again a
        // lap later — a completion for a buffer soundd still holds, which is
        // the assertion above and what took soundd down on the T14. So with no
        // client to mix for they are given up rather than held.
        //
        // While it is stopped it holds nothing at all, so the whole ring is
        // soundd's: that is the free list the next client's prime fills, and
        // it starts at the cursor because the engine froze inside the period
        // after the last one it completed and carries on there.
        if pipeline == Pipeline::Ring {
            if !started {
                free_mask = (1u32 << num_buffers) - 1;
            } else if streams.is_empty() {
                free_mask = 0;
            }
        }
        // Where the fill starts: the beginning of the run soundd may write,
        // which is as many periods back from where the engine now stands as
        // there are of them. A stopped engine's whole lap lands on the same
        // place, which is where it will carry on from.
        let mut fill_at =
            (ring_cursor + num_buffers - free_mask.count_ones() as usize % num_buffers)
                % num_buffers;

        let mut refilled = false;
        let mut deferred: u32 = 0;
        // With no clients there is nothing to mix: leaving the freed buffers
        // unsubmitted is what drains the pipeline (§5.8) instead of feeding
        // the device silence forever.
        while free_mask != 0 && !streams.is_empty() {
            let idx = match pipeline {
                // Any order will do: the device plays what it is given in the
                // order it is given, so the free list is a set.
                Pipeline::Queue => free_mask.trailing_zeros() as usize,
                // The engine's order, which is the ring's and not the index's.
                Pipeline::Ring => {
                    let at = fill_at;
                    fill_at = (fill_at + 1) % num_buffers;
                    at
                }
            };
            assert!(idx < num_buffers, "soundd: completion for nonexistent buffer {idx}");
            assert!(free_mask & (1 << idx) != 0, "soundd: buffer {idx} is not free to fill");
            free_mask &= !(1 << idx);

            // §5.10's "wait until clients have filled", reached by deferring
            // the buffer rather than blocking on the client — which needs no
            // reverse notification, since the ring indices soundd already maps
            // say the same thing.
            //
            // A streaming client whose ring is empty was signalled microseconds
            // ago and is mid-callback, not absent. The ring is `num_buffers`
            // deep precisely so a mix cycle that outruns the client costs
            // margin rather than audio; filling this buffer with silence spends
            // that margin on the one thing it exists to prevent. Deferring is
            // safe for exactly as long as audio already on the wire has not run
            // out, so a client that stops producing altogether still costs
            // silence — at the floor rather than immediately.
            //
            // **A [`Pipeline::Ring`] cannot take that bet.** The floor is a
            // bound on unplayed audio, not on the engine's return, and the
            // engine reaches this period again in `num_buffers` periods and
            // plays the silence `released` left in it — so deferring buys the
            // very gap it exists to avoid, and then hands soundd a completion
            // for a buffer it still holds. It buys nothing even when soundd is
            // in time: the client's period lands one period later than the
            // engine wanted it either way.
            let now = syscall::clock_nanos();
            let mid_refill = pipeline == Pipeline::Queue
                && refill_floor_nanos.is_some()
                && streams.iter().any(|s| s.is_streaming() && s.slot_reader.peek().is_none());
            if mid_refill
                && refill_floor_nanos
                    .is_some_and(|floor| playout_until_ns.saturating_sub(now) >= floor)
            {
                deferred |= 1 << idx;
                stats.deferred += 1;
                continue;
            }

            mix_f32.fill(0.0);

            let mut any_data = false;
            let mut any_streaming = false;
            for stream in streams.iter_mut() {
                let covered = mix_client(
                    stream,
                    &mut mix_f32,
                    &mut decode_buf,
                    &mut convert_buf,
                    device_channels as usize,
                    device_period_frames,
                );
                if covered && !stream.delivered {
                    stream.delivered = true;
                }
                any_data |= covered;
                any_streaming |= stream.is_streaming();
            }

            let dma_buf = unsafe {
                core::slice::from_raw_parts_mut(backend.buffer(idx) as *mut i16, device_period_samples)
            };
            quantize_period(dma_buf, &mix_f32, &mut dither_rng);

            if !started {
                started = true;
                // Before the submit, because that is where a stopped stream is
                // started: the marker has to precede whatever the backend logs
                // about starting.
                say!("soundd: resumed");
            }
            backend.submit(idx, device_period_bytes);
            unplayed += 1;
            // Plays after whatever is already queued — unless that has all
            // played out, in which case the device restarts from now.
            playout_until_ns = playout_until_ns.max(now) + period_nanos;
            refilled = true;
            stats.submitted += 1;
            stats.period(any_streaming, any_data);
        }
        // Deferred buffers stay free and are reconsidered next cycle, by which
        // point the client has had another signal-to-mix window to produce.
        free_mask |= deferred;
        deferred_last = deferred;
        // Not re-stamped by a cycle that only deferred: no audio was added, so
        // the pipeline's remaining depth still dates from the previous fill.
        if refilled {
            pipeline_filled_ns = syscall::clock_nanos();
        }

        retain_active(&mut streams);

        // §5.8 DRAINING → SUSPENDED, on the completion that plays out the last
        // filled period. The stop is immediate: grace between the drain and the PCM
        // STOP is zero, and that is policy like `refill_floor_nanos` above,
        // not physics. virtio STOP does not RELEASE — SET_PARAMS and PREPARE
        // stay valid and resume is one control verb inline with the first
        // submit — so there is no codec pop or renegotiation for grace to
        // amortize, and stopping at once is what puts the suspend markers
        // inside the audio gate's serial window on every run. The one event
        // that makes grace nonzero is a hardware backend that pops on stop,
        // advertised through the trait above; implement it then as a
        // clock comparison against a drain stamp, evaluated at the idle wakes
        // that still arrive while the buffers play out — never as an armed
        // timer, which would put a periodic wake back into the idle path this
        // whole state exists to empty.
        //
        // This block must not move into the full-drain site above:
        // that site is gated on `deferred_last == 0`, and a final streaming
        // cycle that deferred plus a whole-pipeline completion batch — QEMU's
        // routine cadence — would skip it, parking soundd forever with the
        // device started and nothing left to complete. The `started` guard
        // keeps a stray cmd wake (a SetVolume for a removed client) from
        // costing a controlq round trip.
        if started && streams.is_empty() && unplayed == 0 {
            // The device's period grid dies with the stream; the next
            // completion after resume re-initializes the estimate.
            dll.reset();
            backend.stop();
            started = false;
            say!("soundd: suspended");
        }

        // Flushing on the last disconnect keeps the tail between the final
        // periodic window and the client leaving in the record — for a stream
        // shorter than two windows that tail is most of it.
        let now_ns = syscall::clock_nanos();
        if was_streaming && streams.is_empty() {
            report(&stats, 0);
            stats = MixStats::default();
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        } else if now_ns >= next_stats_ns {
            if !streams.is_empty() {
                report(&stats, streams.len());
                stats = MixStats::default();
            }
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        }
    }
}

/// The default output on a machine with no audio hardware. It presents the same
/// virtual device every client negotiates against and drains each stream at that
/// real rate off a monotonic software clock, discarding the mix. Hardware
/// absence is a routing state, never an error: a client's write and backpressure
/// timing is identical to a real device, so nothing upstream can tell its audio
/// reaches nowhere.
///
/// The null sink *is* the mix loop clocked by a timer instead of a device. It
/// reuses every per-client mechanism — `mix_client`, the gain ramps, crash
/// detection, the command ring — and drops only what a device provides: there is
/// no DMA pipeline, so no DLL, no completion records, no dither, and no submit.
/// After mixing one period it throws the samples away.
///
/// Idle discipline matches §5.8: with no streams it holds no timer and takes no
/// wakes, blocking on the command pipe alone, so an audience of zero costs
/// exactly zero CPU. It does not request the RT band — it protects no audible
/// output, so there is nothing for the band to protect.
fn null_sink_thread(
    cmd_ring: &CommandRing,
    cmd_pipe_read: RawHandle,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: usize,
    ramp_frames: u32,
) {
    let device_period_samples = device_period_frames * device_channels as usize;
    let period_nanos = period_nanos(device_period_frames as u64, device_sample_rate as u64);

    let mut streams: Vec<ClientStream> = Vec::new();
    let poller = Poller::new(64);
    let mut mix_f32 = vec![0.0f32; device_period_samples];
    // Sized for the highest client rate accepted at stream open, exactly as
    // mix_thread sizes its scratch, so `mix_client` never allocates.
    let max_client_frames = scratch_frames(device_period_frames, device_sample_rate as usize);
    let mut decode_buf = vec![0.0f32; max_client_frames * 2];
    let mut convert_buf = vec![0.0f32; max_client_frames * 2];

    const TOKEN_CMD: u64 = u64::MAX - 2;

    let mut stats = MixStats::default();
    let mut next_stats_ns = syscall::clock_nanos() + STATS_INTERVAL_NANOS;
    // The virtual playout grid: the wall-clock instant the next period is due.
    // Meaningful only while streaming; re-anchored to now+one period when the
    // first client of a run connects.
    let mut next_period_ns = syscall::clock_nanos();

    say!("soundd: null sink idle");

    loop {
        let was_streaming = !streams.is_empty();

        signal_clients(&mut streams, ramp_frames);

        // Idle discipline (§5.8): no streams → no timer, no wakes. A connect
        // arrives as a command-pipe byte, the only wake source the null sink
        // has. While streaming, wake at the next grid point.
        let timeout = if streams.is_empty() {
            u64::MAX
        } else {
            next_period_ns.saturating_sub(syscall::clock_nanos()).max(1)
        };

        poller.watch_raw(cmd_pipe_read, READABLE, TOKEN_CMD);
        let mut cmd_ready = false;
        poller.wait(1, timeout, |token| match token {
            TOKEN_CMD => cmd_ready = true,
            other => panic!("soundd: unexpected null-sink poll token {other}"),
        });

        if was_streaming {
            stats.wakes += 1;
        }

        if cmd_ready {
            let mut drain = [0u8; 64];
            while matches!(syscall::read_nonblock(cmd_pipe_read, &mut drain), Ok(n) if n == drain.len()) {}
        }
        apply_commands(cmd_ring, &mut streams, ramp_frames);

        // Start the grid when the first client of a run connects, and reset the
        // reporting window so no idle stretch dilutes it.
        if !was_streaming && !streams.is_empty() {
            let now = syscall::clock_nanos();
            next_period_ns = now + period_nanos;
            stats = MixStats::default();
            next_stats_ns = now + STATS_INTERVAL_NANOS;
        }

        // Drain every period the grid says is due, discarding the mix. This is
        // the whole difference from a real device: exactly one period consumed
        // per `period_nanos` of wall clock, so a client's ring drains — and its
        // writes backpressure — at the real audio rate. The batch is capped at
        // the ring depth: a client can be at most `slot_count` periods ahead, so
        // a wake that would drain more than that overslept long enough for the
        // grid to be a dead reference (a loaded CPU, a host suspend). It is
        // re-anchored to now rather than chasing the lost time, which nothing
        // heard it play.
        let mut batch = 0u32;
        while !streams.is_empty() && batch < NULL_SINK_BUFFERS as u32 {
            let now = syscall::clock_nanos();
            if now < next_period_ns {
                break;
            }
            let lateness = now.saturating_sub(next_period_ns);
            stats.max_wake_lat_ns = stats.max_wake_lat_ns.max(lateness);

            mix_f32.fill(0.0);
            let mut any_data = false;
            let mut any_streaming = false;
            for stream in streams.iter_mut() {
                let covered = mix_client(
                    stream,
                    &mut mix_f32,
                    &mut decode_buf,
                    &mut convert_buf,
                    device_channels as usize,
                    device_period_frames,
                );
                if covered && !stream.delivered {
                    stream.delivered = true;
                }
                any_data |= covered;
                any_streaming |= stream.is_streaming();
            }
            // The mix is discarded here — no dither, no DMA buffer, no submit.
            stats.submitted += 1;
            stats.completions += 1;
            stats.period(any_streaming, any_data);
            next_period_ns += period_nanos;
            batch += 1;
        }
        if batch == NULL_SINK_BUFFERS as u32 {
            next_period_ns = syscall::clock_nanos() + period_nanos;
        }
        stats.max_batch = stats.max_batch.max(batch);

        retain_active(&mut streams);

        // Reporting: flush on the last disconnect so a short stream's tail is in
        // the record, and every STATS_INTERVAL_NANOS while streaming — the same
        // cadence and format mix_thread uses, so a discarded stream is not
        // silent about being discarded (#106's status tool reads one shape).
        let now_ns = syscall::clock_nanos();
        if was_streaming && streams.is_empty() {
            report(&stats, 0);
            stats = MixStats::default();
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
            say!("soundd: null sink idle");
        } else if now_ns >= next_stats_ns {
            if !streams.is_empty() {
                report(&stats, streams.len());
                stats = MixStats::default();
            }
            next_stats_ns = now_ns + STATS_INTERVAL_NANOS;
        }
    }
}

/// The widest control payload soundd decodes, **plus one**.
///
/// `StreamOpenRequest` is the only payload wider than a bare signal. The `+ 1`
/// is what keeps an over-long frame a refusal rather than a truncation, and it
/// is the one behaviour the hand-written buffer this replaced had that the
/// SDK's does not: `FrameRx` keeps `min(declared, N)` bytes, so with `N` equal
/// to the struct, a client declaring a hundred bytes of `MSG_STREAM_OPEN` would
/// report exactly the struct's width and open a stream from its first eight.
/// One byte of slack makes that client report one more than the struct instead,
/// which [`classify`]'s exact-width guard refuses by name. Nothing legitimate
/// reaches it — the SDK sends the struct — so the byte costs a byte of stack
/// and buys back a refusal.
const MAX_KEPT_PAYLOAD: usize = core::mem::size_of::<StreamOpenRequest>() + 1;

const _: () = assert!(
    core::mem::size_of::<StreamSetVolume>() < MAX_KEPT_PAYLOAD,
    "a control payload wider than the frame buffer would be refused as malformed",
);

/// One client's inbound framing.
///
/// **soundd never reads a client with a blocking read.** `ipc::recv_header` and
/// `ipc::recv_payload` park the caller until the peer sends the bytes it
/// promised, which hands a client the decision of when the control thread runs
/// again: one client parking a partial header would wedge accept and
/// volume/close/disconnect handling for every other client (the mix thread is
/// unaffected either way). This was soundd's own buffer until the SDK's grew a
/// non-blocking one; the doctrine is `userland/CLAUDE.md`'s, and the type is
/// the same one init, the compositor, netd and every surface host read with.
///
/// A client may declare anything up to `ipc::MAX_FRAME_LEN`; the excess past
/// [`MAX_KEPT_PAYLOAD`] is counted down and discarded rather than waited for, so
/// the connection stays framed whatever a peer declares — the SDK's rule, which
/// the compositor states too (`compositor::client::MAX_KEPT_PAYLOAD`). What such
/// a frame is *worth* is [`classify`]'s answer rather than this type's: it
/// arrives reporting exactly `MAX_KEPT_PAYLOAD` bytes, one more than any payload
/// here is wide, and is refused there.
type ControlRx = ipc::FrameRx<MAX_KEPT_PAYLOAD>;

/// What one framed control message asks for, decided before anything is done.
///
/// This is the whole of the framing soundd still owns: [`ControlRx`] decides
/// where a frame ends, and this decides whether what came out of it is a
/// message this client may send at all. A payload that is not exactly as long
/// as the struct its type names is refused here rather than decoded — the
/// `expect` on the open path rests on this guard and not on the peer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Control {
    Open,
    SetVolume { idx: usize },
    Close { idx: usize },
    /// An unknown message type, a payload the wrong width for its type, or a
    /// message this connection is not in the state to send. All three are the
    /// client getting it wrong, and all three end the connection.
    Violation,
}

fn classify(msg_type: u32, payload_len: usize, stream_idx: Option<usize>) -> Control {
    // **A frame wider than anything this server decodes is a violation whatever
    // its type**, and this is where the buffer this replaced refused it — at the
    // header, before the type was looked at. `FrameRx` reports an over-long
    // frame as `MAX_KEPT_PAYLOAD` exactly (see that constant's `+ 1`), so one
    // comparison is the whole of that refusal.
    if payload_len >= MAX_KEPT_PAYLOAD {
        return Control::Violation;
    }
    match (msg_type, stream_idx) {
        (MSG_STREAM_OPEN, None) if payload_len == core::mem::size_of::<StreamOpenRequest>() => {
            Control::Open
        }
        (MSG_STREAM_SET_VOLUME, Some(idx))
            if payload_len == core::mem::size_of::<StreamSetVolume>() =>
        {
            Control::SetVolume { idx }
        }
        (MSG_STREAM_CLOSE, Some(idx)) => Control::Close { idx },
        _ => Control::Violation,
    }
}

fn reject_open(req: &StreamOpenRequest) -> Option<&'static str> {
    if req.format != FORMAT_S16LE {
        return Some("unsupported sample format");
    }
    if req.channels != 1 && req.channels != 2 {
        return Some("unsupported channel count");
    }
    if !(MIN_CLIENT_RATE..=MAX_CLIENT_RATE).contains(&req.sample_rate) {
        return Some("unsupported sample rate");
    }
    None
}

/// Hand one command to the mix thread, waiting for room if the ring is full.
///
/// The mix thread drains the whole ring at the top of every cycle, so a full
/// ring means it has not run for a cycle and one device period is exactly how
/// long there is to wait. Throttling the control thread is the point: the
/// alternatives are dropping a command (a client stranded in the mix thread
/// forever) or asserting (soundd dead at a client's choosing). The retry is
/// unbounded because the mix thread is the process's main thread — if it has
/// stopped, soundd is already gone.
fn submit(cmd_ring: &CommandRing, cmd_pipe_write: RawHandle, cmd: MixCommand, period_nanos: u64) {
    let mut cmd = cmd;
    loop {
        let full = cmd_ring.try_push(cmd);
        let _ = syscall::write_nonblock(cmd_pipe_write, &[1]);
        match full {
            Ok(()) => return,
            Err(returned) => {
                cmd = returned;
                syscall::nanosleep(period_nanos);
            }
        }
    }
}

/// Tell the mix thread a stream ended, and how.
///
/// Every removal the control thread issues goes through here, so the witness it
/// holds — which of §7's four ways this stream ended — travels with the command
/// instead of being reconstructed from a flag on the other side.
fn remove(
    cmd_ring: &CommandRing,
    cmd_pipe_write: RawHandle,
    client_id: usize,
    departure: Departure,
    period_nanos: u64,
) {
    submit(
        cmd_ring,
        cmd_pipe_write,
        MixCommand::RemoveClient { client_id, departure },
        period_nanos,
    );
}

fn control_thread(
    acceptor: Acceptor,
    cmd_ring: &CommandRing,
    cmd_pipe_write: RawHandle,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_frames: u32,
    slot_count: u32,
    ramp_frames: u32,
) {
    // One handle per client plus the acceptor; `MAX_CONTROL_CLIENTS` is derived
    // from this ring, so the set always fits in one batch.
    let poller = Poller::new(MAX_CONTROL_CLIENTS as u32 + 1);
    let period_nanos = period_nanos(device_period_frames as u64, device_sample_rate as u64);

    struct ControlClient {
        conn: Connection,
        rx: ControlRx,
        // Set once MSG_STREAM_OPEN succeeds; accepted-but-silent connections
        // stay pending so they cannot stall the control plane.
        stream_idx: Option<usize>,
        /// Latest volume this client asked for, not yet handed to the mix
        /// thread. Volume is state, not an event: `set_target` overwrites the
        /// ramp outright, so applying N of them within one mix cycle leaves
        /// exactly what applying only the last would. Collapsing is therefore
        /// lossless, and it keeps a client's message rate off the command ring
        /// — the drain loop below reads everything a client has written before
        /// it yields.
        pending_volume: Option<Gain>,
    }

    let mut clients: Vec<ControlClient> = Vec::new();
    let mut next_idx: usize = 0;

    const TOKEN_ACCEPT: u64 = u64::MAX;

    loop {
        poller.watch(&acceptor, READABLE, TOKEN_ACCEPT);
        for (i, client) in clients.iter().enumerate() {
            poller.watch(&client.conn, READABLE, i as u64);
        }

        let mut ready: Vec<u64> = Vec::new();
        poller.wait(1, u64::MAX, |t| ready.push(t));

        if ready.contains(&TOKEN_ACCEPT) {
            match acceptor.accept() {
                // Refused rather than left queued: a connection past the
                // poller's watchable set would never be read from. It is still
                // accepted first — leaving it in the port's queue keeps the
                // acceptor readable and spins this loop.
                Ok(conn) if clients.len() >= MAX_CONTROL_CLIENTS => {
                    say!("soundd: refusing connection, {MAX_CONTROL_CLIENTS} clients already connected");
                    let _ = conn.signal(MSG_STREAM_ERROR);
                }
                Ok(conn) => {
                    clients.push(ControlClient {
                        conn,
                        rx: ControlRx::new(),
                        stream_idx: None,
                        pending_volume: None,
                    });
                }
                Err(e) => say!("soundd: accept failed: {e:?}"),
            }
        }

        let mut dead: Vec<usize> = Vec::new();
        for i in 0..clients.len() {
            if !ready.contains(&(i as u64)) {
                continue;
            }
            let mut disconnected = false;
            'msgs: loop {
                let step = {
                    let c = &mut clients[i];
                    c.rx.pump(&c.conn)
                };
                let (msg_type, payload_len) = match step {
                    RxStep::Idle => break 'msgs,
                    // The client's process is gone; whether it exited or
                    // crashed is not knowable here.
                    RxStep::Eof => {
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Disconnected, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    // A length no frame here can carry. Nothing after it can be
                    // located, so there is nothing to resynchronise to — and
                    // unlike the EOF above, this one soundd caused.
                    RxStep::Malformed => {
                        say!("soundd: frame this protocol cannot describe, disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    RxStep::Frame { msg_type, payload_len } => (msg_type, payload_len),
                };
                // The payload travels with the frame instead of being read off
                // the connection during dispatch: the read side is finished before
                // anything below acts on the message.
                let mut payload = [0u8; MAX_KEPT_PAYLOAD];
                payload[..payload_len].copy_from_slice(clients[i].rx.payload(payload_len));
                match classify(msg_type, payload_len, clients[i].stream_idx) {
                    Control::Open => {
                        let req: StreamOpenRequest = ipc::decode_payload(&payload[..payload_len])
                            .expect("a payload as long as the struct decodes");
                        if let Some(reason) = reject_open(&req) {
                            say!("soundd: rejecting stream ({reason}): {}Hz {}ch fmt={}",
                                req.sample_rate, req.channels, req.format);
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        }
                        say!("soundd: opening stream: {}Hz {}ch fmt={}",
                            req.sample_rate, req.channels, req.format);
                        let idx = next_idx;
                        next_idx += 1;
                        let Some(client) = open_stream(
                            idx,
                            &req,
                            &clients[i].conn,
                            device_sample_rate,
                            device_channels,
                            device_period_frames,
                            slot_count,
                            ramp_frames,
                        ) else {
                            let _ = clients[i].conn.signal(MSG_STREAM_ERROR);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        submit(cmd_ring, cmd_pipe_write, MixCommand::AddClient(Box::new(client)), period_nanos);
                        clients[i].stream_idx = Some(idx);
                    }
                    Control::SetVolume { idx } => {
                        let raw = f32::from_le_bytes(payload[0..4].try_into().unwrap());
                        let Some(gain) = Gain::from_wire(raw) else {
                            say!("soundd: volume is not a number, disconnecting client");
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                            dead.push(i);
                            disconnected = true;
                            break 'msgs;
                        };
                        clients[i].pending_volume = Some(gain);
                    }
                    Control::Close { idx } => {
                        remove(cmd_ring, cmd_pipe_write, idx, Departure::Closed, period_nanos);
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                    Control::Violation => {
                        say!("soundd: protocol violation (msg {msg_type}), disconnecting client");
                        if let Some(idx) = clients[i].stream_idx {
                            remove(cmd_ring, cmd_pipe_write, idx, Departure::Refused, period_nanos);
                        }
                        dead.push(i);
                        disconnected = true;
                        break 'msgs;
                    }
                }
            }
            // One command for however many volume messages that drain carried.
            // A disconnecting client is skipped: its `RemoveClient` is already
            // queued, and a `SetVolume` behind it would aim the ramp at the
            // volume instead of at silence and strand the stream unremoved.
            if !disconnected {
                if let (Some(client_id), Some(target)) = (clients[i].stream_idx, clients[i].pending_volume.take()) {
                    submit(cmd_ring, cmd_pipe_write, MixCommand::SetVolume { client_id, target }, period_nanos);
                }
            }
        }
        for &i in dead.iter().rev() {
            clients.remove(i);
        }
    }
}

/// The virtual output soundd presents when the machine has no audio hardware.
/// These match the one configuration cpal's ToyOS backend advertises
/// (`src/host/toyos/mod.rs`): 44100 Hz stereo i16, 128 frames per period. A
/// stream negotiates against them exactly as it would a real device, so a
/// no-hardware machine is invisible to the client.
const NULL_SINK_RATE: u32 = 44_100;
const NULL_SINK_CHANNELS: u16 = 2;
const NULL_SINK_PERIOD_FRAMES: usize = 128;
/// Same DMA-pipeline depth as both hardware backends: the client ring
/// is as deep, so a client may fill `NULL_SINK_BUFFERS - 1` periods ahead and
/// its backpressure is the device's. Power of two — ring indices wrap mod 2^32.
const NULL_SINK_BUFFERS: usize = 8;

fn main() {
    let acceptor = endow::acceptor("soundd")
        .expect("the manifest declares this program serves `soundd`");

    // **"Which sound card does this machine have?" is already answered.** init
    // mints a claim per class the manifest names and endows what the machine
    // actually had, so an absent card is a label missing from this process's
    // own table rather than two probing syscalls — and a card another process
    // holds is not a state that can arise, because only init mints.
    //
    // A machine with no sound card is a routing state and not a bug: soundd
    // presents a virtual output and discards what is played to it, so a client
    // building a stream succeeds whether or not hardware is present.
    //
    // The order is virtio first, and it is not a preference between two cards:
    // no machine in this project has both. The T14 has only the second.
    if let Some(dev) = endow::device::<VirtioSoundDev>(DeviceType::VirtioSound) {
        match virtio::Virtio::claim(dev) {
            Ok((virtio, rate, channels)) => return run_virtio(acceptor, virtio, rate, channels),
            Err(why) => {
                say!("soundd: the virtio-sound device cannot carry audio: {why}");
                return run_null_sink(acceptor);
            }
        }
    }
    let Some(dev) = endow::device::<HdaDev>(DeviceType::HdaAudio) else {
        return run_null_sink(acceptor);
    };
    match hda::Hda::claim(dev) {
        Ok((hda, _path, channels)) => run_hda(acceptor, hda, channels),
        Err(why) => {
            say!("soundd: the HDA controller cannot carry audio: {why}");
            run_null_sink(acceptor)
        }
    }
}

fn run_virtio(acceptor: Acceptor, virtio: virtio::Virtio, rate: u32, channels: u8) {
    run_with_device(
        acceptor,
        &mut VirtioBackend { virtio },
        toyos_abi::virtio_sound::PERIODS,
        rate,
        channels as u16,
        toyos_abi::virtio_sound::PERIOD_BYTES,
    );
}

fn run_hda(acceptor: Acceptor, hda: hda::Hda, channels: u8) {
    let info = hda.info();
    let num_buffers = info.periods as usize;
    let period_bytes = info.period_bytes as usize;
    let ring = SharedMemory::adopt(info.pcm, 2 * 1024 * 1024)
        .expect("the PCM buffer the HDA claim just handed over");
    // One region, `periods` buffers end to end: the buffer descriptor list the
    // kernel built points at exactly these offsets.
    let base = ring.as_ptr();
    let buffers = (0..num_buffers).map(|i| unsafe { base.add(i * period_bytes) }).collect();

    run_with_device(
        acceptor,
        &mut HdaBackend { hda, buffers, period_bytes },
        num_buffers,
        toyos_hda::config::RATE,
        channels as u16,
        period_bytes,
    );
}

fn run_with_device(
    acceptor: Acceptor,
    backend: &mut dyn Backend,
    num_buffers: usize,
    device_sample_rate: u32,
    device_channels: u16,
    device_period_bytes: usize,
) {
    // A shape this mixer cannot render is named and the machine gets the null
    // sink, which is what §6 is for. It is checked before any arithmetic
    // derives anything from it — a zero channel count divides by zero on the
    // way to a frame count, which is a panic that names neither the device nor
    // the reason.
    let device_period_frames = match period_frames(num_buffers, device_channels, device_period_bytes) {
        Ok(frames) => frames,
        Err(why) => {
            say!("soundd: this audio device's shape cannot carry audio: {why}");
            return run_null_sink(acceptor);
        }
    };

    // Client ring depth matches the DMA pipeline depth: a wake gap can free
    // at most num_buffers periods, so a full client ring always covers it.
    let slot_count = num_buffers as u32;

    let ramp_frames = ramp_frames(device_sample_rate);

    say!("soundd: ready, {} buffers, {}Hz {}ch, {} bytes/period, {} frames/period",
        num_buffers, device_sample_rate, device_channels, device_period_bytes, device_period_frames);

    let cmd_ring = Arc::new(CommandRing::new());
    let cmd_pipe = syscall::pipe().expect("soundd: failed to create the command pipe");

    let cmd_ring2 = cmd_ring.clone();
    std::thread::Builder::new()
        .name("soundd-ctrl".into())
        .spawn(move || {
            control_thread(
                acceptor,
                &cmd_ring2,
                cmd_pipe.write,
                device_sample_rate,
                device_channels,
                device_period_frames as u32,
                slot_count,
                ramp_frames,
            );
        })
        .expect("soundd: failed to spawn control thread");

    mix_thread(
        backend,
        &cmd_ring,
        cmd_pipe.read,
        num_buffers,
        device_sample_rate,
        device_channels,
        device_period_bytes,
        device_period_frames,
        ramp_frames,
    );
}

fn run_null_sink(acceptor: Acceptor) {
    let device_sample_rate = NULL_SINK_RATE;
    let device_channels = NULL_SINK_CHANNELS;
    let device_period_frames = NULL_SINK_PERIOD_FRAMES;
    let slot_count = NULL_SINK_BUFFERS as u32;

    let ramp_frames = ramp_frames(device_sample_rate);

    say!(
        "soundd: no audio device, presenting a null sink ({}Hz {}ch, {} frames/period, streams discarded)",
        device_sample_rate, device_channels, device_period_frames
    );

    let cmd_ring = Arc::new(CommandRing::new());
    let cmd_pipe = syscall::pipe().expect("soundd: failed to create the command pipe");

    let cmd_ring2 = cmd_ring.clone();
    std::thread::Builder::new()
        .name("soundd-ctrl".into())
        .spawn(move || {
            control_thread(
                acceptor,
                &cmd_ring2,
                cmd_pipe.write,
                device_sample_rate,
                device_channels,
                device_period_frames as u32,
                slot_count,
                ramp_frames,
            );
        })
        .expect("soundd: failed to spawn control thread");

    null_sink_thread(
        &cmd_ring,
        cmd_pipe.read,
        device_sample_rate,
        device_channels,
        device_period_frames,
        ramp_frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The race the `died` line lost: two witnesses, either order, one word.
    ///
    /// The control thread read the peer; the mix loop only found a descriptor
    /// missing. Whichever arrives first, the removal must be reported with what
    /// was actually established — and a clean exit must never be reported as a
    /// death, which is what `SignalPipeGone` refuses to claim.
    #[test]
    fn the_stronger_witness_wins_in_either_order() {
        use Departure::*;
        for (a, b) in [(Closed, SignalPipeGone), (Refused, SignalPipeGone), (Disconnected, SignalPipeGone)] {
            assert_eq!(a.refine(b), a, "{b} must not replace {a}");
            assert_eq!(b.refine(a), a, "{a} must replace {b}");
        }
        // A client that asked to close is not downgraded by the connection it
        // then dropped.
        assert_eq!(Closed.refine(Disconnected), Closed);
        assert_eq!(Disconnected.refine(Closed), Closed);
        // Idempotent, so a repeated witness — the mix loop writes a broken pipe
        // every period until the ramp finishes — changes nothing.
        for how in [Closed, Refused, Disconnected, SignalPipeGone] {
            assert_eq!(how.refine(how), how);
        }
    }

    /// Every framing decision soundd still makes for itself, with a lying peer
    /// on the other side.
    ///
    /// `ipc::FrameRx` decides where a frame ends and this decides whether what
    /// came out of it is a message the client may send: a payload that is not
    /// exactly as wide as the struct its type names never reaches
    /// `decode_payload`, and a message arriving in the wrong state is refused
    /// whatever it carries. Both used to be inline guards on a hand-rolled
    /// buffer that nothing tested.
    ///
    /// What this cannot reach is `FrameRx`'s own reassembly — a split header, a
    /// declared length past `ipc::MAX_FRAME_LEN` — because pumping one needs a
    /// `Connection`, and reading a `Connection` on the host would issue the
    /// ToyOS `syscall` instruction at a macOS kernel. That half is gated in
    /// QEMU, and soundd has no gate of its own there yet.
    #[test]
    fn a_control_message_is_refused_unless_its_width_and_its_state_both_fit() {
        const OPEN: usize = core::mem::size_of::<StreamOpenRequest>();
        const VOL: usize = core::mem::size_of::<StreamSetVolume>();

        assert_eq!(classify(MSG_STREAM_OPEN, OPEN, None), Control::Open);
        // Every width short of the struct, which is what a peer that declared
        // less than it owes produces — and what a truncated payload looks like
        // from here.
        for short in 0..OPEN {
            assert_eq!(
                classify(MSG_STREAM_OPEN, short, None),
                Control::Violation,
                "a {short}-byte open must be refused, not decoded from whatever follows",
            );
        }
        // A second open on a connection that already carries a stream: one
        // client is one stream, and the `None` in the guard is what says so.
        assert_eq!(classify(MSG_STREAM_OPEN, OPEN, Some(3)), Control::Violation);

        assert_eq!(classify(MSG_STREAM_SET_VOLUME, VOL, Some(3)), Control::SetVolume { idx: 3 });
        // Volume before there is a stream to aim it at, and volume of the wrong
        // width — the `f32` read on the other side is unchecked, so this guard
        // is the only thing standing between a short frame and stale bytes.
        assert_eq!(classify(MSG_STREAM_SET_VOLUME, VOL, None), Control::Violation);
        for wrong in [0, VOL - 1, VOL + 1, OPEN] {
            assert_eq!(classify(MSG_STREAM_SET_VOLUME, wrong, Some(3)), Control::Violation);
        }

        // Close is a bare signal: it needs a stream and it ignores whatever a
        // client chose to attach to it, up to the width the frame buffer keeps.
        assert_eq!(classify(MSG_STREAM_CLOSE, 0, Some(3)), Control::Close { idx: 3 });
        assert_eq!(classify(MSG_STREAM_CLOSE, OPEN, Some(3)), Control::Close { idx: 3 });
        assert_eq!(classify(MSG_STREAM_CLOSE, 0, None), Control::Violation);

        // **The over-long frame, which is the one refusal the SDK's reader does
        // not make for us.** A peer that declares more than any payload here is
        // wide is refused whatever type it names, because `FrameRx` reports the
        // excess as exactly `MAX_KEPT_PAYLOAD` — this is the header check the
        // hand-written buffer used to do, kept.
        for kind in [MSG_STREAM_OPEN, MSG_STREAM_SET_VOLUME, MSG_STREAM_CLOSE] {
            for stream_idx in [None, Some(3)] {
                assert_eq!(
                    classify(kind, MAX_KEPT_PAYLOAD, stream_idx),
                    Control::Violation,
                    "an over-long {kind} frame must be refused, not truncated into a message",
                );
            }
        }

        // Nothing else is a message — including the two soundd only ever sends.
        for other in [0, MSG_STREAM_OPENED, MSG_STREAM_ERROR, u32::MAX] {
            for stream_idx in [None, Some(3)] {
                for len in [0, VOL, OPEN] {
                    assert_eq!(classify(other, len, stream_idx), Control::Violation);
                }
            }
        }
    }
}
