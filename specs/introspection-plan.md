# ToyOS Introspection — Technical Specification

> Owner's scope decisions, 2026-08-03 (task #106). Four diagnostic tool families
> in toybox — **hw**, **disk**, **net**, **audio** — plus **log-follow**, all
> thin clients of one uniform surface. Explicitly excluded: `kill` and `top`.
> Adopt/format is in scope at one depth and one only: *adopt by explicit name*.
>
> **Spec only.** This adds syscalls, and the ABI rule is designed-before-built.
> Nothing here is implemented; this file is the discussion artifact.

## 0. The rule this is written under

A diagnostic that invents is worse than no diagnostic. Every field in every
record below has to come from state the kernel or a daemon actually holds, and
§1.1 is the list of places where it does not hold it yet — because the retention
work, not the syscall, is most of this.

Three consequences that shape everything after:

- **A query never talks to a device.** PCI config space is memory behind ECAM
  and reading it is a load; an NVMe Identify or a USB INQUIRY is a *command*,
  which would put a device round trip inside a syscall holding
  `xhci::XHCI` — the lock `poll_if_pending` wants at the top of every scheduler
  pass. So device identity strings are captured **at bind time** and reported
  from a copy. Anything that cannot be captured at bind is not reportable.
- **A read surface may not mutate.** `SYS_QUERY` is total and pure. The one
  irreversible operation in this document gets its own syscall (§4), because a
  signature that can destroy a disk while claiming to answer a question is the
  worst version of "code must not lie about itself".
- **Nothing here may add work to the logging hot path or to `idle_loop`.**
  `specs/issues/boot-media/log-flush-is-unbounded.md` measures `log_file`'s
  flush at **2.0–9.7 ms** against a
  **23.219 ms** DMA pipeline, in front of `pass()`. A follower that wakes a task
  from `write_chunk` — which runs from ISRs under arbitrary kernel locks — is
  not a feature that needs tuning; it is one that must not exist. §3 is designed
  around that and says so at each step.

---

## 1. The kernel query surface

### 1.1 What the kernel holds today, and what it throws away

Read out of the tree, not assumed. This table *is* the W1 work item list; the
syscall is the small part.

