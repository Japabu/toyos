# ToyOS Audio Subsystem — Technical Specification

## 1. Goals

- Server-pull architecture: a DLL-synchronized timer drives all scheduling, not raw hardware interrupts
- Multiple simultaneous audio clients with real-time additive mixing
- Per-client sample rate, channel count, and format — soundd resamples and converts during mixing
- End-to-end latency under 5ms (target: 2.67ms at 128 frames / 48 kHz)
- One data copy in the hot path (the mix operation, including any resampling)
- f32 mix bus with TPDF dither on final i16 quantization — audiophile-grade signal path
- Deadlock-free by design. Missed deadlines produce silence, not stalls
- Glitch-free stream transitions via gain ramping on connect and disconnect
- Client API through cpal
- Hardware-agnostic: kernel driver abstraction supports VirtIO sound, HDA, USB audio, and future hardware

## 2. Architecture

```
┌─────────┐  ┌─────────┐  ┌─────────┐
│ Client A │  │ Client B │  │ Client C │   applications (linked against cpal)
│  48 kHz  │  │ 44.1 kHz │  │  48 kHz  │
└────┬─────┘  └────┬─────┘  └────┬─────┘
     │signal       │signal       │signal
     ▼             ▼             ▼
┌─────────────────────────────────────────┐
│              soundd                      │  userspace daemon
│                                          │
│  DLL timer fires at predicted period     │
│  reads each client's filled slots        │
│  resamples if client rate ≠ device rate  │
│  mixes into f32 accumulator              │
│  TPDF dither → quantize to i16          │
│  submits DMA buffer to kernel            │
│  DLL adjusts timer from completion time  │
│  signals all clients                     │
└────────────────────┬────────────────────┘
                     │ audio syscalls
                     ▼
┌─────────────────────────────────────────┐
│              kernel                      │
│  audio driver abstraction                │
│  DMA buffer management                   │
│  completion notification via io_uring    │
└────────────────────┬────────────────────┘
                     │
                     ▼
              ┌─────────────┐
              │  Sound Card  │  VirtIO, HDA, USB audio, etc.
              │   48 kHz     │  (fixed native rate)
              └─────────────┘
```

The clock flows upward: hardware completion timestamps → DLL → timer → soundd → clients. Clients never initiate; they respond. The device runs at a single native rate; soundd bridges the gap for clients at other rates.

## 3. Timing Model

The device operates at a fixed native configuration determined by hardware capabilities. All timing is derived from the device's native rate and period size.

Reference device configuration:

| Parameter | Value |
|---|---|
| Native sample rate | 48000 Hz |
| Channels | 2 (stereo) |
| Sample format | i16 (2 bytes) |
| Frame size | 4 bytes |
| Period frames | 128 |
| Period bytes | 512 |
| Period duration | 2.667 ms |
| In-flight DMA buffers | 8 |
| Pipeline depth | 8 × 2.667 ms ≈ 21 ms |

Period duration is the cycle time — how often soundd wakes, mixes, and submits. Pipeline depth is the total DMA buffering that absorbs scheduling jitter. Pipeline depth does not add to perceived latency in interactive applications; latency is governed by the period.

These values are negotiated at stream setup time based on hardware capabilities. The architecture does not assume specific values.

Clients may operate at different sample rates. Their per-period frame count is derived from the device period:

```
client_period_frames = ceil(device_period_frames × client_rate / device_rate)
```

For example, a 44100 Hz client when the device runs at 48000 Hz with 128-frame periods: `ceil(128 × 44100 / 48000) = 118 frames`. The client fills 118 frames per cycle; soundd resamples to 128 frames during mixing.

## 4. Shared Memory Protocol

Each connected client gets a shared memory region mapped into both the client's and soundd's address spaces. The protocol uses a small slot ring with atomic indices — no futex, no byte-level cursors, no blocking synchronization.

### 4.1 Layout

