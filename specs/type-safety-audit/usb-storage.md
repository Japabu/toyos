# Independent audit: the USB mass-storage stack

Read-only. Nothing in the repository was changed; no file outside this one was
written. The only command run against the tree was `cargo test -- usb_storage`,
which passed 3/3 and whose serial output is the source of every measured number
below.

Audited at HEAD `42e87a2` (2026-08-01), working tree clean. Per-file HEAD:

| file | commit |
|---|---|
| `kernel/src/drivers/xhci/msc.rs` | `63cacbc` |
| `kernel/src/drivers/xhci/mod.rs` | `606efc9` |
| `kernel/src/drivers/xhci/device.rs` | `63cacbc` |
| `kernel/src/drivers/xhci/legacy.rs` | `755b591` |
| `kernel/src/drivers/usb_storage.rs` | `3c5a7b8` |
| `kernel/src/usb_gate.rs` | `862741d` |
| `kernel/src/block.rs`, `kernel/src/page_cache.rs` | `3c5a7b8` |
| `kernel/src/drivers/pci.rs` | `606efc9` |
| `kernel/src/drivers/nvme.rs` | `63cacbc` |

**Every number below came from a command that was run**, and where a figure is
an estimate or a spec bound rather than a measurement it says so.

---

## Summary

**The hostile-device story is much better than the codebase's average, and most
of the author's claims hold.** Every value a device chooses that becomes a
length, a divisor, a shift or a byte count is checked: block size against a
four-element set, last LBA against `u32::MAX`, residue against the transfer,
CSW signature and tag, short and oversized CSWs, capacity that rounds to zero
blocks. The descriptor walk is bounded three ways and every field is read
through `get`. No allocation anywhere in the stack is sized by a device number.
I built eleven probes looking for the panic; ten came back clean.

The defects are elsewhere, in three shapes:

- **An optional SCSI command's refusal is read as a device failure**, and the
  layer above turns that into a self-sustaining device-write loop from the idle
  loop. This is the one to fix first: it is triggered by an ordinary,
  spec-conformant USB stick, and QEMU structurally cannot produce it.
- **Two waits in the enumeration path have no deadline at all**, and four more
  in controller bring-up. On a machine with no serial these are a silent hang.
  The author's "every wait has a 2 s deadline" applies to `wait_command` and
  `wait_transfer` and not to the six register spins around them.
- **The recovery path issues a command the xHCI spec says is only legal on a
  halted endpoint, on a path reached when the endpoint is running**, and the
  consequence of it failing is that the disk goes offline for the life of the
  boot. Nothing in the suite executes any of that code.

Two doc comments carry numbers that are false. One of them (`MIN_DEVICE_BLOCKS`)
names four scratchpad demands; the real set is 32, and the specific value it
quotes belongs to a variable the code no longer has.

---

## Part 1 — verdicts on the author's claims

| # | Claim | Where | Verdict |
|---|---|---|---|
| 1 | "nothing in this file may panic on it" — no panic on any wire value | `msc.rs:1-8` | **Holds** for `msc.rs`; see F4 for the descriptor that reaches `msc.rs` from `device.rs` |
| 2 | Absurd capacity, zero/odd block size, short/oversized CSW, wrong tag, over-large residue all refuse cleanly | `msc.rs` | **Holds**, all six, verified line by line |
| 3 | "The bound is generous on purpose… nothing but a dead device can reach it" — `USB_TIMEOUT_NS` | `mod.rs:99-106` | **Holds** for what it covers |
| 4 | Every wait has a deadline | prompt's restatement | **Does not hold** — six waits have none (F2) |
| 5 | "Before `clock::init` this is 0 … reachable only by a caller that runs before phase 2" | `mod.rs:108-114` | **Holds** — `clock::init` is `main.rs:336`, `xhci::init` is `main.rs:385` |
| 6 | `MSC_DATA` "cannot cross a 64 KiB boundary" | `mod.rs:231-237` | **Holds** — verified against the logged offsets |
| 7 | `align_2m` is safe here because `max_scratchpad ≤ 1023` | `mod.rs:284-293` | **Holds** — max `dev_base` is 4,390,912 B |
| 8 | `MIN_DEVICE_BLOCKS`: "four of the 1024 possible demands (503, 504, 1014, 1015)… at 504 `dev_base` is exactly 2097152" | `mod.rs:252-258` | **Does not hold** — 32 demands, and none of the four named (F9) |
| 9 | `find` "would still terminate with any two of them removed" | `legacy.rs:16-19` | **Does not hold** (F10) |
| 10 | "nothing here panics, nothing here loops on a number firmware chose" | `legacy.rs:12-19` | **Holds** for `legacy.rs` itself; its *caller* does both (F7, F8) |
| 11 | `READY_BUDGET_NS`: "Boot time is what is being protected, and boot time is what this measures" | `msc.rs:24-32` | **First clause holds, second does not** (F11) |
| 12 | `healthy()`: "answers 'is there still something there', which is what a caller asks after a run of failures" | `usb_storage.rs:45-52` | **Does not hold** (F5) |
| 13 | `BlockDevice` fallibility actually propagates | `block.rs:35-53` | **Holds** — all 18 call sites consume the `Result`; `bcachefs::BlockIO` is the known stop and is already filed |
| 14 | `page_cache::unbind` prevents serving a previous tenant under a new number | `page_cache.rs:184-197` | **Holds** by reading; already filed as having no test that can fail |
| 15 | `pci::enumerate` is bounded and drivers select all matches | `pci.rs:234-277` | **Holds** — `MAX_DEVICES = 256`, `xhci::init` takes every match, `nvme::init` takes the first and says so |
| 16 | The xHCI slot is leaked "on four paths" | `device.rs:225-231`, known-issues | **Undercounted** — 11 exits (F12) |
| 17 | Per-device EP0 rings removed one of hotplug's three obstacles | known-issues | **Holds**, and there are more obstacles left than the record names (F13) |

### The refutations that matter most, stated precisely

**(2) The hostile-device checks all hold.** Each was traced to the value that
would have been the bug:

- *Block size.* `msc.rs:723` `matches!(block_bytes, 512 | 1024 | 2048 | 4096)`.
  Zero would divide at `:737`; anything above 4096 makes `sectors_per_block`
  zero and then divides by *that* at `:738`. Both are refused before either.