| Subject | Held today | Discarded today |
|---|---|---|
| PCI | `PciDevice { mmio, bus, dev, func }`, `pci::enumerate` ceiling `MAX_DEVICES = 256` | **The whole list.** `main.rs:349`'s `Vec<PciDevice>` is a local in `kernel_main`, and so is the `ecam: Mmio`. Vendor/device/class are never stored — `print_device` materialises them into the log ring and drops them |
| Driver binding | `XhciController.pci`, kept only so log lines can name the controller | **Which driver took which device, everywhere else.** `NvmeController` and `VirtioDevice` drop their `PciDevice` copy |
| USB | `Port { attached, slot, work }` per port, `HidDevice { slot_id, port_idx, role, failures, broke_with }`, `MscDevice { blocks, logical_block_bytes, failed }`, all under `XHCI: Lock<Vec<XhciController>>` | INQUIRY vendor/product (read into a stack buffer, logged, dropped); `HidType` (the descriptor's own claim, collapsed to `HidRole` at bind). No public walker for `devices` or `ports` — every existing one is storage-only |
| Block devices | The `BlockDevice` trait; NVMe behind `page_cache::BLOCK_DEV`, USB minted per call by `usb_storage::open(index)` | **Any registry.** "All disks" is a special case per backend today. `DeviceId` is `1` for NVMe and `16 + index` for USB, with no names |
| Disk identity | capacity, logical block size | NVMe Identify Controller is *issued and thrown away* — no model, serial or firmware revision survives |
| Partitions | `gpt::{FIRMWARE, LOG_GUID, RESOLVED}`; two `Volume`s | **The table.** `disk_guid`, `type_guid`, `index`, `used_entries` and every non-matching entry are logged and dropped. `toyos-gpt` has no list API and never parses the 72-byte UTF-16 name |
| Mounts | `Vfs.mounts: HashMap<String, Box<dyn FileSystem>>` | **Enumerability, and everything but the name.** No iterator; no backing device, no extent, no kind. `mount → device` is reconstructible for `boot`/`log` only, via `Role::volume()` |
| Log sink | `Sink { file_id, size, blocked_since }`, `FILE_OWED`, `FILE_DROPPED`, `DROPPED_BYTES`, `OWED` | Flush duration — the number `specs/issues/boot-media/log-flush-is-unbounded.md` had to measure by hand |
| Input sources | `IN_USE: [AtomicU64; 4]`, `BUTTONS: [AtomicU8; 256]`, `HELD: Lock<[u64; 4]>`, `layout_name()` | **The reverse map.** `IN_USE` says which source numbers are live and nothing about who owns them; the join to `HidDevice.role` needs a walk of `XHCI` |
| Clock | TSC period in `TSC_PERIOD_FS`, `calibrated()` | TSC frequency in Hz (logged at init, never stored); the HPET mapping (a local in `clock::init`); any realtime↔monotonic offset |

Two of these are already filed as defects and this work closes them rather than
working around them: `specs/issues/kernel/` records that
`specs/device-test-strategy.md` **requires a `query-pci` verification that
exists nowhere**, and that no test compares what QEMU was told to create against
what the guest enumerated. There is nothing to compare against because the guest
throws its enumeration away. `SYS_QUERY(PciDevices)` is the missing instrument,
and that — not tool convenience — is the strongest argument for building it.

The second is CLAUDE.md's own PCI paragraph: *"a first-match helper hides that
choice, which is how a machine with two identical controllers ends up with one
driven."* The choice is visible at the call site to someone reading the source.
It is invisible on the machine. A bind record makes it visible where it matters
— `nvme.rs:418`'s `find` versus `xhci/mod.rs:1848`'s `filter` becomes a column
in `hw`, on the laptop, at the moment it is wrong.

### 1.2 The one contract

```rust
pub const SYS_QUERY: u64 = 97;

/// Ask the kernel to enumerate one topic into `buf`.
///
/// Contract, identical to `SYS_READDIR`'s: `Ok(n)` with `n <= buf.len()` means
/// the whole answer is in `buf`; `n > buf.len()` means **nothing was written**
/// and `n` is the size to retry with. A partial answer is never produced.
pub fn query(topic: u64, buf: &mut [u8]) -> Result<usize, SyscallError>;
```

`sys_readdir` is the only one of the kernel's four variable-length syscalls whose
contract is both correct and self-describing, and its doc comment records the
truncation defect it and `getcwd` were fixed for. The other three are not
patterns to copy and one of them is a live wart:

- **`sys_sysinfo` truncates silently.** It returns bytes *written*, breaks out of
  the entry loop at `max_entries`, and a caller can detect the loss only by
  comparing header field `@20` against `(ret - 48) / 64` — which nothing
  documents and `ps` does not do. The ABI wrapper then swallows every error as
  `0`.
- **CLOSED — `sys_query_modules` used to lie in its doc comment.** It said it
  "Returns `Err(InvalidArgument)` with the required buffer size encoded";
  `SyscallError` is a fixed set of codes and encodes nothing, so a caller had no
  way to size a retry and no way to learn that was why it failed. It now answers
  `sys_getcwd`'s contract — the return is the length in bytes either way, and
  nothing is written unless all of it fits, which makes an empty buffer a size
  query. A byte length and not a module count, for the reason stated below; the
  records occupy `buf[..records[0].path_offset]`, which is where the count comes
  from without a second number in the return. `query_modules_size` is the gate.
- **`sys_process_stats` is a destructive read** of an exited direct child, once
  (`specs/issues/diagnostics/process-stats-exited-child-only.md`).

The return value is a length in bytes and never a count, because a count cannot
size a retry when the records carry packed strings.

### 1.3 The layout law

One layout, used by `SYS_QUERY` **and** by every daemon status reply (§2). It is
`SYS_QUERY_MODULES`' `ModuleInfo`-plus-packed-strings idiom, generalised and
given the section header it was missing:

```
QueryHeader { magic: u32, section_count: u32, byte_len: u32, as_of_ns: u64 }
  SectionHeader { kind: u32, count: u32, record_size: u32, _pad: u32 }
    [Record; count]
  SectionHeader …
    …
  <packed string/blob area>
```

Every `*_off: u32` in a record is an offset from the start of the buffer, with a
`*_len: u32` beside it. Rules:

- **`record_size` is checked, never trusted to be what the reader expects.** The
  SDK decoder refuses a section whose `record_size != size_of::<T>()` rather than
  reinterpreting it. Everything in this tree is built together, so this can only
  fire on a genuine mistake — which is exactly when a reinterpretation would be
  silent and catastrophic.
- **A reader may skip an unknown `kind`** using `count * record_size`. That is
  what makes the layout survive a tool built one commit behind a daemon.
- **`as_of_ns` is mandatory.** A snapshot that cannot say when it was taken lies
  by omission the moment anything caches it, and soundd's answer (§2.4) is
  genuinely up to one stats window old.

### 1.4 Topic and record type are bound in one place

The failure mode a topic selector invites is `sysinfo`'s: an untyped byte buffer
and offsets written out by hand at each call site (`buf[pos + 16..pos + 24]` in
`ps.rs`, twice, with a `ppid == u32::MAX` sentinel for good measure). The SDK
kills it by never letting a caller name a topic number:

```rust
pub unsafe trait Topic {
    const ID: u64;
    type Record: Pod;
}

pub fn query<T: Topic>(buf: &mut [u8]) -> Result<Records<'_, T>, SyscallError>;
```

The caller writes `query::<PciDevices>(&mut buf)` and gets `&[PciRecord]` plus a
string resolver. Decoding topic A's bytes as topic B's record is not a mistake
to avoid; it is a thing the caller has no way to express.

### 1.5 The topics

Eight. Each is a struct the kernel already has, or is about to start keeping per
§1.1.

| Topic | Record carries |
|---|---|
| `PciDevices` | `bus/dev/func`, vendor, device, class/subclass/prog_if, revision, `driver: DriverId`, `iommu_stream_id` (`StreamId::pci` already `Display`s as `bb:dd.f`, deliberately so lines can be matched against `pci::enumerate`) |
| `UsbDevices` | controller index, port index, `PortWork` state, slot, `HidRole` or mass-storage, bound input source, MSC block index, `failures`, `broke_with`, vendor/product string offsets |
| `BlockDevices` | `DeviceId`, backend, online, `logical_block_bytes`, `block_count`, model/serial/firmware string offsets |
| `Partitions` | `DeviceId`, GPT index, `disk_guid`, `type_guid`, `unique_guid`, first/last LBA, resolved role (`Boot`/`Log`/`Home`/none), content verdict, adopt witness (§4) |
| `Mounts` | mount name, kind, backing `DeviceId`, extent, read-only, last sync result |
| `LogSink` | `Sink` state (not-installed / active / disabled), path, bytes written, rotations, `FILE_DROPPED`, `DROPPED_BYTES`, `FILE_OWED`, `OWED`, `blocked_since`, last and max flush duration |
| `InputSources` | source number, kind, owner (USB record index, or PS/2), merged button state, held-key count, active layout name |
| `Clock` | TSC Hz, HPET present, calibrated, uptime, wall-clock source and epoch (see below) |

**PCI BARs are deliberately absent from W1.** They are a driver-debugging
detail, not a machine-inventory one, and a six-`u64` array in every record buys
nothing `hw` prints. Add them when something needs them.

**`Clock` is shaped for task #107 and does not pre-empt it.** `SYS_CLOCK_EPOCH`
and `SYS_CLOCK_REALTIME` already exist, so the *time* needs no new syscall; what
is missing is its **provenance**. `rtc.rs` is stateless, hardcodes the century
register at `0x32` with a comment saying "ACPI FADT says 0x32 for most hardware"
— and the FADT is never read for it — and both readers spin on the
update-in-progress bit with no timeout. The topic reports what the source is and
whether it was believed; #107 decides what it should be.

### 1.6 Bounds, and what the caller sees at each

Every bound is a `MAX_*` on the primitive, not a check at a call site, and each
is policy rather than physics.

| Bound | Value | Sits on | Caller sees |
|---|---|---|---|
| `MAX_QUERY_BYTES` | 256 KiB | `sys_query`'s reply builder | `ResourceExhausted`. Not truncation: a machine with more devices than this is a machine the tool must be told about |
| `MAX_PCI_DEVICES` | 256 | `pci::enumerate` (exists) | the existing ceiling, now reported |
| `MAX_QUERY_STRING` | 64 | each captured identity string | truncated **at capture**, at the boundary, with the record carrying the captured length — never truncated at report time |
| existing | `MAX_LIST_ENTRIES` 16 384, `MAX_PATH` 4096 | `vfs` | unchanged; `Mounts` enumerates mounts, not files |

`MAX_QUERY_BYTES` is a policy number and the derivation is honest: the largest
topic is `PciDevices` at `MAX_PCI_DEVICES = 256` records, and 256 KiB leaves
roughly 1 KiB per record against a record measured in tens of bytes. It is not
derived from `mm::MAX_HEAP_ALLOC` the way `MAX_PATH` and `MAX_LIST_ENTRIES` are,
because the reply is built into the caller's buffer and the kernel allocates
nothing proportional to it — that is a property to preserve, not an accident.

### 1.7 What `SYS_QUERY` may never become

`Partitions` reports a partition table. CLAUDE.md's boot-device-identity rule
says the kernel is *given* its partitions and never asked to find any, and that
selecting on a partition's type — or on "the first one of the right format" — is
the defect the rule exists to make unrepresentable. A list API is one refactor
away from being a selection API.

The invariant, and it is structural rather than a matter of care: **no kernel
code path consumes the output of the partition enumeration.** The enumeration
exists to fill a userland buffer. The one kernel path that acts on a partition
identity — §4's adopt — takes a GUID *from userland* and resolves it through
`toyos_gpt::locate(guid)`, the existing by-unique-GUID call, never through the
list. A reviewer checks this by grepping for the enumeration's callers and
finding exactly one, in `sys_query`.

---

## 2. The daemon status protocol

### 2.1 One reserved range, one message pair

The three daemons agree on exactly one thing: `msg_type: u32` in an 8-byte
header. They disagree on everything else — compositor uses 1–11 in each
direction, soundd 1–5, netd 4–22 with `RespType::Result = 128` / `Error = 129`.

`toyos/src/ipc.rs` reserves `0xF000_0000..=0xF000_FFFF` for protocol-independent
messages, and no daemon may allocate in it:

```rust
pub const MSG_STATUS_QUERY:   u32 = 0xF000_0001;   // no payload
pub const MSG_STATUS_REPLY:   u32 = 0xF000_0002;   // §1.3 layout
pub const MSG_STATUS_REFUSED: u32 = 0xF000_0003;   // bare signal
```

The reservation lives beside `MAX_FRAME_LEN` and `MAX_TYPED_PAYLOAD`, on the
primitive, so a daemon that wants a new opcode collides at the SDK rather than
on the wire.

### 2.2 Answering costs a daemon a handful of lines

`toyos/src/status.rs`:

```rust
pub struct StatusWriter<'a> { … }
impl StatusWriter<'_> {
    pub fn section<T: Pod>(&mut self, kind: u32, records: &[T]);
    pub fn string(&mut self, s: &str) -> (u32, u32);   // (offset, len)
}

/// Build a reply into a stack buffer and hand it over in **one** non-blocking
/// write. Never blocks, never partially writes, never allocates.
pub fn answer<F>(conn: &Connection, fill: F) -> Result<(), TrySendError>
where F: FnOnce(&mut StatusWriter);
```

A daemon's dispatch gains one arm:

```rust
ipc::MSG_STATUS_QUERY => {
    if status::answer(&conn, |w| self.status_into(w)).is_err() {
        mark_dead(dead, fd, pid, DropReason::NotReading);
    }
}
```

Four properties, each load-bearing:

- **A slow reader cannot stall a daemon.** `answer` is one `ipc::try_send_bytes`
  and its refusal is `TrySendError::Full`, which the SDK already documents as
  "the connection is no longer at a message boundary — the only correct answer is
  to drop the peer". That is `a45a3ee`/`a24100a`'s rule verbatim, and a status
  endpoint is exactly the kind of endpoint that attracts a client which connects
  and stops reading. Behind a peer that cannot take one frame sit 2,097,088
  bytes of unread messages.
- **The reply is built before any write.** No fill callback runs with a partial
  frame on the wire.
- **`try_send_bytes` puts a `8 + MAX_FRAME_LEN` = 8200-byte buffer on the
  stack.** That is fine once per query and is not a per-frame path. It is *not*
  fine on soundd's mix thread — see §2.4.
- **The 64-byte `MAX_TYPED_PAYLOAD` cap does not apply**, because a status reply
  is bytes, not a typed payload. `MAX_FRAME_LEN` (8192) is the ceiling, and a
  daemon whose status does not fit answers `MSG_STATUS_REFUSED` rather than
  truncating. 8192 against soundd's largest plausible reply — a device section
  plus 63 stream records — is roughly an order of magnitude of headroom.

### 2.3 The client side

`status::ask(&Connection) -> Result<Status, StatusError>` blocks; a *client*
blocking on a server is fine and is what `window::Window` already does. The tool
is a one-shot process.

### 2.4 What each daemon must gain

**compositor — nothing structural.** It is already the shape: `ClientRx`,
`PendingConn`, `DRAIN_BUDGET`, every write a `try_*`, every drop a named
`DropReason` printed with the pid. It answers first, and it is the proof the
helper is a handful of lines. Its sections: screen mode, window count against
`MAX_WINDOW_SLOTS` (221), pending connections against `MAX_PENDING_CONNS` (32),
per-window pid/geometry, frame timing.

**soundd — a snapshot across the thread boundary.** The control thread accepts
and the mix thread owns everything worth reporting: `streams: Vec<ClientStream>`,
`MixStats { wakes, completions, submitted, underruns, drains, max_wake_lat_ns,
max_batch, deferred }`, `free_mask`, `started`, the DLL. The existing
`CommandRing` runs control → mix only.

The answer is a **seqlock published by the mix thread and read by the control
thread**, never a lock the mix thread can wait on. Publish at the stats window
close (`STATS_INTERVAL_NANOS`, 2 s) and on each stream add/remove, not per
period — so the reply is up to 2 s old and `as_of_ns` says so. Two constraints:

- `MixStats` resets every window. The snapshot must carry a **cumulative** copy
  as well, or a query lands mid-window and reports a fraction. Cumulative is the
  right shape for a diagnostic anyway; the windowed report stays as it is,
  because gate A parses it.
- **This is a change to soundd's mix loop, so it gates on gate A's thorough
  tier** (`cargo test --test toyos-build -- --audio-gate 30`), same-session A/B
  against the same HEAD. A publish is a handful of stores at 0.5 Hz and should
  be unmeasurable; "should be" is not a result.

`specs/daemon-testability.md` proposes extracting `mix_pass` behind a
`PeriodSink`. The snapshot belongs *inside* `mix_pass`'s output — `PassOutcome`
already carries `stats_flush: Option<MixStats>` — so the two designs compose
rather than collide, and whichever lands first should leave room for the other.

**netd — the shape first, then the state.** netd's loop still does
`accept` → blocking `recv_header` → handle → close: precisely what `a24100a`
deleted from the compositor. A client that connects and sends four bytes parks
netd's entire event loop, TCP bridging included.

**Adding a status endpoint to netd before fixing that makes an existing DoS
reachable from a diagnostic tool.** So the order is not negotiable: convert
netd to the compositor's shape (`ClientRx`, non-blocking reads, `try_send`,
named drops), *then* give it something to report. What it has to gain is
substantial and honest to state: there is no interface table, no link concept,
no counters. The addresses are hardcoded at `main.rs:1137-1142` (10.0.2.15/24,
gateway 10.0.2.2, DNS 10.0.2.3) and live inside smoltcp's `Interface`; MTU is
hardcoded 1514 rather than taken from the device; and "netd is running" is
currently the only link signal there is, because a NIC-less machine makes netd
print one line and exit. `specs/net-gate-plan.md` wants per-daemon counters for
gate N anyway — the same counters, one consumer earlier.