```
Offset      Size                        Field
──────      ──────────────────────────  ─────────────────────────────────
0x00        4                           write_idx (AtomicU32): next slot client will fill
0x04        4                           read_idx  (AtomicU32): next slot soundd will read
0x08        4                           slot_count (u32): N (number of slots)
0x0C        4                           reserved (alignment)
0x10        N × client_period_bytes     slot[0..N]: PCM audio data
```

Slot size is per-client: `client_period_bytes = client_period_frames × client_frame_size`. This depends on the client's negotiated sample rate, channel count, and format. soundd computes the slot size and total shared memory allocation at stream open time.

Available for soundd to read: `write_idx - read_idx`. Available for client to fill: `slot_count - (write_idx - read_idx)`.

Default slot count is 4 (configurable per-client by soundd at stream open). This provides 3 periods of lookahead — enough to absorb typical scheduling jitter and interrupt coalescing where 2-3 DMA completions arrive in a single wake cycle.

Total per client: 16 + N × client_period_bytes. At the reference configuration with 4 slots of 512 bytes, this is 2064 bytes — fits in a single 4 KB page.

### 4.2 Slot Ownership

Slots between `read_idx` and `write_idx` are owned by soundd (filled, awaiting consumption). Slots between `write_idx` and `read_idx + slot_count` are owned by the client (empty, available to fill). Indices wrap modulo `slot_count`.

The client advances `write_idx` after filling a slot. soundd advances `read_idx` after consuming a slot. Since only one side writes each index, no lock is needed.

### 4.3 Steady-State Cycle

```
         time ──────────────────────────────────────────────────────────►

timer:   ┌──fire──┐                                    ┌──fire──┐

soundd:  wake ── signal clients ── wait ── consume ── mix ── submit ── sleep
                 │                  │       │
                 │                  │       │ read slots, resample + mix each
                 │                  │       │
client:          │          signal ── wake ── fill slots ── done
                 │                  ▲ priority inheritance: client
                 │                  │ preempts other threads during fill
                 └──────────────────┘
```

soundd signals clients **before** reading their slots, then waits briefly (one period duration) for them to fill. The kernel's priority inheritance mechanism ensures that when soundd (running at real-time priority) blocks on the client's response, the client thread is temporarily boosted to soundd's priority level. This guarantees the client gets scheduled immediately — even on a single CPU with many runnable threads — and fills its slots within the deadline.

Each cycle, soundd consumes all available client slots (up to however many DMA buffers are free). With 4 slots, the client can fill up to 3 slots ahead. When soundd wakes and finds 2 DMA completions, it consumes 2 client slots — both have real audio, no silence padding needed. If a client misses the deadline, soundd proceeds with whatever is in the ring (possibly silence) and moves on. No client can stall the pipeline.

## 5. soundd

soundd is a privileged userspace daemon. It is the sole owner of the audio hardware clock and the sole submitter of DMA buffers. All audio output flows through soundd.

### 5.1 DLL Timer Scheduling

soundd does not wake directly on DMA completion interrupts. Instead, it maintains a delay-locked loop (DLL) that predicts when each DMA period will complete and sets a timer to fire at the optimal moment.

The DLL works as follows:

1. **Initialization:** after priming the DMA pipeline, soundd sets the timer period to the nominal device period (e.g., 2.667 ms).
2. **On each wake:** soundd reads DMA completion timestamps from the kernel. These are the actual times the hardware finished playing each buffer.
3. **DLL update:** the DLL compares predicted vs. actual completion times and adjusts the timer frequency. A second-order IIR filter smooths the adjustment to avoid oscillation.
4. **Timer reset:** soundd arms the timer for the next predicted completion.

This decouples soundd from interrupt delivery jitter. Even if the kernel batches 2-3 interrupts or delivers them late, the DLL's timer fires at the right moment. The DMA pipeline depth absorbs the timing difference.

