//! VirtIO console — single-port (no MULTIPORT). Replaces the 16550 UART
//! as the kernel log channel after init. Per byte, the UART takes two
//! port-IO vmexits (LSR poll + data write); the FIFO drain bit defeats
//! the 16-byte FIFO so a 100-byte log line eats ~200 vmexits. virtio-console
//! takes one notify per submission — host writes the chardev directly with
//! no per-byte stalls.
//!
//! Single-port mode uses queues 0 (RX) and 1 (TX). MULTIPORT is offered by
//! QEMU but not negotiated; the device falls back to port-0-only with no
//! control queues. RX is poll-driven, matching the existing UART semantics
//! (see `arch/idt/mod.rs` — the legacy PIC is disabled and no UART IRQ
//! handler is wired, so input has always been polled).

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;
use crate::mm::{Dma, Unaligned};

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_CONSOLE_DEVICE: u16 = 0x1043; // 0x1040 + device_id 3

const QUEUE_SIZE: u16 = 16;
const TX_BUF_SIZE: usize = 4096;
const RX_BUF_SIZE: u32 = 256;
const RX_BUF_COUNT: usize = 8;

// DMA layout (4KB-aligned regions in a 16KB pool):
const OFF_TX_BUF:  usize = 0x0000; // 1 page (4KB)
const OFF_RX_BUFS: usize = 0x1000; // 8 × 256 = 2KB, fits in 1 page
const OFF_RXVQ:    usize = 0x2000; // virtqueue desc/avail/used
const OFF_TXVQ:    usize = 0x3000;
const DMA_SIZE:    usize = 0x4000;

struct RxPending {
    buf_idx: usize,
    slot: DescSlot,
    len: u32,
    pos: u32,
}