**sshd** is not in the owner's four families and gets the endpoint for free if it
wants it; nothing here requires it.

### 2.5 Volume: the one write op

Read and write do not share a message type, for §0's reason.

Today volume is **per-stream only**: `Gain(f32)` clamped to `[0.0, 1.0]`,
`GainRamp` over ~5 ms, set by the stream's own owner through
`MSG_STREAM_SET_VOLUME`, coalesced in `ControlClient::pending_volume` because
volume is state and not an event. There is no master gain, no mute, and no way
to read a volume back.

The `audio` tool is not a stream owner, and letting a third party set *another
client's* gain is a new authority that belongs with the capability work, not
here. So the write op is a **master gain** applied in the mix after per-stream
gain:

```rust
pub const MSG_AUDIO_SET_MASTER: u32 = 0xF001_0001;   // f32, soundd-specific
```

- soundd is the authority: it clamps, refuses NaN through the existing
  `Gain::from_wire`, and ramps over the same `ramp_frames`.
- It is reportable — the status reply carries master gain and each stream's
  current and target — which closes "no way to read a volume back" for the thing
  a user means by "volume".
- It is a mix-loop change and gates on gate A's thorough tier.
- Per-stream third-party control is **not** built. Named here so the omission is
  a decision.

---

## 3. log-follow