The DLL bandwidth (how aggressively it tracks hardware drift) is configurable. A lower bandwidth produces smoother timing at the cost of slower convergence. Default: 0.03 (convergence within ~30 periods).

**Kernel requirement:** the completion notification path must provide a monotonic timestamp for each completed buffer (e.g., the `nanos_since_boot` at interrupt time). Without timestamps, the DLL cannot function and soundd falls back to direct interrupt-driven scheduling.

### 5.2 Startup

1. Open the audio device and read hardware parameters (native sample rate, channels, format, period size, buffer count, DMA buffer offsets)
2. Map the DMA shared memory region
3. Start the PCM stream
4. Prime the DMA pipeline by submitting all buffers filled with silence
5. Initialize the DLL with the nominal period duration
6. Arm the timer for the first expected completion
7. Begin the main loop

Priming starts the completion cycle that drives all subsequent scheduling. The device plays buffer 0, completes it, the DLL observes the completion timestamp, and the self-sustaining timer loop begins.

### 5.3 Main Loop

```
loop:
    wait for events (timer, DMA completion, new client, client messages)

    on timer fire:
        read completion bitmask → add completed indices to free list
        update DLL from completion timestamps
        arm timer for next predicted completion

    on new client connection:
        accept, compute client period size, allocate shared memory,
        create signal pipe, set initial gain to 0.0 (ramp pending),
        add to active clients

    on client disconnect or crash:
        begin gain ramp to 0.0; remove client after ramp completes

    on volume change:
        begin gain ramp to target value

    signal all clients (write 1 byte to each client's signal pipe)
    wait up to one period for clients to fill slots (see §5.10)

    for each free buffer:
        zero the f32 accumulator (silence baseline)

        for each client:
            if slot ring has data (write_idx != read_idx):
                read slot at read_idx
                advance read_idx
                if client_rate ≠ device_rate: resample to device_period_frames
                if client_channels ≠ device_channels: channel convert
                if client_format ≠ device_format: sample format convert
                apply current gain (with ramp interpolation if active)
                mix into f32 accumulator (additive)
            // else: underrun — silence already in place

        apply TPDF dither and quantize f32 accumulator to i16
        write i16 samples to DMA buffer
        submit buffer to kernel
```

The critical ordering: signal **before** read, not after. This gives clients the maximum time to fill their slots. The brief wait after signaling uses priority inheritance (§5.10) to guarantee clients run immediately, even on a single CPU.

### 5.4 Mixing, Dither, and Quantization

**f32 mix bus.** Each client's samples are converted to f32, scaled by the client's gain factor (f32 multiply), and summed into an f32 accumulator. The mix bus operates entirely in f32 — no intermediate integer quantization. This preserves full precision through the gain and summation stages. Integer gain scaling (e.g., `sample * vol / 256`) introduces quantization noise proportional to volume reduction and is not acceptable.

**Sinc resampling.** For clients whose sample rate differs from the device's native rate, soundd resamples the client's audio to the native rate before mixing. The resampler runs per-client and maintains state across cycles to preserve phase continuity. Sinc-based resampling is required (polyphase FIR or windowed-sinc). Linear interpolation introduces aliasing artifacts and is not acceptable for the mix bus.

**Channel conversion.** Mono → stereo: duplicate. Stereo → mono: average. Other mappings follow standard channel layout rules.

**TPDF dither.** After all clients are summed in f32, the output is quantized to i16 for the DMA buffer. Before quantization, TPDF (Triangular Probability Density Function) dither is applied: two independent uniform random values in [-0.5, +0.5] LSB are summed and added to each sample. This eliminates correlated quantization distortion and prevents noise-floor modulation — the noise floor remains constant regardless of signal level.

TPDF dither is the minimum acceptable quality. Noise-shaped dither (e.g., Wannamaker F-weighted, Lipshitz shaped5) may be used for perceptually improved results by shifting quantization noise to less audible frequency ranges. Without dither, signals below -60 dBFS have fewer than 4 effective bits of resolution in i16 and the quantization steps are audible as distortion.