- *Capacity.* `:731` refuses `last_lba > u32::MAX` (the driver issues READ(10));
  `:739` refuses `blocks == 0`. **Both verified live** — `Profile::UsbDiskHuge`
  produced `usb-storage: slot 2 has 6442450944 sectors; this driver issues
  READ(10) and addresses 2^32`, and the device did not join `storage`.
- *Short CSW.* `bot` requires `Some((CC_SUCCESS, 0))` from the status transfer
  (`:393`). A 12-byte CSW completes as `CC_SHORT_PACKET` and a 14-byte one as
  Babble; both fall to `_ => return Bot::Broken`.
- *Wrong tag.* `:414`, with the correct reason in the comment.
- *Residue > transfer.* `scsi` guards `residue <= data_len` at `:272` before the
  subtraction at `:273`. Without it `delivered` underflows and every caller
  believes a 4-billion-byte transfer.
- *Partial transfer reported as success.* `:239` refuses it rather than
  truncating, which is the `SYS_READDIR` lesson applied correctly.

**No allocation in the stack is sized by a device number.** `Layout::new` is
sized from HCSPARAMS with the reasoning written out and correct; `msc.rs`
allocates nothing at all; `usb_gate.rs`'s three `vec!`s are sized by kernel
constants. Verified by reading every `vec!`/`with_capacity`/`alloc` in scope.

**(6) The 64 KiB placement claim holds, checked against reality.** The boot log
gives `msc_block +0x10000` and `+0x20000`, so `MSC_DATA` occupies
`0x18000..0x20000` and `0x28000..0x30000` — each exactly the second half of one
64 KiB region. `DmaPool::alloc` takes whole 2 MiB PMM pages, so the physical
base is 2 MiB aligned and the offsets carry through. The rest of the layout
line agrees too: `dev_base` = `0x30000` matches `slot 1 enabled (dma +0x30000)`,
`slot 2` at `+0x33000` matches `DEV_STRIDE = 0x3000`, and
`(2097152 − 196608) / 12288 = 154`, `min(64)` = the logged `device blocks=64`.

**(13) The new fallibility is real and it propagates.** `grep` over
`kernel/src` finds 19 calls to `read_blocks`/`write_blocks`/`flush`; every one
is `?`, `.is_err()`, `match`, or `map_err`. Not one discards the status. The
`#[must_use]` messages are the right ones. This is the claim that was worth
checking hardest — it is the direct fix for the NVMe defect — and it holds.

---

## Part 2 — findings, ranked

### F1 (critical). A stick that does not implement SYNCHRONIZE CACHE turns the idle loop into a permanent device-write loop

**Location.** `msc.rs:157-168` (`msc_flush`), with `msc.rs:262-303` (`scsi`
collapsing two outcomes into one), `fat32_adapter.rs:277-280`,
`fat32_adapter.rs:607-611` (`EspFs::sync`), `esp_log.rs:254`.

**The bug the current shape permits.** `msc_flush` issues SYNCHRONIZE
CACHE(10) and treats anything other than `Scsi::Ok` as failure:

```rust
let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
matches!(ctrl.scsi(dev, &cdb, 10, 0, 0, false), Scsi::Ok { .. })
```

SYNCHRONIZE CACHE is optional in SBC, and a great many USB flash drives do not
implement it — they answer CHECK CONDITION with sense
`0x05/0x20/0x00` (ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE). Linux's
`sd` driver treats exactly that reply as a non-fatal outcome; this driver
treats it as a failed flush.

The consequence is not one bad log line. It is a loop, and every piece of it is
in the tree today:

1. `esp_log::poll` runs from the idle loop. `Sink::flush` writes the pending
   log bytes, then calls `vfs.sync_mount("/boot")` (`esp_log.rs:254`).
2. `sync_mount` reaches `EspFs::sync`, which is `if let Err(e) =
   self.fs.sync() { log!("esp: sync failed: {e}"); }` — it **swallows the error
   into the log ring** and returns `()`.
3. `Sink::flush` therefore returns `Ok(())`, so `esp_log::poll`'s
   disable-the-sink path (`esp_log.rs:198-204`, which fires only on an `Err`
   from `flush`) never runs.
4. The line logged in step 2 is new pending ring content, so the next idle pass
   finds `log_ring::file_has_pending()` true and does the whole thing again.

Each iteration is a real file write plus FAT and directory-entry writes plus a
SYNCHRONIZE CACHE — that is, several USB round trips — issued under
`XHCI.lock()`, which disables preemption. On the T14 this is the state the
machine boots into and never leaves: 100% of one idle CPU, a `kernel.log` that
grows to `MAX_LOG_BYTES` and rotates forever, and known-issues' "the ESP log's
flush is unbounded, uninterruptible, and in front of the scheduler pass"
becoming permanent rather than periodic.

**Why nothing catches it.** QEMU's `usb-storage` implements 0x35. The whole
`cargo test -- usb_storage` run contains zero occurrences of `refused to flush`
or `sync failed`, including on the write-protected LUN, where every WRITE(10)
failed and the flush still succeeded. This is structurally untestable in QEMU,
exactly like the USBLEGSUP handoff.

**The shape statement.** `request_sense` already reads the three bytes that
answer the question — it is called on `Bot::Failed` at `msc.rs:283` — and
throws them into a log line. `Scsi` has two variants where it needs three: the
distinction between "this device refused this command" and "this device
does not have this command" is information the driver *fetches and discards*.

**Proposed shape.**

```rust
/// The completion of one SCSI command, after the transport's own recovery.
enum Scsi {
    Ok { delivered: u32 },
    /// The device understood the command and declined it, with the sense it
    /// gave. Carried rather than logged and dropped: a caller issuing an
    /// *optional* command needs to tell "I will not" from "I cannot", and
    /// those three bytes are the only place that answer exists.
    Refused { key: u8, asc: u8, ascq: u8 },
    /// The transport broke. Nothing about the buffer is known.
    Broken,
}

impl Scsi {
    /// SBC's "this opcode is not implemented". For an optional command this is
    /// an answer, not a failure.
    fn unimplemented(&self) -> bool {
        matches!(self, Self::Refused { key: 0x05, asc: 0x20, ascq: 0x00 })
    }
}

// msc_flush
let outcome = ctrl.scsi(dev, &cdb, 10, 0, 0, false);
// A device with no SYNCHRONIZE CACHE has no write cache this could flush, so
// the writes before it are as durable as they are going to get. Reporting a
// failure here reports the wrong thing, and the caller above turns it into an
// unbounded retry.
matches!(outcome, Scsi::Ok { .. }) || outcome.unimplemented()
```