### 3.1 The two constraints that decide the mechanism

**Nothing may be added to the producer.** `write_chunk` runs from ISRs under
arbitrary kernel locks. A follower that is woken when bytes arrive needs a wake
from there, and `idle_loop` is `drain_serial(); log_file::poll(); pass()` with
the flush already measured at 2.0–9.7 ms in front of the scheduler pass. The
follow path therefore **never blocks in the kernel and never registers for
readiness**: the tool polls on a timer and the syscall is a pure read. That is
not a limitation to remove later; io_uring readiness on this ring would require
exactly the wake that must not exist.

**The follower must not feed itself, and this is not hypothetical.** The ring is
shared with userland console output: `SerialWriter::console` →
`log_ring::write_chunk_blocking`, so every `println!` from any process whose
stdout is the serial console is in the same buffer. `/bin/console` does
`std::io::stdout().lock().write_all(&buf[..n])` on every chunk of its shell's
output (`userland/console/src/main.rs:134,145`) — so on the machine this tool
exists for, a follower's own output re-enters the ring. `--console-boot`'s
residual already records it: *"the seed is read once at startup, because the
console copies the shell's output to its own stdout and that is the ring
`log_file` drains — a tail would feed itself."* Each poll would regenerate the
volume it just read, forever, and `write_chunk_blocking` throttles on the
backend rather than dropping.

### 3.2 The mechanism: a caller-held cursor