Dither is only applied when the output format has fewer than 24 bits of precision. For f32 or i32 output paths, dither is unnecessary.

### 5.5 Gain Ramping

All gain changes — including initial connection, disconnection, and volume adjustments — are applied as smooth ramps over a short duration rather than instantaneous steps. This prevents clicks and pops caused by discontinuities in the output waveform.

- **On connect:** client's gain starts at 0.0 and ramps to the target gain over ~5 ms (~2 periods at the reference configuration).
- **On disconnect:** client's gain ramps from current to 0.0 over ~5 ms. soundd removes the client only after the ramp completes. During the ramp, soundd reads the client's last filled slot or silence.
- **On volume change:** gain ramps from current to target over ~5 ms.

The ramp is linear and applied per-sample within each period: at the start of the period, gain is at the ramp's current value; at the end, it has advanced by `(target - current) × period_frames / ramp_frames`. soundd interpolates linearly across the period.

### 5.6 Volume Control

Each client has a gain factor (linear, range 0.0 to 1.0, default 1.0). soundd applies the gain during the mix phase by scaling each sample before accumulation. Volume changes always go through the gain ramp (§5.5). Clients adjust their gain via `MSG_STREAM_SET_VOLUME` (see §7.4).

### 5.7 Signal Pipe

soundd creates a unidirectional pipe for each client during stream setup. soundd retains the write end and sends the read end to the client in `MSG_STREAM_OPENED`. Each cycle, soundd writes 1 byte to each client's pipe **before** reading client slots. The client blocks on the read end until signaled.

The signal pipe participates in priority inheritance (§5.10). When soundd writes to the pipe and then waits for the client to fill slots, the kernel temporarily boosts the client thread's priority to match soundd's real-time priority. This ensures the client is scheduled immediately, even on a single-CPU system with many runnable threads.

If a client crashes, the read end closes and soundd's next write returns a broken-pipe error. soundd uses this as the crash-detection mechanism.

### 5.8 Idle Behavior

When no clients are connected, soundd either continues submitting silence (zero connection latency, near-zero CPU) or quiesces the device and re-primes on first connection (zero idle CPU, small connection latency). Either policy is acceptable.

### 5.9 Pipeline Recovery

If all DMA buffers drain due to a catastrophic scheduling stall, soundd detects a full free list and re-primes the pipeline with silence. The DLL re-initializes from the first new completion timestamp. Audio resumes on the next cycle.

### 5.10 Synchronous Client Scheduling

The audio subsystem must produce glitch-free audio on a single-CPU system running concurrent workloads (e.g., a game with rendering, audio, and MIDI threads). This requires that client audio threads are scheduled deterministically within each mix cycle, not left to compete for CPU time at normal priority.

**The problem:** in a naive signal-after-read model, the client thread runs at normal priority and may not be scheduled for tens of milliseconds when the CPU is contended. No amount of ring buffer depth fully compensates — deeper buffers add latency and only delay the problem.

**The solution:** soundd signals clients before reading, then briefly waits. Two kernel mechanisms make this work:

1. **Thread priority for soundd.** The mix thread runs at real-time priority (above all normal threads). The kernel scheduler must support at least two priority bands: real-time and normal. soundd's mix thread is the only userspace thread in the real-time band. This ensures soundd always preempts game logic, rendering, and other normal-priority work.

2. **Priority inheritance on pipe blocking.** When a real-time thread writes to a pipe and then blocks reading from another fd (or polls with a short timeout), the kernel identifies which threads are blocking on the read end of the signaled pipes. Those threads temporarily inherit the writer's real-time priority for up to one period duration. This is the same mechanism Linux uses for `PTHREAD_PRIO_INHERIT` mutexes and `PI-futexes`, applied to pipes.

**The cycle with priority inheritance:**

