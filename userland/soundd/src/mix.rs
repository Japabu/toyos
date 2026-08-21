//! The two loops: a device's, and the null sink's.
//!
//! Both are the same body of code with one difference — where a mixed period
//! goes and what clocks the next one. The device loop is clocked by completion
//! records through a delay-locked loop and writes into DMA buffers; the null
//! sink is clocked by a monotonic grid and throws the samples away, so a
//! machine with no audio hardware backpressures its clients at exactly the real
//! rate. Everything else — the command ring, the per-client mix, the ramps,
//! crash detection, the underrun accounting, the idle discipline — is shared.
//!
//! **What a period *is* was decided in `toyos-mixer`** before either loop ran:
//! this file spends the answers and owns the effects around them.

use toyos::endow::Endowments;
use toyos::poller::{Poller, READABLE};
use toyos::syscap::SysCap;
use toyos_abi::audio::AudioCompletionRecord;
use toyos_abi::syscall;
use toyos_abi::RawHandle;
use toyos_hda::stream;
use toyos_mixer::{
    deferral_floor_nanos, period_nanos, quantize_period, scratch_frames, Dll, MixStats, Xorshift32,
};

use crate::backend::{Backend, Pipeline};
use crate::client::{mix_client, ClientStream, Departure};
use crate::command::{CommandRing, MixCommand};
use crate::NULL_SINK_BUFFERS;

const STATS_INTERVAL_NANOS: u64 = 2_000_000_000;

/// One reporting window on the console. One line, one `write`.
///
/// The counters are `toyos_mixer::MixStats`, and what they mean is documented
/// there beside the decision that fills them; this is the emission, which is an
/// effect and stays here. `#106`'s status tool reads one shape, so the null
/// sink prints the same line.
fn report(stats: &MixStats, clients: usize) {
    say!("soundd: wakes={} completions={} submitted={} underruns={} drains={} max_wake_lat_us={} max_batch={} clients={} deferred={} starve_max={} worst_irq_late_us={} worst_pickup_us={} worst_empty={} worst_batch={}",
        stats.wakes, stats.completions, stats.submitted, stats.underruns, stats.drains,
        stats.max_wake_lat_ns / 1_000, stats.max_batch, clients, stats.deferred,
        stats.starve_max, stats.worst.irq_late_ns / 1_000, stats.worst.pickup_ns / 1_000,
        stats.worst.empty, stats.worst.batch);
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

pub(crate) fn mix_thread(
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
            // Read before the record loop, because it is what a *pickup* is
            // measured to: the instant soundd first held the record, not the
            // instant it finished acting on a batch of them.
            let seen_at = syscall::clock_nanos();
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
            // Measured against the prediction this wait was *armed* on, not
            // against whatever the DLL holds when the wait returns. They differ
            // on a window's first wake, armed while soundd was still idle and
            // asking for no wake time at all — reading the estimate directly
            // scores that sleep as a missed deadline. Nothing is hidden:
            // whenever soundd armed a timer the distance from that prediction
            // is the sample, however large.
            //
            // **And it is recorded in two halves**, split at the oldest
            // record's ISR timestamp: everything before it is the device
            // failing to complete when it was due, everything after it is
            // soundd failing to run once it had. `WorstWake` is where that
            // distinction is argued; here it costs one subtraction, because
            // both instants were already in hand.
            if let Some(t_est) = armed_on {
                let t_est = t_est as u64;
                // Clamped to the grid point, which keeps the identity
                // `irq_late + pickup == wake_lat` exact: an interrupt that
                // landed *before* it was due was not late by any amount, and a
                // soundd that is nonetheless late here slept through it, so all
                // of the overshoot is the pickup.
                let irq_at = records[0].timestamp_nanos.max(t_est);
                stats.wake(
                    seen_at.saturating_sub(t_est),
                    irq_at.saturating_sub(t_est),
                    seen_at.saturating_sub(irq_at),
                    wake_completions,
                );
            }
            if !streams.is_empty() {
                stats.completions += wake_completions;
                stats.max_batch = stats.max_batch.max(wake_completions);
            }
        } else if armed_on.is_some() {
            // Armed on a grid point, woken, and the device had produced
            // nothing. Counted rather than ignored: a run of these is the only
            // evidence that separates a device that went quiet from a soundd
            // that overslept, and both arrive as the same large lateness on
            // the wake that finally carries a record.
            stats.empty_wake();
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
pub(crate) fn null_sink_thread(
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
            stats.wake_on_software_grid(lateness);

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