```rust
pub const SYS_LOG_READ: u64 = 98;

/// In/out cursor. The kernel reads `seq`, fills `buf` with kernel-origin log
/// bytes from there, and writes back the next position plus what was lost.
#[repr(C)]
pub struct LogCursor {
    pub seq: u64,      // in: where to read from. out: where to resume.
    pub lost: u64,     // out: kernel-origin bytes evicted before `seq`.
    pub oldest: u64,   // out: the oldest sequence still readable.
    pub newest: u64,   // out: the sequence one past the newest byte.
}

pub fn log_read(cursor: &mut LogCursor, buf: &mut [u8]) -> Result<usize, SyscallError>;
```

Three arguments, an in/out struct, the `fstat`/`sched_info` idiom. The SDK
wrapper is what makes it correct:

```rust
pub struct LogTail(LogCursor);
impl LogTail {
    pub fn from_oldest() -> Self;
    pub fn from_newest() -> Self;
    /// Borrows `self` mutably, so a caller cannot read the same window twice.
    pub fn read<'a>(&mut self, buf: &'a mut [u8]) -> Result<LogChunk<'a>, SyscallError>;
}
pub struct LogChunk<'a> { pub bytes: &'a [u8], pub lost: u64 }
```

Why a caller-held cursor rather than a descriptor:

- **The kernel keeps no per-follower state at all.** No `Descriptor` variant, no
  fd to leak, no cursor to go stale, no cost for a second follower. A leaked or
  stale kernel-side cursor is not a bug to avoid — it is unrepresentable.
- `read` takes `&mut self`, so advancing the cursor is not something a caller
  can forget.
- Unlike a `read()` return value, the reply has room to say *what was lost*
  without an in-band marker.

**The stream is not consumed.** The read is `peek_tail`-shaped: it reads
`retained`, the window that only a 64 KiB wrap shortens, and touches neither
`tail`/`len` (serial's debt) nor `file_tail`/`file_len` (the file sink's). Two
followers and the two sinks are four independent readers of one buffer, which is
what `retained` was introduced for.

### 3.3 Kernel-origin only, and the span record

`SYS_LOG_READ` returns **only bytes produced by `log!`**. A userland process
cannot produce a kernel-origin byte, so no chain of userland processes — a
follower, a pipe, a `grep`, the console's echo — can feed a follower. The loop is
unrepresentable at any depth, rather than a rule the tool has to remember.

It costs the ring one record per `append`, which is one per `SerialWriter::spill`
and so roughly one per log line:

```rust
struct Span { end_seq: u64, kernel: bool }
const MAX_LOG_SPANS: usize = 256;
```

- `LogRing` gains `written: u64`, incremented in the loop that already runs, and
  a `Span` ring appended once per `append` — **merged with the previous entry
  when the origin is unchanged**, so a machine logging only from the kernel holds
  one span, and the boot log (measured at 82 lines / 5.9 KB under metal-sim)
  costs nothing.
- The readable window is `retained` intersected with the oldest live span.
  `MAX_LOG_SPANS = 256` against a 64 KiB ring means overflow needs 256 producer
  alternations inside 64 KiB — an average span under 256 bytes. When it happens
  the window shortens and `lost` says by how much. Policy, not physics.
- This adds **nothing** to `idle_loop`, **nothing** to `log_file::poll`, and one
  struct push per line to `write_chunk`, under a lock it already holds.

The cost of the choice, stated plainly: `log-follow` cannot show what another
process printed. That is the right trade — a daemon's own state is what §2 is
for, and scraping a daemon's `printf` was never a good way to ask it anything.

### 3.4 What was rejected

- **The ring as a file (`/dev/klog`).** Attractive: no new syscall, and `cat`,
  `grep` and `>` work. It loses on the read path. `sys_open` produces
  `Descriptor::File(OpenFile)` served through the **file cache**, which would
  cache a live ring; a synthetic stream needs either a magic path special-cased
  in `sys_open` or a third outcome added to the 13-method `FileSystem` trait, for
  one file. Both are surgery on the VFS's shape to avoid one syscall number, and
  neither leaves the loop-prevention anywhere to live — a plain `read()` has no
  argument and no return field for it. Reconsider if a second synthetic stream
  ever appears; one does not justify a `/dev`, and a `/proc` of formatted text
  that something else re-parses is exactly the C-ism this project rejects.
- **A `SYS_QUERY` topic.** The enumerations are all-or-nothing snapshots; the log
  is a resumable partial read. One syscall number with two return contracts is a
  signature that does not describe itself. A cursor argument meaningless for
  seven of eight topics is the sentinel wearing a different hat.
- **Blocking, or io_uring readiness.** §3.1.

### 3.5 In task #95's terms

`specs/metal-log-capture.md` asks how to get bytes off a machine with no 16550,
and answers: xHCI DbC as the primary (gated on one host-side check the owner has
to run), the paged on-screen console as the complement, netconsole and CDC-ACM
rejected. `specs/issues/hardware/kernel-log-unreadable-once-userland-owns-the-screen.md`
records what is still open: every refusal on the path is a `log!` into a ring
whose only sinks are a 16550 the T14 does not have, the on-screen console the
compositor claims tens of milliseconds after the last boot checkpoint, and
`/log` — the thing that had failed. A working desktop and no evidence is the
designed outcome of that arrangement. That is task #95.

`SYS_LOG_READ` adds a **fourth sink, and the first one that needs no hardware and
no host**: any process on the machine can read the retained ring. It is not a
substitute for DbC — it cannot report a wedge before userland, and DbC's whole
argument is that it exists before storage — but it is the piece that turns
"a working desktop and no evidence" into "a working desktop and `log-follow`".
It also closes `/bin/console`'s residual directly: the console currently seeds
its scrollback once from `/log/kernel.log` and its own module doc says *"no
syscall reads the kernel's ring and adding one is not this program's call."*
With this syscall the console tails the live ring into its scrollback and
**never echoes those bytes to its own stdout**, which is what stops the feed
that made a tail impossible. `/log` stops being a prerequisite for seeing this
boot's log, which matters because `/log` is exactly what fails on the boots
worth investigating.