1. soundd (RT priority) wakes on DLL timer
2. soundd reads DMA completions, updates DLL
3. soundd writes 1 byte to each client's signal pipe
4. soundd calls a short blocking wait (one period, ~2.9ms)
5. Kernel boosts each client thread that is blocked on its pipe read end to RT priority
6. Client threads wake immediately, preempting game logic / rendering / MIDI
7. Client threads fill one or more slots in the ring, then block on the pipe again (dropping back to normal priority)
8. soundd's wait returns (either the timeout expires or all clients have filled)
9. soundd reads slots, mixes, dithers, submits, loops

On a single CPU, steps 6-7 happen sequentially — each client fills its slots and blocks, yielding the CPU to the next boosted client or back to soundd. The total client fill time is bounded: N clients × callback_time. As long as this fits within one period, no underruns occur.

**Fallback without priority inheritance:** if the kernel does not yet support priority inheritance on pipes, soundd falls back to a larger slot count (8 slots) to absorb scheduling jitter. This is a degraded mode, not the target design. The spec assumes priority inheritance is implemented.

**Kernel requirements:**

- Scheduler support for at least two priority bands (real-time and normal)
- A mechanism for soundd to request real-time priority (e.g., a syscall or a capability flag)
- Priority inheritance on pipe wait: when a RT thread signals a pipe, threads blocked on that pipe's read end are temporarily boosted

## 6. cpal

cpal is the client-side audio library. Applications link against cpal and provide an audio callback. The ToyOS backend adapts the server-pull protocol to cpal's callback interface.

### 6.1 Stream Creation

1. Connect to soundd via the service registry
2. Send `MSG_STREAM_OPEN` with the requested sample rate, channels, and format
3. Receive `MSG_STREAM_OPENED` from soundd containing the shared memory token, signal pipe read fd, the client's negotiated period size in frames, and the slot count
4. Map the shared memory
5. Spawn the stream thread

### 6.2 Stream Thread

```
loop:
    read 1 byte from signal pipe (blocks until soundd signals)
    // priority inheritance: this thread is boosted to RT while filling

    while slot ring has space (write_idx - read_idx < slot_count):
        slot_index = write_idx % slot_count
        invoke user callback → callback writes client_period_frames into shm.slot[slot_index]
        write_idx.store(write_idx + 1, Release)

    // thread blocks on next pipe read → drops back to normal priority
```

The user callback receives a mutable slice pointing directly into the shared memory slot, sized to `client_period_frames`. The callback writes PCM samples at the client's own sample rate and format — zero intermediate copies between the application and the shared buffer. soundd handles any rate or format conversion.

The client fills as many slots as available on each wake. With the default 4-slot ring, the client can fill up to 3 slots ahead (one is always being read by soundd). This provides buffering headroom for cycles where soundd consumes multiple slots due to batched DMA completions.

When priority inheritance is active (§5.10), the client thread runs at real-time priority from the moment it wakes on the signal pipe until it blocks on the next pipe read. This window is just long enough to invoke the callback and fill slots — typically under 1ms. The client thread does not hold RT priority while idle.

### 6.3 Callback Deadline

The callback must complete within one device period (~2.67 ms at the reference configuration) per slot. With priority inheritance, the client thread is guaranteed to be scheduled immediately when signaled, so the deadline is achievable even on a single CPU under heavy load.

If a client's callback exceeds the deadline, soundd's wait expires and it proceeds with whatever slots are filled. The signal pipe accumulates an unread byte and the next read returns immediately — the client catches up on the next cycle. soundd mixes silence for any missing periods. A consistently slow callback produces underruns for that client only; other clients and the pipeline are unaffected.

### 6.4 Pause and Resume

- **Pause:** the stream thread stops reading from the signal pipe. soundd sees an empty slot ring each cycle and mixes silence. No explicit coordination required.
- **Resume:** the stream thread resumes reading. It picks up on the next signal.

### 6.5 Teardown

1. Send `MSG_STREAM_CLOSE` to soundd
2. Join the stream thread
3. Unmap shared memory