**What it deletes.** The `log!` in `scsi`'s `Bot::Failed` arm stops being the
only consumer of the sense data, and `request_sense`'s doc comment ("never as a
decision") becomes a statement about the *transport*, which is what it means.

**The other half is not the driver's.** `EspFs::sync` returning `()` and
swallowing into `log!` is what closes the loop; a fix there (return the error,
or at minimum do not `log!` on the path the log itself drives) belongs with
`fat32_adapter.rs`. Both halves are needed: the driver fix removes the trigger,
the adapter fix removes the amplifier.

### F2 (high). Six waits with no deadline, two of them in the enumeration path

**Location.** `device.rs:195-199`; `mod.rs:765-767`, `:770-772`, `:773-775`,
`:825-827`.

**The bug the current shape permits.** The author added `USB_TIMEOUT_NS` to
`wait_command` and `wait_transfer`, which is where a *device* fails to answer.
Six register spins around them have nothing:

```rust
// device.rs:195 — waiting for the port to finish resetting
loop {
    let ps = op_base.read_u32(portsc_off);
    if ps & PORTSC_PRC != 0 { break; }
    core::hint::spin_loop();
}
```

`init_device` is called for every port whose CCS bit is set. A port that
asserts CCS and then never asserts PRC — a device pulled between the scan and
the reset, a marginal cable, a port whose reset the controller refuses — hangs
the boot, on the boot CPU, before the scheduler exists, with no log line. The
four in `init_one` are the same shape against the controller: waiting for
HCHalted, for HCRST to clear, for CNR to clear, and for the controller to leave
Halted after R/S.

This matters more here than it would in most drivers, because of the machine it
is for. `--diag-boot` and the on-screen console give the T14 a way to show its
last boot checkpoint; a wedge inside `xhci::init` shows `Boot: peripherals
ready` and nothing else, forever, and the same wedge is what a dead port and a
dead controller both look like.

**Not hypothetical in the way the others are**: `init_one` already has the
right answer for a controller it cannot drive — `arm_interrupt` returning
`None` logs a per-controller refusal by name and returns `None`, and `init`
then reports how many controllers were present and how many were usable. These
four spins bypass that machinery entirely.

**Proposed shape.** One helper, and the four controller spins become refusals
through the path that already exists:

```rust
/// Spin until `ready`, or give up and say which wait it was. Same budget as a
/// transfer: these are register bits the controller sets in microseconds, and
/// one that never sets is a controller this driver cannot drive.
fn await_bit(op: &Mmio, what: &str, ready: impl Fn(&Mmio) -> bool) -> bool {
    let deadline = deadline();
    while !ready(op) {
        if crate::clock::nanos_since_boot() >= deadline {
            log!("xHCI: {what} did not complete in {}ms", USB_TIMEOUT_NS / 1_000_000);
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

// init_one
if !await_bit(&op_base, "halt", |o| o.read_u32(OP_USBSTS) & 1 != 0) { return None; }
if !await_bit(&op_base, "reset", |o| o.read_u32(OP_USBCMD) & (1 << 1) == 0) { return None; }
if !await_bit(&op_base, "controller-not-ready", |o| o.read_u32(OP_USBSTS) & (1 << 11) == 0) { return None; }
```

and the port reset becomes a skipped port rather than a dead machine:

```rust
let deadline = deadline();
loop {
    let ps = op_base.read_u32(portsc_off);
    if ps & PORTSC_PRC != 0 { break; }
    if crate::clock::nanos_since_boot() >= deadline {
        log!("xHCI: port {} never finished its reset (PORTSC {ps:#010x}); skipping it",
            port_idx + 1);
        return;
    }
    core::hint::spin_loop();
}
```

**What it deletes.** Nothing. It makes `init_one`'s existing "NOT INITIALISED …
No USB device on it can be used" refusal cover the cases it was written for.

### F3 (high). The recovery path issues Reset Endpoint on an endpoint that is not halted, and the disk goes offline for the boot when it fails