That paragraph is this document's half of the cross-reference; `metal-log-capture`
should gain a matching line under its options list.

---

## 4. Adopting a disk

This section carries the most safety weight in the document and is written to the
same standard as the boot-identity design it extends.

### 4.1 Where consent lives today, and the two things wrong with it

The mechanism exists and is good: `DESIGNATION_MAGIC = b"TOYOS-FORMAT-ME\0"` at
block 0, bytes 16..24 the little-endian block count of the device it designates,
with a `const` assertion that a stamp and a superblock can never parse as each
other. Its own doc states the principle this section keeps: *"there is no way to
designate a disk by accident, and no way to do it without having already decided
to lose its contents."* `probe()` reads block 0 and returns `Ours` / `Designated`
/ `Foreign`, and `Foreign` means nothing is written. The kernel used to format
on a failed mount; removing that is why the T14's first boot did not take the
disk.

Two things do not survive contact with the owner's actual goal — bcachefs
`/home` on the T14's internal NVMe.

**The stamp is written by the host, and it designates a whole device.**
`src/build.rs::designate_for_format` stamps a sparse image file. There is no path
by which a running ToyOS can designate anything, so "adopt a disk from the
machine" does not exist. And the unit is a *device*: `bcachefs_adapter::probe`
reads block 0 of whatever `page_cache` holds, which on the T14 is the whole
NVMe — including its partition table and whatever else is on the laptop.

**A stamp over a used volume does not reformat, and this is reproduced.**
`designate_for_format` writes block 0 only; `Superblock::read` falls back to the
backup superblock at the last block when block 0 does not parse; a stamp does not
parse. So `mount()` succeeds from the backup, `probe()` returns `Ours`, and the
old volume is mounted. "Re-stamp the disk to reformat `/home`" is not a workflow
that works, and `probe`'s doc comment claims the decision comes "from one read of
block 0" when it comes from two and the second wins.

### 4.2 Six invariants

**I1 — The unit of adoption is a GPT partition, named by its unique GUID.**
Never a device, never an index, never "the NVMe". An index is an ordinal that
changes with enumeration order and this tree has already been bitten by ordinals
(the button merge keyed by xHCI slot id; a machine with two identical
controllers). A unique GUID is minted once and never means anything else. It is
the same identity, in the same raw byte order with no conversion on either side,
that `boot_partition_guid` and `log_partition_guid` already use.

**Scope this wave defers — NOT a limit on ToyOS (owner ruling, 2026-08-04):
this wave does not make a blank disk usable, and the infrastructure must never
assume it can't.** ToyOS is a full operating system; erasing a device, writing a
GPT, laying down partitions, and installing alongside another OS on the internal
NVMe (dual-boot beside Windows) are all wanted features, and the consent model
here must extend to them without being redesigned. What this wave builds is the
*narrower* authority — format a partition the disk already names — because it is
the piece HDA and `/home` need first, not because whole-disk authority is
forbidden. The design carries the seam: adoption is expressed against a **device
and a witnessed layout**, so "adopt this whole disk and write it a new GPT" is
the same consent shape with the unit widened from partition to device, and the
witness widened from "this partition in this state" to "this device with exactly
this table (or empty)". The dual-boot case is the north star that keeps the
model honest: writing partitions into a disk's free space must never be able to
touch an extent the witness did not account for — which is *stronger* care than
this wave needs, not weaker, and is why the model is built witness-first now.
CLAUDE.md's rule — *the kernel is given its partitions by the bootloader, never
asked to find any* — governs the **boot** identity, not what an explicitly
adopted device may later become.

**I2 — Naming is necessary and not sufficient: the request carries a second,
independent account.** `KernelArgs` already carries firmware's LBA and length
*beside* the GUID precisely so the kernel has two accounts of one partition, and
*"a disagreement means the table on the disk is not the table firmware read, and
the kernel refuses rather than picking one."* The adopt request carries
`disk_guid`, `partition_guid`, `first_lba` and `block_count`; the kernel resolves
the partition itself and refuses unless every one agrees. A transposed GUID
resolves to nothing. A GUID copied off another machine fails on `disk_guid`. A
stale record from before a resize fails on the extent.

**I3 — A valid request can only be built from an observation the kernel made,
this boot, of that partition, in that state.** `SYS_QUERY(Partitions)` mints a
**witness** per partition: a MAC, under a per-boot random key, over the partition
identity *and* a fingerprint of what the kernel found there (the first block, the
last block, and the content verdict). The adopt call carries it back and the
kernel recomputes it. This makes two things unrepresentable rather than merely
warned about:

- Adopting a partition nobody looked at. There is no way to obtain a witness
  except by asking.
- Adopting a partition whose contents changed between the look and the command —
  a USB stick swapped, a disk that arrived late (the boot stick shares the bus,
  and `xhci-slow-storage-connect` exists because arrival *order* is a real
  machine shape). The fingerprint differs, the witness fails.

The key is per boot, so a witness cannot be carried across a reboot or written
into a script.

**I4 — The kernel refuses to adopt anything it is already using.** It knows
`boot_partition_guid` and `log_partition_guid` and its own mount table. A request
naming the ESP, the log partition, or any partition overlapping either is refused
outright — not confirmed, refused. The single most likely catastrophic mistake
is "the user names the volume he booted from", and it is the one the kernel can
rule out with certainty.

**I5 — Adopt does not go through `probe`, and format writes both superblocks.**
`probe` answers the boot path's question ("is this ours?"). Adopt is a command
("make this ours"). §4.1's second defect exists because the second was expressed
as a mutation of the first's input; separating them removes it rather than
patching around the backup-superblock fallback. The format writes the superblock
at the partition's first block *and* the backup at its last, over a
partition-scoped `BlockIO` clamped to the extent — the adapter's invariant, not
the filesystem's to be trusted about.