## 7. Connection Protocol

### 7.1 Stream Open

1. Client connects to soundd via the service registry
2. Client sends: `MSG_STREAM_OPEN { sample_rate, channels, format }`
3. soundd computes the client's period size: `ceil(device_period_frames × client_rate / device_rate)`
4. soundd allocates shared memory (header + slot_count × client_period_bytes), creates a signal pipe, adds the client to the active list with gain 0.0
5. soundd responds: `MSG_STREAM_OPENED { shm_token, signal_pipe_read_fd, client_period_frames, slot_count, device_sample_rate, device_channels }`
6. Client maps the shared memory and begins its stream thread
7. soundd begins gain ramp from 0.0 to 1.0 (§5.5)

soundd allocates the shared memory because it knows both the device parameters and the client's requested format, which together determine the slot size. The slot count is determined by soundd based on system load and the client's latency class (default: 4).

### 7.2 Stream Close

1. Client sends: `MSG_STREAM_CLOSE`
2. soundd begins gain ramp from current to 0.0 (§5.5)
3. After ramp completes: soundd removes the client, unmaps shared memory, closes signal pipe write end
4. Client unmaps shared memory

### 7.3 Client Crash

soundd detects a broken signal pipe on the next write. It begins a gain ramp to 0.0 and removes the client after the ramp completes. If the slot ring still has data, soundd drains it during the ramp to avoid a hard cut. If the ring is empty, the ramp applies to silence (inaudible). No other clients are affected. Since soundd never blocks on client state, a crashed client cannot stall the audio pipeline.

### 7.4 Volume Control

Client sends: `MSG_STREAM_SET_VOLUME { gain: f32 }` where gain is a linear multiplier in the range [0.0, 1.0]. soundd begins a gain ramp from the current value to the target (§5.5). Values outside the valid range are clamped.

## 8. Format Negotiation

The audio device has a native format determined by hardware capabilities (sample rate, channels, sample format). soundd operates at the device's native format.

Clients request their preferred format in `MSG_STREAM_OPEN`. If the requested format matches the native format, the shared memory slots contain raw PCM at that format and no conversion is needed.

If the requested format does not match, soundd performs the conversion during the mix phase:

- **Sample rate mismatch:** soundd resamples the client's audio. The client's slot size holds one period at the client's native rate. soundd converts to the device rate during mixing.
- **Channel count mismatch:** soundd up-mixes or down-mixes during the mix phase (mono → stereo duplication, stereo → mono averaging).
- **Sample format mismatch:** soundd converts (e.g., f32 → the internal f32 mix bus is a no-op; i16 → f32 widening) before mixing.

If a requested format is not supported by the implementation, soundd rejects the stream with an error. The client receives a stream-build error and must retry with a supported format or fail gracefully. soundd must never silently accept a mismatched format and produce garbled audio.

## 9. Kernel Audio Driver Abstraction

The kernel exposes audio hardware to userspace through a uniform interface, independent of the underlying hardware.

### 9.1 Interface to Userspace

- **Device open:** returns hardware parameters — native sample rate, channels, format, period size, number of DMA buffers, buffer offsets within a shared DMA memory region
- **Start / Stop:** begin or cease hardware playback
- **Buffer submit:** userspace submits buffer N (index + byte length) for playback
- **Completion notification:** kernel signals userspace via io_uring poll when one or more buffers finish playback. Userspace reads a completion record containing the buffer index bitmask and a monotonic timestamp (nanos_since_boot at interrupt time) for DLL synchronization.
- **DMA shared memory:** a physically contiguous memory region mapped into both kernel and userspace, containing the DMA buffers. soundd writes mixed audio directly into these buffers before submission.

### 9.2 Driver Trait

Each sound card driver implements:

| Operation | Description |
|---|---|
| **configure** | Set sample rate, channels, format, period size, buffer count |
| **start** | Begin playback |
| **stop** | Cease playback |
| **submit_buffer** | Enqueue a filled DMA buffer for playback |
| **poll_completions** | Return completion record: bitmask of finished buffers + timestamp |
| **interrupt** | Hardware interrupt handler; records timestamp, sets pending flag consumable via io_uring |

### 9.3 DMA Buffer Configuration

Period size and buffer count are negotiated at stream setup based on hardware capabilities. The architecture works with whatever buffer count the hardware provides; a recommended minimum of 8 in-flight buffers provides sufficient pipeline depth to absorb scheduling jitter at low-latency period sizes.

The kernel allocates physically contiguous DMA memory and maps it into the soundd process. Buffer indices and offsets are communicated to soundd at device open time.

### 9.4 Scheduling Requirements

The kernel scheduler must support the following for correct audio operation:

**Real-time priority band.** The scheduler must support at least two priority bands: real-time and normal. Threads in the real-time band always preempt threads in the normal band. soundd's mix thread runs in the real-time band. All other userspace threads (including game loops, rendering, MIDI synthesis) run in the normal band.

A mechanism must exist for soundd to elevate its mix thread to real-time priority. This can be a syscall (e.g., `sys_set_thread_priority(RT)`), a capability flag on the process, or a kernel-recognized service name. The mechanism should not be available to unprivileged applications.

Equivalent systems: Linux `SCHED_FIFO` / `SCHED_RR`, Windows MMCSS "Pro Audio" task, macOS Mach `THREAD_TIME_CONSTRAINT_POLICY`.

**Priority inheritance on pipes.** When a real-time thread writes to a pipe and a normal-priority thread is blocked reading from that pipe, the blocked thread must be temporarily promoted to the writer's priority level. The promotion lasts until the promoted thread blocks again (e.g., on its next pipe read). This ensures the client audio thread is scheduled immediately when soundd signals it, even if the CPU is fully loaded with normal-priority work.

Priority inheritance is critical for single-CPU operation. Without it, the client thread competes with the game loop for CPU time at equal priority, leading to scheduling delays of 30-40ms and massive audio underruns. With it, the client thread preempts the game loop, fills one buffer (~0.5ms of work), and yields — total additional latency is negligible.

Equivalent systems: Linux `PTHREAD_PRIO_INHERIT` on mutexes, Linux PI-futexes, Darwin Mach priority donation.

**io_uring event delivery.** When a DMA completion arrives via interrupt, the kernel must post a CQE to any io_uring instance that has a pending poll on the audio fd. The CQE must be posted before the blocked thread re-enters the blocked pool (see the io_uring race condition described in the implementation notes). This ensures soundd wakes promptly on hardware completion events, not just on DLL timeout.

## 10. Failure Modes

| Failure | Behavior | Recovery |
|---|---|---|
| Client callback exceeds deadline | soundd mixes silence for that client's missing slots | Automatic next cycle |
| Client crashes | Gain ramps to 0, client removed after ramp | Other clients unaffected |
| DMA completions batch (2-3 per wake) | soundd consumes multiple client slots per cycle | Slot ring absorbs batching |
| soundd scheduling jitter | DLL timer + pipeline depth absorbs jitter | Automatic |
| All DMA buffers drain | soundd re-primes with silence, DLL re-initializes | Audio resumes next cycle |
| No clients connected | soundd submits silence or quiesces | Zero overhead |
| Hardware error | Driver reports error to soundd | soundd logs and attempts re-init |
| Client connects | Gain ramps from 0 to target | No click or pop |
| Client disconnects | Gain ramps to 0 before removal | No click or pop |
| Single CPU under heavy load | Priority inheritance boosts client threads during fill | Client preempts game loop, fills in <1ms |

No failure mode produces a deadlock or permanently stalled state. The worst case is a brief audible artifact, after which the system self-recovers on the next DMA cycle. Stream transitions (connect/disconnect) are glitch-free by design. Single-CPU operation is a first-class target, not a degraded mode.