/// **The buffers are [`Dma`] views and not raw pointers, and that is what
/// deleted this type's `unsafe impl Send`.** Every field is `Send` on its own,
/// and a view carries the bounds the bare `*mut u8` did not, so the TX copy and
/// the RX byte read below are checked against the buffer's length rather than
/// against a comment.
///
/// `'static` because the pool is leaked at `init`: this console is the kernel's
/// log channel for the life of the boot and is never unbound. The `static`
/// holding the pool alive is gone with it — `Dma<'static>` is the same claim
/// made in the type, and it is the only way to obtain one.
struct VConsole {
    device: VirtioDevice,
    rx: Virtqueue<'static>,
    tx: Virtqueue<'static>,
    tx_buf: Dma<'static>,
    tx_slot: Option<DescSlot>,
    /// The RX buffers under the unaligned discipline: what is read out of one is
    /// a single byte the device has already delivered — its descriptor came back
    /// through the used ring and has not been refilled — so nothing is racing
    /// the read and there is no alignment to keep.
    rx_bufs: [Dma<'static, Unaligned>; RX_BUF_COUNT],
    /// Maps virtqueue desc id → rx_buf index (filled at refill, read at poll).
    desc_to_rx: [u8; QUEUE_SIZE as usize],
    /// Currently-draining RX buffer (slot recovered from used ring but not
    /// yet refilled, because not all bytes have been consumed).
    rx_pending: Option<RxPending>,
}

struct ConsoleCell(UnsafeCell<MaybeUninit<VConsole>>);
// SAFETY: irreducible — a `static` needs `Sync` and the payload is a
// `MaybeUninit` that has to be written after `init` has built it, which no
// `Sync` type in this kernel expresses (a `Lock` cannot be taken from the
// panic path, and `VConsole` is far too large for an atomic). What makes it
// sound is stated on `CONSOLE` below and enforced at every reader: `READY`
// (Acquire) gates construction, and the three accessors go through
// `with_console`, which every caller reaches holding `serial::BackendGuard`
// with interrupts disabled — that outer lock is the mutual exclusion this impl
// does not provide.
unsafe impl Sync for ConsoleCell {}

/// Initialized exactly once in `init()` and then never written. Reads are
/// gated by `READY` (Acquire); mutation goes through `write_bytes_locked`,
/// `try_read_byte_locked`, and `has_data_locked`, all of which require the
/// caller to be holding `serial::BackendGuard` with interrupts disabled — that
/// outer lock is what serializes concurrent access to the VConsole state.
static CONSOLE: ConsoleCell = ConsoleCell(UnsafeCell::new(MaybeUninit::uninit()));
static READY: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn is_ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Disable the virtio-console fast path. After this, `is_ready()` returns
/// false and the log macro falls back to UART. Used by the panic handler
/// to bypass any potentially-wedged virtqueue state.
pub fn disable() {
    READY.store(false, Ordering::Release);
}

/// Run `f` against the live console, or answer `None` because there is not one.
///
/// **The readiness check and the dereference are one thing now.** Three callers
/// each wrote `if !is_ready() { return … }` and then `unsafe { console_mut() }`,
/// which is three copies of an obligation and three chances to write the second
/// without the first. Here it is structural, and the three call sites are
/// ordinary safe code.
#[inline]
fn with_console<R>(f: impl FnOnce(&mut VConsole) -> R) -> Option<R> {
    if !is_ready() {
        return None;
    }
    // SAFETY: irreducible — `MaybeUninit::assume_init_mut` is the only way to
    // reach a value written into a `static` after its declaration, and the
    // `&mut` out of an `UnsafeCell` is what `ConsoleCell`'s `Sync` impl exists
    // for. Initialisation is proved by the `READY` Acquire load above, which
    // pairs with the Release store `init` makes after `CONSOLE` is written and
    // is the only thing that ever sets it. Exclusion is *not* proved here and
    // cannot be: every caller of this function is reached only through
    // `serial::BackendGuard`, which is a global spinlock held with interrupts
    // disabled, and that is the lock serialising these `&mut`s.
    Some(f(unsafe { (*CONSOLE.0.get()).assume_init_mut() }))
}

fn refill_rx(c: &mut VConsole, buf_idx: usize, slot: DescSlot) {
    let desc_id = c.rx.submit(
        slot,
        &[(c.rx_bufs[buf_idx].phys(), RX_BUF_SIZE, BufDir::Writable)],
        c.device.notify_mmio(),
        c.device.notify_off_multiplier(),
        0,
    );
    c.desc_to_rx[desc_id as usize] = buf_idx as u8;
}

/// Write to the host. Caller must hold `serial::BackendGuard` with IRQs disabled.
/// Synchronous: waits for the host to consume each chunk before returning,
/// matching the existing UART writer's "byte is on the wire when we return"
/// guarantee. With QEMU/TCG the host processes the notify vmexit inline,
/// so this is one vmexit per chunk, not per byte.
pub fn write_bytes_locked(bytes: &[u8]) {
    with_console(|c| {
        let mut off = 0;
        while off < bytes.len() {
            let n = (bytes.len() - off).min(TX_BUF_SIZE);
            // Bounded by `copy_from`, which refuses more than `tx_buf.size()`
            // — `TX_BUF_SIZE`, which `n` is `min`ed against. Nothing else may be
            // touching the buffer: the caller holds `serial::BackendGuard`, and
            // the previous chunk's `submit_and_wait` returned, so the device is
            // done with it.
            c.tx_buf.copy_from(0, &bytes[off..off + n]);
            let slot = c.tx_slot.take().expect("vconsole: no tx slot");
            c.tx_slot = Some(c.tx.submit_and_wait(
                slot,
                &[(c.tx_buf.phys(), n as u32, BufDir::Readable)],
                c.device.notify_mmio(),
                c.device.notify_off_multiplier(),
                1,
            ));
            off += n;
        }
    });
}

/// Read one byte from RX. Caller must hold `serial::BackendGuard` with IRQs disabled.
pub fn try_read_byte_locked() -> Option<u8> {
    with_console(|c| {
        if c.rx_pending.is_none() {
            // Both numbers are bounded by `poll_used`: the id indexes
            // `desc_to_rx`, which is exactly `QUEUE_SIZE` long, and `len` is at
            // most the `RX_BUF_SIZE` this driver posted, so the walk below
            // stays inside the buffer it started in. Neither was checked
            // before, and the read at the bottom of this function is inside the
            // direct map, so an over-long `len` used to hand kernel memory to
            // the console as typed input.
            let (slot, len) = c.rx.poll_used()?;
            let buf_idx = c.desc_to_rx[slot.id() as usize] as usize;
            c.rx_pending = Some(RxPending { buf_idx, slot, len, pos: 0 });
        }
        let p = c.rx_pending.as_mut().unwrap();
        // Bounded twice over: `read` refuses `pos + 1 > RX_BUF_SIZE`, and
        // `pos < len` is the loop condition below with `len` already bounded by
        // `poll_used` to the chain this driver published.
        let byte: u8 = c.rx_bufs[p.buf_idx].read(p.pos as usize);
        p.pos += 1;
        if p.pos >= p.len {
            let p = c.rx_pending.take().unwrap();
            refill_rx(c, p.buf_idx, p.slot);
        }
        Some(byte)
    })
    .flatten()
}

/// Caller must hold `serial::BackendGuard` with IRQs disabled.
pub fn has_data_locked() -> bool {
    with_console(|c| c.rx_pending.is_some() || c.rx.has_used()).unwrap_or(false)
}

pub fn init(devices: &[PciDevice]) -> bool {
    let pci_dev = match devices.iter().find(|d| d.is_id(VIRTIO_VENDOR, VIRTIO_CONSOLE_DEVICE)) {
        Some(d) => *d,
        None => {
            log!("virtio-console: no device found");
            return false;
        }
    };
    log!("virtio-console: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);

    // Leaked rather than held in a `static`: the console is never unbound, and
    // `Dma<'static>` says that where a `Lock<Option<DmaPool>>` nobody read only
    // implied it.
    let dma = DmaPool::alloc(DMA_SIZE).leak();

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    let mut rx = Virtqueue::new(dma.subview(OFF_RXVQ, 0x1000), QUEUE_SIZE);
    let mut tx = Virtqueue::new(dma.subview(OFF_TXVQ, 0x1000), QUEUE_SIZE);

    device.setup_queue(0, &mut rx);
    device.setup_queue(1, &mut tx);
    device.enable_queue(0);
    device.enable_queue(1);
    device.activate();

    let tx_buf = dma.subview(OFF_TX_BUF, TX_BUF_SIZE);

    let rx_bufs: [Dma<'static, Unaligned>; RX_BUF_COUNT] = core::array::from_fn(|i| {
        dma.subview(OFF_RX_BUFS + i * RX_BUF_SIZE as usize, RX_BUF_SIZE as usize).unaligned()
    });

    let mut tx_slots = tx.initial_slots();
    let tx_slot = tx_slots.pop().expect("vconsole: no tx slots");
    drop(tx_slots);

    let mut rx_slots = rx.initial_slots();

    let mut console = VConsole {
        device, rx, tx,
        tx_buf,
        tx_slot: Some(tx_slot),
        rx_bufs,
        desc_to_rx: [0; QUEUE_SIZE as usize],
        rx_pending: None,
    };

    for i in 0..RX_BUF_COUNT {
        let slot = rx_slots.pop().expect("vconsole: not enough rx slots");
        refill_rx(&mut console, i, slot);
    }
    drop(rx_slots);

    // SAFETY: irreducible — this is the write that initialises the
    // `MaybeUninit` payload of a `static`, and there is no safe form of it
    // without a `Sync` container the panic path may not take a lock on.
    // Sound because it happens exactly once: `init` is called once from the
    // boot sequence, before any AP is started and before `READY` is set, so
    // nothing can be reading `CONSOLE` — every reader goes through
    // `with_console`, which loads `READY` with `Acquire` and the `Release`
    // store below is what publishes this write to it.
    unsafe { (*CONSOLE.0.get()).write(console); }
    READY.store(true, Ordering::Release);
    crate::drivers::serial::console_changed();

    log!("virtio-console: initialized ({} RX bufs of {} bytes, TX buf {} bytes)",
        RX_BUF_COUNT, RX_BUF_SIZE, TX_BUF_SIZE);
    true
}