**I6 — Adopt changes nothing about the running system.** It does not mount, does
not touch the VFS, does not disturb the tmpfs `/home` this boot is using. Its
only effect on this boot is bytes on a partition the user named. The mount
happens on the **next** boot, through the bootloader-given GUID, on the same code
path every other mount uses. One mount path, not two.

### 4.3 The flow, end to end

```
1. disk list
     → SYS_QUERY(BlockDevices) + SYS_QUERY(Partitions) + SYS_QUERY(Mounts)
     → the tool prints every disk, every partition, its GUIDs, its extent,
       what the kernel thinks is on it, and which ones are in use and why.
       Each row carries an opaque witness the tool keeps and does not display.

2. disk adopt <partition-guid>
     → the tool matches the GUID against exactly one row it just printed;
       an unmatched or ambiguous GUID is refused in userland, before any syscall.
     → it prints what will be destroyed: the disk, the partition, its size,
       the content verdict, and — for a partition the kernel recognises — what
       it recognised. Then it requires the user to type the partition's GUID
       back. Not "yes": the GUID, which cannot be typed by reflex.

3. SYS_DISK_ADOPT(&AdoptRequest, &mut AdoptOutcome)
     → the kernel re-derives everything (I2, I3, I4), formats (I5), and records
       the GUID as `\toyos\home.guid` on the ESP.

4. reboot
     → the bootloader reads `\toyos\home.guid` and carries it in `KernelArgs`
       exactly as it carries `log_partition_guid`; the kernel mounts /home from
       the partition it was given (I6).
```

Step 4 is the part with the least new machinery in it, and that is the point.
`/home` becomes identical in shape to `/log`: **named by the bootloader, never
found by the kernel.** A `KernelArgs` field, a bootloader read, a `gpt::locate`.
The boot-identity design is not extended so much as applied a second time.

The `\toyos\log.guid` precedent also fixes the absent-file semantics for free: no
presence flag and no fallback. A machine with no `\toyos\home.guid` has no
adopted `/home` and gets the tmpfs, which is what a machine whose disk we may not
touch gets today.

### 4.4 The syscall

```rust
pub const SYS_DISK_ADOPT: u64 = 99;

#[repr(C)]
pub struct AdoptRequest {
    pub disk_guid: [u8; 16],
    pub partition_guid: [u8; 16],
    pub first_lba: u64,
    pub block_count: u64,
    pub witness: [u8; 16],
    pub role: u32,           // Home. The only value W6 accepts.
    pub _pad: u32,
}

#[repr(C)]
pub struct AdoptOutcome {
    pub refusal: u32,        // AdoptRefusal
    pub _pad: u32,
    pub found_first_lba: u64,
    pub found_block_count: u64,
    pub gpt_error: u32,      // toyos_gpt::GptError discriminant, or 0
    pub _pad2: u32,
}

pub fn disk_adopt(req: &AdoptRequest, out: &mut AdoptOutcome) -> Result<(), SyscallError>;
```

`role` exists so that adopting a partition for something other than `/home`
later is an added enum value rather than a second syscall, and so that the record
on the ESP is role-named — a mount is named for its role, never its format.

### 4.5 The refusal vocabulary

Everything here crossed the trust boundary, so **nothing on this path panics** —
fail-fast is for kernel bugs, and an `expect()` on a user-supplied GUID is a
userland-triggered kernel panic wearing fail-fast's clothes. `SyscallError` has
nine variants and no room to say *which* check failed, so the `SyscallError` is
the class and `AdoptOutcome::refusal` is the sentence:

| `AdoptRefusal` | `SyscallError` | The tool can say |
|---|---|---|
| `NoSuchDisk` | `NotFound` | no disk with that GUID |
| `NoSuchPartition` | `NotFound` | that disk has no partition with that GUID |
| `GptUnreadable` | `NotFound` | the GPT error by name, out of the 18 `toyos_gpt` variants |
| `ExtentDisagrees` | `InvalidArgument` | "you named N blocks at L; the table says M at K" |
| `WitnessStale` | `InvalidArgument` | the partition changed since you looked — look again |
| `WitnessUnknown` | `InvalidArgument` | that witness was not minted this boot |
| `IsBootPartition` / `IsLogPartition` / `OverlapsInUse` | `PermissionDenied` | which mount, by name |
| `TooSmall` | `InvalidArgument` | the minimum a bcachefs volume needs |
| `WriteFailed` | `ResourceExhausted` | which step, and that the partition is now in an unknown state |

`ExtentDisagrees` returning the found values is the point of the out-struct: *"a
bound's second question is what the caller sees when it is hit"*, and "invalid
argument" is not an answer a person can act on.

### 4.6 What this does not defend against

Said plainly, because overclaiming a safety property is worse than not having it.

- **It is not authorization.** Any process can call `SYS_QUERY` and get a
  witness, and any process can call `SYS_DISK_ADOPT`. The witness defends against
  *mistake* and against *TOCTOU*; it does not defend against a hostile process,
  because ToyOS has no answer to "which processes may do this" — that is
  `specs/capability-handles-spec.md`'s question.
  `specs/issues/boot-media/log-is-userland-writable.md` already records the
  neighbouring hole: `/boot` has no permission model and a guest binary
  truncated `kernel.elf` to five bytes. Adopt does not make that worse and does
  not fix it.
- **It does not make mounting a crafted volume safe.**
  `specs/issues/isolation/bcachefs-untrusted-input-holes.md` lists
  three residual untrusted-input holes in `bcachefs` — an unchecked extent that
  reaches a block read, a `Vec` sized from an on-disk file size, an unchecked
  multiply — and the recommendation to tighten `Superblock::check` from `<=` to
  `==`. Those are orthogonal and should land first; adopting a partition is not
  the operation that mounts a stranger's disk.
- **It does not verify the format afterwards.** A read-back-and-verify pass is
  cheap and worth adding; it is not in W6's minimum.

### 4.7 The blocker to sequence against