**Location.** `msc.rs:458-492` (`clear_stall`), `msc.rs:498-512`
(`reset_recovery`), `msc.rs:293-301` (`scsi`'s `Bot::Broken` arm).

**The bug the current shape permits.** `Bot::Broken` means four different
things — a phase error, a malformed or mistagged CSW, a stall the endpoint
reset did not clear, **or silence** — and `scsi` responds to all four
identically:

```rust
Bot::Broken => {
    if !self.reset_recovery(dev) {
        log!("usb-storage: reset recovery failed; disk is offline");
        dev.failed = true;
    }
    Scsi::Error
}
```

`reset_recovery` calls `clear_stall` on both endpoints unconditionally, and
`clear_stall`'s first step is a Reset Endpoint command. The xHCI spec permits
Reset Endpoint only on an endpoint in the Halted state; on a Running endpoint a
conformant xHC returns Context State Error. So on the *silence* path — the
device NAKed until `wait_transfer` gave up, and the endpoint is still
Running — `run_command` fails, `clear_stall` returns false, `reset_recovery`
returns false, and `dev.failed = true` **permanently**. There is no path that
ever clears it: no re-probe, no reset, no rebind. One transfer that times out
takes the boot disk offline for the life of the boot, which on a machine that
boots off USB means `/boot`, `esp_log`, and every subsequent diagnostic.

The failure is *fail-closed*, which is the right direction — but it converts a
recoverable device into an unrecoverable one, which is exactly what the
author's own comment two lines above warns against for the opposite case:
"leaving one halted because the other step failed turns a recoverable device
into a permanently offline one."

**Nothing in the suite executes any of this.** `grep` over the full
`cargo test -- usb_storage` output finds zero occurrences of `transport broke`,
`Reset Endpoint`, or `Set TR Dequeue`. Every failure the suite produces is
`Bot::Failed` (a CSW with status 1) — `usb-storage: SCSI 0x2a failed, sense
0x07/0x27/0x00`, three times. The data-phase stall path *may* have run inside
the write-protected-LUN test, and this is the second half of the finding: **a
`clear_stall` that succeeded and a `clear_stall` that was never called are
indistinguishable in the log**, because it logs nothing on success. That is the
`panic_console::capture` shape from known-issues §"A panic strands whatever
lock the dead thread held" — a recovery path whose only evidence of having run
is that nothing went wrong.

**Proposed shape.** `Bot` should say which state the endpoint is in, because
that is what decides which command is legal:

```rust
enum Bot {
    Done { residue: u32 },
    Failed,
    /// The endpoint halted. Reset Endpoint is legal, and is the only way out.
    Halted,
    /// The device never answered, or answered something no endpoint reset
    /// addresses. The endpoint is still Running, so Reset Endpoint would
    /// return Context State Error — and treating that as "recovery failed" is
    /// what takes a device offline for a fault it had already recovered from.
    Silent,
}
```

with `reset_recovery` branching on it: `Halted` keeps today's three steps;
`Silent` issues Stop Endpoint (TRB type 15, which *is* legal on a Running
endpoint) before Set TR Dequeue, and skips CLEAR_FEATURE(HALT) for an endpoint
that is not halted. The minimal version, if the fuller one is too much to land
at once, is one line in `clear_stall`:

```rust
// Context State Error means the endpoint was not Halted, which is the state
// this command exists to leave. Nothing to do, and not a reason to give up on
// the device.
match self.run_command_code(reset, "Reset Endpoint") {
    Some(CC_SUCCESS) | Some(CC_CONTEXT_STATE_ERROR) => {}
    _ => return false,
}
```

**And it needs a test that can fail.** The actuator is a kernel feature in the
shape `xhci-one-slot` already has: one that makes the next bulk transfer's
`wait_transfer` return `None` without waiting. Two assertions: the log says
`transport broke`, and the *next* `read_blocks` on the same disk succeeds.

### F4 (medium-high). An endpoint address whose endpoint number is 0 becomes DCI 0 or DCI 1, and the driver writes a bulk endpoint over the slot context or over EP0's

**Location.** `device.rs:140-150` (`parse_config`'s MSC endpoint arm),
`msc.rs:534-535` and `:553-569` (`bind`); the same hole at `device.rs:335-336`
for HID.

**The bug the current shape permits.** `parse_config` accepts an endpoint
descriptor on the strength of its direction bit and its transfer type, and
never looks at the endpoint *number*:

```rust
Some(Function::Msc(m)) if transfer == 2 => {
    if addr & 0x80 != 0 && m.in_ep == 0 { m.in_ep = addr; ... }
    else if addr & 0x80 == 0 && m.out_ep == 0 { m.out_ep = addr; ... }
}
```

The final gate is `m.in_ep != 0 && m.out_ep != 0`, which tests the whole byte.
`0x80` is a non-zero byte naming endpoint 0 IN; `0x10` is a non-zero byte
naming endpoint 0 OUT. `bind` then computes

```rust
let in_dci  = (info.in_ep  & 0x0F) * 2 + 1;   // 0x80 -> 1, which is EP0
let out_dci = (info.out_ep & 0x0F) * 2;       // 0x10 -> 0, which is the slot context
```

With `out_dci == 0`, the loop at `:553` runs with `ctx = out_dci + 1 = 1` and
writes five dwords into **context index 1, the slot context** — overwriting the
speed and Context Entries written at `:545` with zero, and the root hub port
number written at `:546` with an endpoint's max-packet/burst word. With
`in_dci == 1`, the Add-Context flags at `:543` set A1 (which a Configure
Endpoint command must not set) and the loop writes context index 2, EP0's
endpoint context, with EP Type 6 and the bulk ring's dequeue pointer.

**Blast radius, stated honestly.** No out-of-bounds write: the largest context
index reached is 32, and `32 * 64 + 16 = 2064` is inside the 4 KiB input
context. A conformant xHC rejects both commands with Parameter Error, so what
actually happens is `xHCI: Configure Endpoint (bulk) failed, code=5`, `bind`
returns false, and the slot leaks. **The driver is relying on the host
controller to reject a command it constructed from device-supplied bytes** —
and if a controller accepted the `in_dci == 1` case, the device's control
endpoint would be reprogrammed as a bulk endpoint pointing at `in_ring`, and
`clear_stall` and `reset_recovery`, both of which issue control transfers on
`dev.ep0_ring`, would then be driving a context that names a different ring.

The file's own header says wire values "are checked and the device is refused
by name". This one is refused by the controller, with a line naming Configure
Endpoint rather than the bad descriptor.

**Proposed shape.** The computation is the thing to make total, since two
callers do it:

```rust
/// The device context index of a non-control endpoint, which is the only kind
/// this driver configures. `None` for endpoint 0: its IN DCI is 1, which is
/// the control endpoint a Configure Endpoint command must not name, and its
/// OUT DCI is 0, which is the slot context.
fn endpoint_dci(addr: u8) -> Option<u8> {
    let num = addr & 0x0F;
    (num != 0).then(|| num * 2 + u8::from(addr & 0x80 != 0))
}
```

`msc::bind` takes `MscInterface { in_dci, out_dci }` already resolved, so the
refusal happens in `parse_config` where the descriptor is, with the log line
that names it:

```rust
Some(Function::Msc(m)) if transfer == 2 && endpoint_dci(addr).is_some() => { ... }
```

`device.rs:336`'s `let int_ep_dci = ep_num * 2 + 1;` becomes the same call, and
the HID gate `(h.ep_addr != 0)` at `:186` becomes
`endpoint_dci(h.ep_addr).is_some()`.

**What it deletes.** Two hand-rolled DCI expressions and the `!= 0` byte tests
that were standing in for a check on the endpoint number.

### F5 (medium). `healthy()` returns true for a disk the driver has permanently given up on

**Location.** `usb_storage.rs:44-53`, with `msc.rs:70-74` and `msc.rs:78-83`.

**The bug the current shape permits.**

```rust
/// Whether the controller still has a disk under this index at all.
///
/// Distinct from a failed transfer, which the trait reports: this answers
/// "is there still something there", which is what a caller asks after a
/// run of failures.
pub fn healthy(&self) -> bool {
    xhci::storage_geometry(self.index).is_some_and(|g| g.blocks > 0)
}
```

`MscDevice::failed` is the flag that says the driver will never speak to this
device again — every `transfer_blocks` and `msc_flush` returns `false` on
sight of it without touching the bus. `geometry()` returns
`logical_block_bytes` and `blocks`, neither of which `failed` disturbs. So the
one question this function exists to answer — *after a run of failures, is
there still something there?* — is the one it gets wrong, and it gets it wrong
in the direction that keeps a caller retrying a device the driver has already
written off.

Latent today: the only caller is `usb_gate.rs:206`, a log line, and on that
path the disk is never `failed`. It stops being latent the moment anything
retries on it — which is the situation the doc comment describes.

**Proposed shape.** Publish the fact rather than inferring it from a proxy:

```rust
// xhci/mod.rs
/// Whether the machine's `index`-th disk is still being spoken to. `false`
/// once recovery has failed on it: the geometry survives that (it is what the
/// device reported before it broke) so it cannot be the answer.
pub fn storage_online(index: usize) -> Option<bool> {
    with_disk(index, |ctrl, local| ctrl.storage[local].online())
}

// usb_storage.rs
pub fn healthy(&self) -> bool {
    xhci::storage_online(self.index) == Some(true)
}
```

**What it deletes.** The `is_some_and(|g| g.blocks > 0)` inference, and the
implicit claim that a geometry is evidence of liveness.

### F6 (medium). `MSC_SCRATCH` is 64 bytes and `bot` bounds every transfer against `MSC_DATA_LEN`, which is 32 KiB

**Location.** `msc.rs:342`, against `mod.rs:229-230` and the four callers at
`msc.rs:310-314` and `msc.rs:657-658`.

**The bug the current shape permits.**

```rust
assert!(data_len as usize <= MSC_DATA_LEN);
```

Four of the five call sites do not point at `MSC_DATA`. `request_sense` and
`bring_up`'s `read_scratch` closure point at `MSC_SCRATCH`, whose length is
`MSC_SCRATCH_LEN = 64`. The assertion permits a 32,768-byte transfer into a
64-byte buffer. Today's largest is 36 (INQUIRY), so there is no live bug — the
next command added is where it becomes one, and the assertion is what a person
adding it will read to decide the buffer is big enough.

This is the `IpcPayload` shape: a bound that names a buffer and binds a
different one. It is a bound in the right place with the wrong operand.

**Proposed shape.** `bot` is given a physical address it cannot reason about;
give it the region instead:

```rust
fn bot(&mut self, dev: &mut MscDevice, cdb: &[u8], cdb_len: u8,
       data: Option<(usize, u32)>, data_in: bool) -> Bot
```

where the `usize` is the offset inside the device's block and the length is
checked against the region that offset names, rather than against the largest
region in the block. `MSC_SCRATCH_LEN` then has one reader and it is the one
that matters.

**What it deletes.** The separate `write_bytes(.., MSC_SCRATCH_LEN)` zeroing at
three sites, which exists because the caller and the callee currently disagree
about which buffer is in play.

### F7 (medium). `CAP_DBOFF` and `CAP_RTSOFF` are controller-supplied and reach an unchecked subtraction

**Location.** `mod.rs:724-725`, `mod.rs:732-735`.

**The bug the current shape permits.**

```rust
let db_offset  = (bar.read_u32(CAP_DBOFF)  & !0x3)  as u64;
let rts_offset = (bar.read_u32(CAP_RTSOFF) & !0x1F) as u64;
let bar_size = 0x10000u64;
let db_base = bar.subregion(db_offset, bar_size - db_offset);
let rt_base = bar.subregion(rts_offset, bar_size - rts_offset);
```

Both are 32-bit values read out of the controller's capability registers, up to
~4 GiB. `bar_size` is a hardcoded 64 KiB — the BAR is never sized. With
`db_offset > 0x10000`, `bar_size - db_offset` underflows. The kernel is built
with `[profile.dev]` and `kernel/Cargo.toml:101` states that overflow checks
stay on, so this is a panic reading *"attempt to subtract with overflow"* — a
kernel panic at boot whose message says nothing about USB.

With overflow checks off it is worse, not better: `db_offset + (0x10000 -
db_offset)` wraps back to exactly `0x10000`, so `Mmio::subregion`'s
`assert!(offset + size <= self.size)` **passes**, and the driver gets an `Mmio`
whose base is outside the mapping and whose size is ~2^64. The first doorbell
write faults.

Real controllers put DBOFF around 0x800–0x3000, so this is a low-probability
path — but it is the only place in this file where a controller-supplied number
reaches arithmetic with no check at all, in a file whose `Layout::new` writes a
paragraph justifying why the *other* controller-supplied numbers are safe.

**Proposed shape.** Same refusal `arm_interrupt` already established:

```rust
// The BAR is mapped at a fixed 64 KiB and both offsets are the controller's
// own 32-bit numbers, so a controller that puts its doorbells or its runtime
// registers outside the window is one this driver cannot address — refused
// here rather than faulting on the first doorbell.
let (Some(db_len), Some(rt_len)) =
    (bar_size.checked_sub(db_offset), bar_size.checked_sub(rts_offset))
else {
    log!("xHCI: DBOFF={db_offset:#x} RTSOFF={rts_offset:#x} outside the {bar_size:#x} \
          register window at PCI {:02x}:{:02x}.{} — not initialised",
        pci_dev.bus, pci_dev.dev, pci_dev.func);
    return None;
};
```

### F8 (medium). `PAGESIZE` panics the whole machine for one controller's property, on a machine that has two controllers

**Location.** `mod.rs:740-747`.

```rust
assert_eq!(pagesize, 1,
    "xHCI: controller wants a page size this driver does not implement, PAGESIZE={pagesize:#x}");
```

**The bug the current shape permits.** Sixty lines above, a controller that
offers neither MSI-X nor MSI is refused *by name* and `init` carries on with
the rest — with a comment explaining that this is deliberate, and a log line
distinguishing "no controller on this machine" from "controllers present, none
usable". Both properties are equally fatal to that controller and neither is
fatal to the machine. One gets the refusal; the other panics the boot.

The T14 has two xHCIs. A panic here because of the Thunderbolt controller's
register takes down a machine whose keyboard is on the other one — which is
the exact failure the `63cacbc` "drive every controller" work exists to
prevent.

Second, smaller half: the test is `pagesize == 1`, but the property the driver
needs is "4 KiB is supported", which is bit 0. Linux's `xhci-hcd` reads this
register with `ffs()` and drives any controller that sets bit 0. I could not
confirm from the spec text in reach whether more than one bit is legal, so I am
not claiming a controller in the field sets two — I am noting that the check is
stricter than the requirement and that the comment ("Bit 0, and only bit 0,
means 4 KiB") states the requirement in a way that reads as if it justifies the
equality when it justifies a mask.

**Proposed shape.**

```rust
if pagesize & 1 == 0 {
    log!("xHCI: NOT INITIALISED at PCI {:02x}:{:02x}.{} — PAGESIZE={pagesize:#x} does not \
          include 4 KiB, and every ring, context and scratchpad buffer here is placed at \
          4 KiB. No USB device on it can be used.",
        pci_dev.bus, pci_dev.dev, pci_dev.func);
    return None;
}
```

**What it deletes.** The asymmetry between the two refusals, and the last
`assert!` in this file that a machine can reach.

### F9 (low). `MIN_DEVICE_BLOCKS`'s comment names four scratchpad demands; there are 32, and none of them is one of the four

**Location.** `mod.rs:250-258`.

> "for four of the 1024 possible demands (503, 504, 1014, 1015) it leaves
> *nothing*: at 504, `dev_base` is exactly 2097152, so without the floor
> `dev_blocks` is 0"

**Computed** by reimplementing `Layout::new` exactly (`SHARED_SIZE`,
`MSC_STRIDE`, `MSC_BLOCKS`, `DEV_STRIDE`, `align_2m`) and sweeping all 1024
demands. Without the floor, `dev_blocks` is 0 for **32** demands: **458–473 and
969–984**. At n=504, `dev_base` is **2,228,224** and `align_2m(dev_base)` is
4,194,304, giving 160 blocks — the floor is not load-bearing there at all.

The number 2,097,152 is real, and at n=504 it is `msc_base`. The comment
describes the layout as it was *before* the mass-storage array was inserted
ahead of the device array, when `dev_base` was where `msc_base` now is. It was
not re-derived when the array moved. The smallest value the floor actually
guards against, with today's layout, is 10 blocks (at n=426–431).

Same comment, same paragraph: "`dev_base` is at most about 4.4 MiB" — the
measured maximum is 4,390,912 bytes, which is 4.19 MiB (or 4.39 MB decimal).
Right to within the unit confusion.

This is CLAUDE.md's rule about numbers doing exactly what it exists to do. The
sentence reads perfectly plausible, the mechanism it describes is real, and
every specific figure in it is wrong.

**Proposed shape.** The comment should state the property, and let the number
be a derived example rather than a memorised list:

```rust
/// Device blocks to size the pool for before the controller's slot count is
/// consulted. A scratchpad demand that lands `dev_base` on or just under a
/// 2 MiB boundary leaves no slack in the page it forced us to allocate anyway;
/// for 32 of the 1024 possible demands it leaves *nothing at all*, and
/// `dev_blocks` would be 0, MaxSlotsEn would be written as zero, and the
/// controller would enumerate nothing. This is not defensive padding; it is
/// what keeps the pool from having no room for devices.
```

### F10 (low). `legacy.rs`'s termination argument overstates itself by one

**Location.** `legacy.rs:16-19`.

> "The walk terminates for three independent reasons — the next pointer is a
> strictly positive *forward* delta …, every read is bounds checked …, and the
> iteration count is capped — and it would still terminate with any two of them
> removed."

"Any two removed" leaves one. The bounds check alone terminates (the offset
strictly increases and eventually leaves the window). `MAX_CAPS` alone
terminates. **The forward-delta property alone does not**: with the window
check and the cap both gone, `offset += next * 4` climbs through the u64 space,
which is not termination in any sense the sentence is claiming — and the
selftest's own last case ("an endless chain inside a window that never ends")
is the demonstration, since it is the case where only `MAX_CAPS` saves the
walk.

The claim should be "with any *one* of them removed", which is true and is what
the selftest's eight cases actually cover.

### F11 (low). `READY_BUDGET_NS`'s second clause: one attempt can cost twenty times the budget

**Location.** `msc.rs:24-32`, `msc.rs:618-647`.

The first clause holds and is the interesting half: a device that answers
nothing has already spent `USB_TIMEOUT_NS`, the budget is blown when the loop
next checks, and it does not get three more transfer timeouts. **Verified by
reading the loop** — `if dev.failed || nanos_since_boot() >= give_up { break }`
is after the round trip, so a 2 s timeout ends the loop on the first pass.

What does not hold is "Boot time is what is being protected, and boot time is
what this measures." The budget measures when to stop *starting* attempts; it
bounds nothing about the one already running. A device that NAKs indefinitely
costs, in one pass: the CBW's 2 s, then `reset_recovery`'s Bulk-Only Reset
control transfer (2 s), then two `clear_stall`s each ending in a
CLEAR_FEATURE(HALT) control transfer (2 s each) — about 10 s of the boot for
one device, against a 500 ms budget. Multiplied by however many such devices
are on the bus; `Profile::MetalUsb` puts six on one controller.

The honest statement is that `READY_BUDGET_NS` bounds the *retries* and
`USB_TIMEOUT_NS` times how long each costs, and that the product, not the
budget, is the boot-time figure. A budget that meant what the comment says
would have to be checked inside `bot`, or the deadline would have to be passed
down.

### F12 (low). The slot leak is 11 exits, not four plus three; three of them are unnamed anywhere

**Location.** known-issues §"The xHCI driver never gives a slot back";
`device.rs:225-231`.

Enumerated by reading every path between the successful Enable Slot at
`device.rs:214` and a bound device:

| # | location | what failed |
|---|---|---|
| 1 | `device.rs:232` | slot id past the pool's device blocks — *named* |
| 2 | `device.rs:269` | Address Device — *named* |
| 3 | `device.rs:284` | GET_DESCRIPTOR(Device) — *named* |
| 4 | `device.rs:298` | GET_DESCRIPTOR(Config) — *named* |
| 5 | `device.rs:303` | no interface this driver binds (hub, camera, or an MSC interface with no bulk pair) — *named* |
| 6 | `device.rs:314` | **SET_CONFIGURATION — not named** |
| 7 | `msc.rs:528` | no mass-storage block left — *named* |
| 8 | `msc.rs:574` | **Configure Endpoint (bulk) — not named** |
| 9 | `msc.rs:596` | `bring_up` — *named* |
| 10 | `device.rs:390` | **Configure Endpoint (HID) — not named** |
| 11 | `device.rs:405` | `PointerSource::claim` exhausted — *not named* |

Also: known-issues lists "a disk whose interface has no bulk pair" as one of
mass storage's three additions, but that one is refused inside `parse_config`
and exits at #5, which was already on the list. `msc::bind`'s three exits are
#7, #8 and #9.

Not a new defect — the class is filed and the exposure argument (harmless where
slots outnumber ports) is right. Recorded because a fix that adds Disable Slot
to the four named sites leaves seven behind, and the leak entry is what
somebody will work from.

### F13 (low). Hotplug's remaining obstacles, named precisely

Verified against the code, since the prompt asked and the record's count is one
short:

1. **`XhciController` does not retain `op_base`.** The struct's fields are
   `db_base`, `rt_base`, `context_size`, `layout`, `pool`, `disk_base`,
   `cmd_ring`, `event_ring`, `event_head`, `event_phase`, `devices`, `storage`
   (`mod.rs:352-380`). `op_base` and `max_ports` are locals in `init_one`.
   PORTSC is unreachable after `init` returns, so nothing can read a port's
   state and nothing can call `scan_ports` — this is the concrete blocker and
   it is not in the record.
2. **`dispatch_event` drops every non-transfer event** (`mod.rs:418`), Port
   Status Change (TRB type 34) included, so a plug is never observed even by
   the CPU that takes the interrupt.
3. **The enumeration scratch is one instance per controller.**
   `OFF_INPUT_CTX` and `OFF_DATA_BUF` are fixed pool offsets used by
   `init_device` and by `msc::bind`, safe only because enumeration is serial.
   This is the one the record names, and its answer — an enumeration lock, not
   more buffers — is right.
4. **Slots are never given back** (F12), so unplug/replug walks
   `layout.dev_blocks` down until `layout.device(slot_id)` starts returning
   `None`.

The record's claim that per-device EP0 rings removed one of three obstacles
holds. The event demux is genuinely done and correct: `wait_transfer` matches
slot *and* endpoint (`mod.rs:501`), which mass storage needed because one slot
carries three endpoints.

---

## Part 3 — cost

**There is no FAT32-shaped impedance mismatch inside the USB stack.** I went
looking for it and it is not there. The arithmetic, so nobody has to derive it
again:

- One SCSI command is exactly **three bulk transfers** when it moves data
  (CBW 31 B out, data, CSW 13 B in) and two when it does not. Each is one
  Normal TRB, one doorbell write, and one `wait_transfer`.
- `MSC_MAX_BLOCKS = MSC_DATA_LEN / 4096 = 8`, so
  `read_blocks(lba, n)` costs `3 * ceil(n / 8)` transfers and moves at most
  32 KiB per command.
- Therefore `read_blocks(lba, 1)` — 4096 bytes — costs 3 transfers, of which
  two move 44 bytes of protocol. That ratio is Bulk-Only Transport's, not this
  driver's, and it improves 8× if the caller batches.
- The batching loop is correct and the driver never re-reads anything. Nothing
  in scope is O(n) where it should be O(1): `with_disk` is O(controllers),
  `with_storage` copies a ~120-byte `MscDevice` per operation, and
  `wait_transfer` is O(events pending) because it must drain and dispatch
  foreign events rather than drop them.

**What does batch, and what does not.** On USB the only batching caller is
`EspDevice`'s whole-block path in `fat32_adapter.rs`. `page_cache::raw_block_read`,
`raw_block_write`, `PageCache::read`, `EspDevice::load`, `EspDevice::write_at`'s
partial path and `gpt.rs`'s `DeviceSectors` all hardcode `count = 1`.
`page_cache::sync` coalesces up to 32 blocks — which is 4 SCSI commands where 1
would do if `MSC_DATA_LEN` were 128 KiB — but the page cache's device is NVMe,
so that costs nothing today and becomes a real number the day `/home` moves to
USB.

**Measured, with its resolution stated.** From the `usb_storage_gate` boot: the
whole in-guest sweep — 11 SCSI commands, 24 KiB read and 36 KiB written plus a
flush — runs between `usb-gate: disk 1 designated` at t=0.102 and `usb-gate:
disk done` at t=0.105, i.e. **3 ms**, under TCG. Serial timestamps have 1 ms
resolution, so that is ~0.3 ms per command as an upper bound and no better.
Enumerating and bringing up one disk (TEST UNIT READY, INQUIRY, READ
CAPACITY(10)) fits inside 2 ms; the ESP probe and mount, isolated from the
concurrent AP bring-up on the same log, takes ~2 ms.

**What those numbers do not say.** They are QEMU's, where a bulk transfer is a
memcpy. known-issues records the ESP flush at 2.0–9.7 ms per flush against a
23.219 ms audio pipeline and correctly names `wait_transfer`'s spin and `Lock`'s
preemption hold as why nothing shortens it — **all of that was measured against
an emulated xHC.** The quantity that transfers to metal is the transfer *count*
(3 per 4 KiB, plus the FAT and directory-entry writes above it), not the
millisecond figure. On a real high-speed bulk endpoint the wire time per
transfer is bounded below by the microframe schedule; I am not putting a number
on that here because I have not measured one, and the metal-track history is
explicit that only same-session A/B measurements count.

**The one genuine amplifier I found is F1, and it is a correctness bug rather
than a unit mismatch.** A stick without SYNCHRONIZE CACHE does not make each
operation more expensive; it makes the idle loop never stop issuing them.

**Second-order, worth knowing.** Every storage operation runs under
`XHCI.lock()`, which is a ticket spinlock that disables preemption and panics
on 500M spins. known-issues already files "one command at a time per
controller, under the xHCI lock, with preemption disabled for its duration".
What is not filed is the interaction with F2 and F3: a single failing command
holds that lock across up to three 2 s `wait_transfer` deadlines plus
`reset_recovery`'s round trips, while any other CPU entering `poll_if_pending`
(scheduler pass, or a keyboard/mouse `sys_read`) spins on the same lock. Whether
that reaches `Lock`'s 500M-spin `DEADLOCK` panic depends on the host's `pause`
latency, and I did not measure it — the two bounds are within an order of
magnitude of each other, which is close enough that it should not be left to
chance.

---

## Part 4 — probes tried, including the ones that came back clean

Eleven, built by reading the value back to where it could do damage. A clean
result is a result.

| # | probe | outcome |
|---|---|---|
| 1 | Block size 0, 3, 8192 from READ CAPACITY | **Refused** at `msc.rs:723` before the divide and the double-divide |
| 2 | Last LBA = `u32::MAX` → READ CAPACITY(16) with a 64-bit answer | **Refused** by name; observed live on `UsbDiskHuge` — 6442450944 sectors |
| 3 | Capacity smaller than one 4 KiB block | **Refused** at `msc.rs:739` |
| 4 | CSW with the wrong tag / wrong signature / 12 bytes / 14 bytes | **All four refused**; the short and long cases fall out of `Some((CC_SUCCESS, 0))` |
| 5 | Residue larger than the transfer | **Refused** at `msc.rs:272`, before the subtraction |
| 6 | Descriptor with `bLength == 0`, `wTotalLength` past the buffer, a truncated final descriptor | **All bounded** — zero-length breaks the walk, `wTotalLength` is `.min(buf.len())`, every field goes through `get` or a `desc.len()` guard |
| 7 | Descriptor with endpoint address `0x80` / `0x10` | **PASSED** into DCI 1 / DCI 0 → F4 |
| 8 | INQUIRY peripheral type ≠ 0 | **Refused** at `msc.rs:679` |
| 9 | Device-supplied ASCII in the log line | **Bounded** — `Printable` escapes everything outside 0x20..0x7F and the quote character |
| 10 | SYNCHRONIZE CACHE answered ILLEGAL REQUEST | **PASSED** as a device failure → F1 |
| 11 | `Layout::new` swept over all 1024 scratchpad demands | No overflow, no zero-block outcome with the floor; the comment's numbers are wrong → F9 |

Probes 1–6 and 8–9 are the ones the prompt named as the likely holes, and seven
of the nine came back clean. That is the result worth recording: this is the
first driver in this tree whose device-value handling I could not break by
reading it.

---

## Part 5 — examined and deliberately not flagged

- **`Drop` guards that cannot fire.** There is no `impl Drop` anywhere in
  scope. CLAUDE.md's caveat does not apply; `MscDevice` is `Copy` and its
  lifecycle is `with_storage`'s take-modify-write-back, which is explicit.
- **`transfer_blocks`' `assert_eq!(host.len(), count * HOST_BLOCK)`.** A kernel
  contract, documented as one at `msc.rs:179-181`, and correct: every caller
  (`usb_gate`, `EspDevice`, `page_cache`, `gpt.rs`) constructs the slice from
  `count` by the same arithmetic. No device value reaches it.
- **`bot`'s `assert!(cdb_len <= cdb.len() && cdb_len <= 16)`.** The CDBs are
  this file's own literals. Same category.
- **The double memcpy through `MSC_DATA`.** Every byte is copied device→pool
  and pool→caller. It is what makes the "no 64 KiB crossing" placement rule
  satisfiable without physically-contiguous caller buffers, and 4 KiB of memcpy
  is far below one USB transfer. Correct, cheap, and the alternative is worse.
- **`wait_transfer` matching on (slot, dci) rather than on the TRB address.**
  A completion left outstanding by a timeout could in principle be attributed
  to a later transfer on the same endpoint. I traced it: for a READ the data
  phase and the CSW are both on the IN endpoint, so a stale event shifts the
  pairing by one and the CSW read then lands on a zeroed buffer, which fails
  the signature check at `msc.rs:406` and becomes `Bot::Broken`. The signature
  and tag checks close this; it degrades safely and I am not filing it.
- **`next_tag` wrapping at 2^32.** The tag is checked per command, not across
  commands. A repeat after four billion commands is not a hazard.
- **`blocks = sectors / sectors_per_block` rounding down**, so a device whose
  sector count is not a multiple of 8 has an unreachable tail — including,
  on a 512-byte-sector disk, the backup GPT header at the last LBA.
  `storage-stack.md` already records this for `DeviceSectors::lba_count` and
  notes it becomes relevant when a backup-GPT fallback exists. Worth knowing
  that USB is now where `gpt::probe` actually runs, which that entry said was
  not yet the case — but it is the same finding and belongs there.
- **`MSC_BLOCKS = 2` is per controller, not per machine.** The doc reasons
  about "a machine booting off a USB stick with a second one plugged in"; a
  two-controller machine gets four. Not a bug — the constant sizes one pool and
  one pool belongs to one controller.
- **Multiple LUNs, UAS, CBI/CB, READ(16)/WRITE(16), removable media, MODE
  SENSE, one-command-at-a-time concurrency.** All seven are already written up
  in known-issues §"USB mass storage: what is not implemented", each with its
  reason, and I agree with every one of them.
- **`page_cache::sync` allocating 128 KiB per call.** Transient, well under
  `MAX_HEAP_ALLOC`, and `sync` is not on a hot path. Not worth complicating.
- **`page_cache::alloc_slot`'s `assert!(!ptr.is_null())`.** A heap failure, not
  a device value. Same category as the rest of the kernel's allocation asserts.
- **`selftest()` runs once per controller**, so a two-controller machine logs
  the xECP selftest line twice. Cosmetic.
- **The `#[must_use]` messages on `BlockDevice`.** They are the best ones in the
  tree — each names the *consequence* rather than the rule. Nothing to flag;
  worth copying.

---

## What could not be checked

- **Anything requiring real hardware.** The USBLEGSUP happy path is
  structurally untestable in QEMU, as `legacy.rs`'s own header says. What I can
  add to that: the walk and the eight malformed lists are covered by the
  selftest, and the *unguarded* part is everything after `find` returns an
  offset — the semaphore write, the release wait, and the USBLEGCTLSTS
  read-modify-write have no test of any kind and no assertion, and the driver's
  behaviour if firmware releases and then reclaims is undefined by this code.
  The one thing I would change on that basis alone is F7 and F2, since a
  controller firmware is fighting over is exactly the one whose register bits
  do not settle.
- **Whether a real xHC returns Context State Error for Reset Endpoint on a
  Running endpoint** (F3). Taken from the spec's statement that the command
  applies to the Halted state and from Linux issuing it only after a halt; not
  verified against QEMU's implementation, and not reachable in the suite at all.
- **Whether more than one PAGESIZE bit is legal** (F8). Linux reads the
  register with `ffs()`, which is consistent with either reading. The half of
  F8 that does not depend on this — panic versus per-controller refusal — stands
  regardless.
- **Wire-time cost on metal.** Everything measured here is TCG. The transfer
  counts are read off the code and are exact; the milliseconds are not
  transferable and I have not claimed they are.
- **Whether `usb_storage_write_error` exercises `clear_stall`.** It cannot be
  determined from the log, which is itself F3's second half.
