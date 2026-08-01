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
use core::ptr::copy_nonoverlapping;
use core::sync::atomic::{AtomicBool, Ordering};

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::log;

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

struct VConsole {
    device: VirtioDevice,
    rx: Virtqueue,
    tx: Virtqueue,
    tx_buf_phys: u64,
    tx_buf_ptr: *mut u8,
    tx_slot: Option<DescSlot>,
    rx_phys: [u64; RX_BUF_COUNT],
    rx_ptrs: [*mut u8; RX_BUF_COUNT],
    /// Maps virtqueue desc id → rx_buf index (filled at refill, read at poll).
    desc_to_rx: [u8; QUEUE_SIZE as usize],
    /// Currently-draining RX buffer (slot recovered from used ring but not
    /// yet refilled, because not all bytes have been consumed).
    rx_pending: Option<RxPending>,
}

unsafe impl Send for VConsole {}

struct ConsoleCell(UnsafeCell<MaybeUninit<VConsole>>);
unsafe impl Sync for ConsoleCell {}

/// Initialized exactly once in `init()` and then never written. Reads are
/// gated by `READY` (Acquire); mutation goes through `write_bytes_locked`,
/// `try_read_byte_locked`, and `has_data_locked`, all of which require the
/// caller to be holding `serial::BackendGuard` with interrupts disabled — that
/// outer lock is what serializes concurrent access to the VConsole state.
static CONSOLE: ConsoleCell = ConsoleCell(UnsafeCell::new(MaybeUninit::uninit()));
static READY: AtomicBool = AtomicBool::new(false);

/// Holds the DmaPool so its physical pages stay live for the device's
/// lifetime. Single-write at init, never read after.
static mut DMA_HOLDER: Option<DmaPool> = None;

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

#[inline]
unsafe fn console_mut() -> &'static mut VConsole {
    (*CONSOLE.0.get()).assume_init_mut()
}

fn refill_rx(c: &mut VConsole, buf_idx: usize, slot: DescSlot) {
    let desc_id = c.rx.submit(
        slot,
        &[(c.rx_phys[buf_idx], RX_BUF_SIZE, BufDir::Writable)],
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
    if !is_ready() { return; }
    let c = unsafe { console_mut() };
    let mut off = 0;
    while off < bytes.len() {
        let n = (bytes.len() - off).min(TX_BUF_SIZE);
        unsafe { copy_nonoverlapping(bytes.as_ptr().add(off), c.tx_buf_ptr, n); }
        let slot = c.tx_slot.take().expect("vconsole: no tx slot");
        c.tx_slot = Some(c.tx.submit_and_wait(
            slot,
            &[(c.tx_buf_phys, n as u32, BufDir::Readable)],
            c.device.notify_mmio(),
            c.device.notify_off_multiplier(),
            1,
        ));
        off += n;
    }
}

/// Read one byte from RX. Caller must hold `serial::BackendGuard` with IRQs disabled.
pub fn try_read_byte_locked() -> Option<u8> {
    if !is_ready() { return None; }
    let c = unsafe { console_mut() };
    if c.rx_pending.is_none() {
        let (slot, len) = c.rx.poll_used()?;
        let buf_idx = c.desc_to_rx[slot.id() as usize] as usize;
        c.rx_pending = Some(RxPending { buf_idx, slot, len, pos: 0 });
    }
    let p = c.rx_pending.as_mut().unwrap();
    let byte = unsafe { *c.rx_ptrs[p.buf_idx].add(p.pos as usize) };
    p.pos += 1;
    if p.pos >= p.len {
        let p = c.rx_pending.take().unwrap();
        refill_rx(c, p.buf_idx, p.slot);
    }
    Some(byte)
}

/// Caller must hold `serial::BackendGuard` with IRQs disabled.
pub fn has_data_locked() -> bool {
    if !is_ready() { return false; }
    let c = unsafe { console_mut() };
    c.rx_pending.is_some() || c.rx.has_used()
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

    let dma = DmaPool::alloc(DMA_SIZE);
    let dma_slice = dma.slice();
    unsafe { DMA_HOLDER = Some(dma); }

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    let mut rx = Virtqueue::new(dma_slice.subslice(OFF_RXVQ, 0x1000), QUEUE_SIZE);
    let mut tx = Virtqueue::new(dma_slice.subslice(OFF_TXVQ, 0x1000), QUEUE_SIZE);

    device.setup_queue(0, &mut rx);
    device.setup_queue(1, &mut tx);
    device.enable_queue(0);
    device.enable_queue(1);
    device.activate();

    let tx_buf = dma_slice.subslice(OFF_TX_BUF, TX_BUF_SIZE);
    let tx_buf_phys = tx_buf.phys();
    let tx_buf_ptr = tx_buf.base();

    let rx_phys: [u64; RX_BUF_COUNT] = core::array::from_fn(|i| {
        dma_slice.phys() + (OFF_RX_BUFS + i * RX_BUF_SIZE as usize) as u64
    });
    let rx_ptrs: [*mut u8; RX_BUF_COUNT] = core::array::from_fn(|i| {
        dma_slice.ptr_at(OFF_RX_BUFS + i * RX_BUF_SIZE as usize)
    });

    let mut tx_slots = tx.initial_slots();
    let tx_slot = tx_slots.pop().expect("vconsole: no tx slots");
    drop(tx_slots);

    let mut rx_slots = rx.initial_slots();

    let mut console = VConsole {
        device, rx, tx,
        tx_buf_phys, tx_buf_ptr,
        tx_slot: Some(tx_slot),
        rx_phys, rx_ptrs,
        desc_to_rx: [0; QUEUE_SIZE as usize],
        rx_pending: None,
    };

    for i in 0..RX_BUF_COUNT {
        let slot = rx_slots.pop().expect("vconsole: not enough rx slots");
        refill_rx(&mut console, i, slot);
    }
    drop(rx_slots);

    unsafe { (*CONSOLE.0.get()).write(console); }
    READY.store(true, Ordering::Release);

    log!("virtio-console: initialized ({} RX bufs of {} bytes, TX buf {} bytes)",
        RX_BUF_COUNT, RX_BUF_SIZE, TX_BUF_SIZE);
    true
}