Step 3 writes `\toyos\home.guid` to the ESP, and
`specs/issues/boot-media/boot-exists-only-on-a-usb-boot.md` records that
**`/boot` exists only on a machine that boots from USB**: `fat32_adapter::mount`
resolves the volume through `usb_storage::open` and has no second arm, because a
machine that boots from its internal disk has its NVMe taken by
`page_cache::init` and there is no second handle to it.

This does **not** block the owner's case — the T14 boots from a stick, so the ESP
is writable and the NVMe is the disk being adopted. It does block adopt on a
machine that boots from NVMe, and the fix is the same shared-block-device-handle
work that entry names. W6 should state which machine it supports on the day it
lands rather than discovering the difference on the laptop.

---

## 5. The tools

All in `userland/toybox`, all thin, none holding state.

| Tool | Reads | Notes |
|---|---|---|
| `hw` | `PciDevices`, `UsbDevices`, `InputSources`, `Clock` | The bind column is the point. `hw -v` prints unbound devices too |
| `disk` | `BlockDevices`, `Partitions`, `Mounts`, `LogSink` | `disk adopt` is §4 and is the only subcommand that writes |
| `net` | netd status | **Name collision:** `net` is today an HTTP GET demo (`userland/toybox/src/net.rs`). Recommend renaming the demo to `http` and giving `net` its conventional meaning; the owner's call, not this spec's |
| `audio` | soundd status; `audio volume <0..1>` writes | |
| `log-follow` | `SYS_LOG_READ` | ~30 lines: `LogTail::from_oldest()`, read, print, `nanosleep`, repeat. `--since-boot` / `--follow`. A poll interval of 50 ms is a syscall at 20 Hz and is imperceptible against a 64 KiB ring |

`free`, `ps` and `stats` stay as they are. `ps` should stop hand-decoding
`sysinfo`'s byte offsets at some point, but converting `SYS_SYSINFO` to the §1.3
layout is a separate change with its own justification and is not smuggled in
here.

## 6. Waves

| Wave | Contents | Gate | Depends on |
|---|---|---|---|
| **W1** | Retention (§1.1) + `SYS_QUERY` + the SDK `Topic` decoder + `hw` + `disk` (read-only) | `cargo test`; **and the new one**: a harness test comparing QMP `query-pci` against the guest's `PciDevices` — the check `specs/device-test-strategy.md` requires and nothing has ever provided | — |
| **W1b** | `written`/`Span` in `log_ring`, `SYS_LOG_READ`, `log-follow`, console tails the live ring | `cargo test`; a test asserting a `println!` from the follower never appears in its own output | Independent of W1 — disjoint files, can run concurrently |
| **W2** | `toyos::status` + reserved range + compositor answers | `cargo test` | W1 (shares the §1.3 layout law and its decoder) |
| **W3** | soundd seqlock snapshot + `audio` (read-only) | **gate A thorough, `--audio-gate 30`, same-session A/B** | W2 |
| **W4** | netd converted to the `a24100a` shape | `cargo test`; gate N when it exists | W2 |
| **W5** | netd interface/link/counter state + `net` tool | `cargo test` | W4 — not before |
| **W6** | soundd master gain + `audio volume` | gate A thorough | W3 |
| **W7** | `SYS_DISK_ADOPT` + the `\toyos\home.guid` chain + `disk adopt` | A harness test that adopts a scratch disk and refuses the boot stick, plus the negative controls in §4.5. **A test that writes to a disk decides which disk from data on the disk, never from the flag that enabled it** — the boot stick shares the bus | **Owner review of §4 first.** Then W1 (witness minting), and the bcachefs untrusted-input residuals |

W1 and W1b are the natural first cut: they are read-only, they touch disjoint
files, and each is useful alone. W7 is last and gated on review, which is the
owner's instruction and also what the section deserves.

## 7. Three syscalls, and why not two or one

`SYS_QUERY` (97), `SYS_LOG_READ` (98), `SYS_DISK_ADOPT` (99).

- **Why not eight.** The alternative to a topic selector is `SYS_PCI_LIST`,
  `SYS_USB_LIST`, `SYS_DISK_LIST`, `SYS_MOUNT_LIST`, `SYS_LOGSINK_STAT`,
  `SYS_INPUT_LIST`, `SYS_PARTITION_LIST`, `SYS_CLOCK_INFO` — eight dispatch arms
  and eight independent chances to get the buffer contract wrong, against one
  contract checked once. The risk a selector carries is an untyped return, and
  §1.4 removes it by never letting a caller name a topic number.
- **Why `SYS_LOG_READ` is not a `SYS_QUERY` topic.** §3.4. Two return contracts
  behind one syscall number is a signature that does not describe itself.
- **Why `SYS_DISK_ADOPT` is separate.** §0. A read surface that can destroy a
  disk is the defect this document is most concerned with, in its purest form.
- **Why the whole thing is not a filesystem.** A `/proc` of formatted text that
  something else re-parses is a C-ism: the kernel would be writing a string
  whose grammar is an unversioned contract, and every tool would carry a parser
  for it. Records are records. Streams — and there is exactly one — are the case
  where a byte channel is the honest shape, which is why §3.4 reconsiders a file
  and rejects it only on the file cache and the loop, not on principle.

## 8. What this document does not build

- **`kill` and `top`** — excluded by the owner.
- **A live per-process stats query.**
  `specs/issues/diagnostics/process-stats-exited-child-only.md`: `SYS_PROCESS_STATS`
  reports an exited direct child, once, and cannot sample a daemon. That is a
  layer-1 gap in the diagnostics roadmap, it is real, and it is not this task.
  `SYS_QUERY` is deliberately about *the machine*, not about processes; folding
  process accounting into it would make the topic list unbounded.
- **Event tracing and RIP sampling** — layers 2 and 3, unchanged.
- **A permission model for any of this.** §4.6.
- **Partition table writing.** §4.2, I1.
- **Per-stream third-party volume.** §2.5.
