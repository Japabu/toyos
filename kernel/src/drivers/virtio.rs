use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use toyos_untrusted::{Refused, Untrusted};

use crate::mm::Mmio;
use super::pci::PciDevice;
use crate::mm::paging::CachePolicy;
use crate::log;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// Vendor-specific PCI capability ID
const PCI_CAP_ID_VENDOR: u8 = 0x09;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// Common config field offsets (virtio_pci_common_cfg)
pub const COMMON_DEVICE_FEATURE_SELECT: u64 = 0x00;
pub const COMMON_DEVICE_FEATURE: u64 = 0x04;
pub const COMMON_DRIVER_FEATURE_SELECT: u64 = 0x08;
pub const COMMON_DRIVER_FEATURE: u64 = 0x0C;
const COMMON_MSIX_CONFIG: u64 = 0x10;
pub const COMMON_DEVICE_STATUS: u64 = 0x14;
const COMMON_QUEUE_SELECT: u64 = 0x16;
pub const COMMON_QUEUE_SIZE: u64 = 0x18;
const COMMON_QUEUE_MSIX: u64 = 0x1A;
pub const COMMON_QUEUE_ENABLE: u64 = 0x1C;
pub const COMMON_QUEUE_NOTIFY_OFF: u64 = 0x1E;
pub const COMMON_QUEUE_DESC: u64 = 0x20;
pub const COMMON_QUEUE_DRIVER: u64 = 0x28;
pub const COMMON_QUEUE_DEVICE: u64 = 0x30;

/// What a virtio device reads back where it was written a vector it could not
/// allocate resources for (virtio 1.2 §4.1.5.1.2).
const NO_VECTOR: u16 = 0xFFFF;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// One descriptor, for a caller sizing a table it allocates itself.
pub const DESC_BYTES: usize = core::mem::size_of::<VirtqDesc>();

// Avail ring layout: flags(u16) + idx(u16) + ring[size](u16 each)
const AVAIL_IDX_OFF: usize = 2;
const AVAIL_RING_OFF: usize = 4;

// Used ring layout: flags(u16) + idx(u16) + ring[size](id:u32 + len:u32 each)
const USED_IDX_OFF: usize = 2;
const USED_RING_OFF: usize = 4;
const USED_ELEM_SIZE: usize = 8; // id(u32) + len(u32)

/// Parsed VirtIO PCI capability locations.
struct VirtioPciConfig {
    common: Mmio,
    notify: Mmio,
    notify_off_multiplier: u32,
    #[allow(dead_code)] // parsed from spec, used for interrupt-based operation
    isr: Mmio,
    device: Mmio,
}

impl VirtioPciConfig {
    fn parse(pci_dev: &PciDevice) -> Self {
        let mut common = None;
        let mut notify = None;
        let mut notify_off_multiplier = 0u32;
        let mut isr = None;
        let mut device = None;

        let mut mapped_bars: [Option<crate::mm::Mmio>; 6] = [None, None, None, None, None, None];
        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR { continue; }
            let bar_idx = cap.read_u8(4) as usize;
            if bar_idx < 6 && mapped_bars[bar_idx].is_none() {
                // The index is the device's, so the BAR it names may be
                // anything — including an I/O BAR, whose port number this used
                // to map as a physical address. A capability whose BAR is not
                // memory is skipped and the loop below then finds no window for
                // it; `parse`'s `expect`s are what say which one was missing.
                match pci_dev.memory_bar(bar_idx as u8) {
                    Ok(memory) => {
                        mapped_bars[bar_idx] = Some(crate::mm::paging::map_mmio(
                            memory.address(), 0x4000, CachePolicy::DeferToMtrr));
                    }
                    Err(why) => log!(
                        "VirtIO: PCI {:02x}:{:02x}.{} names BAR {bar_idx} and {why} — skipping \
                         every capability in it",
                        pci_dev.bus, pci_dev.dev, pci_dev.func),
                }
            }
        }

        for cap in pci_dev.capabilities() {
            if cap.id() != PCI_CAP_ID_VENDOR {
                continue;
            }
            let cfg_type = cap.read_u8(3);
            let bar_idx = cap.read_u8(4) as usize;
            let offset = cap.read_u32(8) as u64;
            let length = cap.read_u32(12) as u64;

            let Some(bar) = mapped_bars[bar_idx].as_ref() else { continue };
            let mmio = bar.subregion(offset, length.max(4));

            match cfg_type {
                VIRTIO_PCI_CAP_COMMON_CFG if common.is_none() => common = Some(mmio),
                VIRTIO_PCI_CAP_NOTIFY_CFG if notify.is_none() => {
                    notify = Some(mmio);
                    notify_off_multiplier = cap.read_u32(16);
                }
                VIRTIO_PCI_CAP_ISR_CFG if isr.is_none() => isr = Some(mmio),
                VIRTIO_PCI_CAP_DEVICE_CFG if device.is_none() => device = Some(mmio),
                _ => {}
            }
        }

        Self {
            common: common.expect("VirtIO: missing COMMON_CFG capability"),
            notify: notify.expect("VirtIO: missing NOTIFY_CFG capability"),
            notify_off_multiplier,
            isr: isr.expect("VirtIO: missing ISR_CFG capability"),
            device: device.expect("VirtIO: missing DEVICE_CFG capability"),
        }
    }
}

/// DMA region specification for a virtqueue.
use crate::mm::KernelSlice;

pub struct VirtqueueRegions {
    pub desc: KernelSlice,
    pub avail: KernelSlice,
    pub used: KernelSlice,
}

impl VirtqueueRegions {
    /// Compute regions from a single contiguous DMA buffer.
    pub fn from_contiguous(buf: KernelSlice, queue_size: u16) -> Self {
        let desc_size = queue_size as usize * core::mem::size_of::<VirtqDesc>();
        let avail_size = 4 + queue_size as usize * 2;
        let used_size = 4 + queue_size as usize * USED_ELEM_SIZE;
        let avail_off = (desc_size + 1) & !1;
        let used_off = (avail_off + avail_size + 3) & !3;
        Self {
            desc: buf.subslice(0, desc_size),
            avail: buf.subslice(avail_off, avail_size),
            used: buf.subslice(used_off, used_size),
        }
    }

    /// Compute regions from three separate DMA pages.
    pub fn from_separate(desc: KernelSlice, avail: KernelSlice, used: KernelSlice, queue_size: u16) -> Self {
        let desc_size = queue_size as usize * core::mem::size_of::<VirtqDesc>();
        let avail_size = 4 + queue_size as usize * 2;
        let used_size = 4 + queue_size as usize * USED_ELEM_SIZE;
        Self {
            desc: desc.subslice(0, desc_size),
            avail: avail.subslice(0, avail_size),
            used: used.subslice(0, used_size),
        }
    }
}

/// Proof that a descriptor slot is available for submission, **and that its
/// number indexes this queue**.
///
/// Non-Copy, non-Clone: must be obtained from `poll_used()` or
/// `initial_slots()`. Consumed by `submit()` — prevents overwriting in-flight
/// descriptors. `id()` is below `Virtqueue::size` for every slot either
/// constructor hands out, which is what lets a driver index a table sized by
/// its queue with it and not have to know that the number came off DMA.
pub struct DescSlot(u16);

impl DescSlot {
    pub fn id(&self) -> u16 { self.0 }
}

/// Why a used-ring element this driver read is not one it will act on.
///
/// **Both fields of a used-ring element are written by the device**, and for
/// virtio-sound's control and event queues the ring itself is in memory a
/// userland process maps writable — so neither number is evidence about
/// anything until it has been compared with what the driver published.
/// Refused rather than clamped: an element that names a descriptor this queue
/// does not have, or claims more bytes than the chain it names was given, is
/// not a completion with a bad field in it, and there is nothing to recover
/// from it.
/// Two of the three come straight out of [`Untrusted`]'s own exits — an id
/// that is not an index into the descriptor table, a length past the chain —
/// because those are the questions the type asks and there is no version of
/// this driver that gets to skip them. The third is a fact only this queue
/// holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsedRefusal {
    /// The head descriptor id is not an index into this queue's table.
    Head(Refused),
    /// The head names a descriptor this queue has published no chain at. A
    /// completion for a request that was never made.
    NoChain { id: u16 },
    /// The device claims to have written more bytes than the chain whose head
    /// this is was ever given. A driver that believes it reads past the buffer
    /// it posted.
    Written { id: u16, refused: Refused },
}

impl core::fmt::Display for UsedRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Head(refused) => write!(f, "its head descriptor {refused}"),
            Self::NoChain { id } => {
                write!(f, "a completion for descriptor {id}, where this queue published no chain")
            }
            Self::Written { id, refused } => {
                write!(f, "chain {id} was written {refused}")
            }
        }
    }
}

/// Interrupt-context consumer of a virtqueue's used ring, split off with
/// `Virtqueue::split_used_consumer`. Lock-free: reads only device-written
/// memory plus its own `last_used_idx`, so an ISR can drain completions
/// while another CPU submits to the same queue under a lock.
pub struct UsedRingConsumer {
    used: KernelSlice,
    size: u16,
    last_used_idx: u16,
    refused: u32,
}

impl UsedRingConsumer {
    /// Non-blocking poll: returns the head descriptor id of a completed
    /// chain, or `None` if nothing new.
    ///
    /// A head past the queue is skipped and counted, never returned — the
    /// caller indexes an array with it. Skipped rather than answered `None`,
    /// because `None` here means *the ring is empty* and one bad element must
    /// not hide the completions behind it.
    ///
    /// **Nothing here logs.** The only caller is an ISR, and the log backend
    /// takes a lock this context cannot wait on; [`refused`](Self::refused) is
    /// how the count is read from somewhere that can.
    pub fn poll(&mut self) -> Option<u16> {
        loop {
            let used_idx = unsafe { read_volatile(self.used.ptr_at(USED_IDX_OFF) as *const u16) };
            if used_idx == self.last_used_idx {
                return None;
            }
            // Acquire: the device wrote the used element before bumping the
            // used idx — pair with that ordering before reading the element.
            fence(Ordering::Acquire);
            let slot = self.last_used_idx % self.size;
            let id: Untrusted<u32> = Untrusted::new(unsafe {
                read_volatile(self.used.ptr_at(USED_RING_OFF + slot as usize * USED_ELEM_SIZE) as *const u32)
            });
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            let Ok(head) = id.index(self.size as usize) else {
                self.refused = self.refused.saturating_add(1);
                continue;
            };
            // Exact: `index` proved `head < self.size`, and `self.size` is a
            // `u16`.
            return Some(head as u16);
        }
    }

    /// How many used-ring elements this consumer has refused, for the life of
    /// the boot.
    pub fn refused(&self) -> u32 {
        self.refused
    }
}

/// A VirtIO split virtqueue.
pub struct Virtqueue {
    desc: KernelSlice,
    avail: KernelSlice,
    used: KernelSlice,
    size: u16,
    last_used_idx: u16,
    notify_offset: u16,
    used_split: bool,
    /// How many bytes the chain headed by each descriptor was given, written
    /// by [`Virtqueue::submit`] and [`Virtqueue::write_chain`] and read by
    /// [`Virtqueue::poll_used`].
    ///
    /// **The one number the device's `len` can be compared against.** The
    /// device reports how much it wrote; only the driver knows how much room
    /// it published, and without this the two are never compared. A descriptor
    /// no chain has been built at holds 0, so a completion for one is refused
    /// rather than believed.
    chain_bytes: alloc::vec::Vec<u32>,
    refused: u32,
}

/// Direction of a buffer in a descriptor chain.
pub enum BufDir {
    Readable,
    Writable,
}

/// How many bytes a chain gives the device to write into.
///
/// Saturating rather than wrapping: a chain whose lengths sum past `u32` is
/// one no caller in this tree builds, and a wrap would make the bound *smaller*
/// than the buffers actually posted, which refuses legal completions.
fn chain_bytes(bufs: &[(u64, u32, BufDir)]) -> u32 {
    bufs.iter().fold(0u32, |sum, (_, len, _)| sum.saturating_add(*len))
}

impl Virtqueue {
    /// Create a new virtqueue from a contiguous DMA region.
    pub fn new(buf: KernelSlice, queue_size: u16) -> Self {
        unsafe { buf.zero(); }
        Self::from_regions(&VirtqueueRegions::from_contiguous(buf, queue_size), queue_size)
    }

    /// Create a new virtqueue from explicit DMA regions.
    pub fn from_regions(regions: &VirtqueueRegions, queue_size: u16) -> Self {
        unsafe {
            regions.desc.zero();
            regions.avail.zero();
            regions.used.zero();
        }
        Self {
            desc: regions.desc,
            avail: regions.avail,
            used: regions.used,
            size: queue_size,
            last_used_idx: 0,
            notify_offset: 0,
            used_split: false,
            chain_bytes: alloc::vec![0u32; queue_size as usize],
            refused: 0,
        }
    }

    /// Hand the used ring to a dedicated (interrupt-context) consumer.
    /// Afterwards this queue is submit-only: `poll_used`/`has_used` panic,
    /// enforcing the single-consumer invariant at runtime.
    pub fn split_used_consumer(&mut self) -> UsedRingConsumer {
        assert!(!self.used_split, "virtqueue: used ring already split");
        self.used_split = true;
        UsedRingConsumer {
            used: self.used,
            size: self.size,
            last_used_idx: self.last_used_idx,
            refused: 0,
        }
    }

    /// Physical addresses for device register programming.
    pub fn descs_phys(&self) -> u64 { self.desc.phys() }
    pub fn avail_phys(&self) -> u64 { self.avail.phys() }
    pub fn used_phys(&self) -> u64 { self.used.phys() }

    /// Where in the notification region this queue's doorbell sits, in bytes.
    ///
    /// Read from the device by `setup_queue`, so it is meaningless before that.
    pub fn notify_bytes(&self, multiplier: u32) -> u64 {
        self.notify_offset as u64 * multiplier as u64
    }

    /// Write one descriptor chain and publish nothing.
    ///
    /// The half of [`submit`](Self::submit) that names memory, separated so a
    /// queue whose chains are fixed at bind can have them built once by the
    /// kernel and published by somebody who never sees a descriptor — the line
    /// that keeps a device address out of a driver's hands. Chains built this
    /// way are addressed by index rather than by a [`DescSlot`], because the
    /// proof a slot carries —
    /// that this descriptor is not in flight — is the publisher's to hold and
    /// the publisher is not here.
    pub fn write_chain(&mut self, first_desc: u16, bufs: &[(u64, u32, BufDir)]) {
        assert!(
            (first_desc as usize + bufs.len()) <= self.size as usize,
            "virtqueue: chain at {first_desc} of {} descriptors runs past a queue of {}",
            bufs.len(),
            self.size
        );
        self.chain_bytes[first_desc as usize] = chain_bytes(bufs);
        for (i, (addr, len, dir)) in bufs.iter().enumerate() {
            let desc_idx = first_desc + i as u16;
            let mut flags: u16 = match dir {
                BufDir::Readable => 0,
                BufDir::Writable => VIRTQ_DESC_F_WRITE,
            };
            if i != bufs.len() - 1 {
                flags |= VIRTQ_DESC_F_NEXT;
            }
            let desc = VirtqDesc { addr: *addr, len: *len, flags, next: desc_idx + 1 };
            let desc_ptr = self
                .desc
                .ptr_at(desc_idx as usize * core::mem::size_of::<VirtqDesc>())
                as *mut VirtqDesc;
            unsafe { write_volatile(desc_ptr, desc) };
        }
    }

    fn avail_idx_ptr(&self) -> *mut u16 {
        self.avail.ptr_at(AVAIL_IDX_OFF) as *mut u16
    }
    fn avail_ring_ptr(&self, i: u16) -> *mut u16 {
        self.avail.ptr_at(AVAIL_RING_OFF + i as usize * 2) as *mut u16
    }

    fn used_idx_ptr(&self) -> *const u16 {
        self.used.ptr_at(USED_IDX_OFF) as *const u16
    }

    /// Where the used element at ring position `i` sits.
    fn used_elem_at(&self, i: u16) -> usize {
        USED_RING_OFF + i as usize * USED_ELEM_SIZE
    }

    /// The two fields of a used element, as what they are: numbers the device
    /// wrote.
    ///
    /// **These are the only reads of the used ring in this driver**, and they
    /// hand back [`Untrusted`] rather than `u32`, so there is no expression
    /// anywhere below them that turns one into an index or a length without
    /// naming the bound. That is the difference between this bound and the
    /// hand-written ones it replaced: the next consumer of this queue does not
    /// have to know the rule, because the code that breaks it does not compile.
    fn used_ring_id(&self, i: u16) -> Untrusted<u32> {
        Untrusted::new(unsafe { read_volatile(self.used.ptr_at(self.used_elem_at(i)) as *const u32) })
    }
    fn used_ring_len(&self, i: u16) -> Untrusted<u32> {
        Untrusted::new(unsafe {
            read_volatile(self.used.ptr_at(self.used_elem_at(i) + 4) as *const u32)
        })
    }

    /// Return the initial pool of descriptor slots. Call once after construction.
    /// The caller manages these tokens — `submit()` consumes one, `poll_used()` returns one.
    pub fn initial_slots(&self) -> alloc::vec::Vec<DescSlot> {
        (0..self.size).map(DescSlot).collect()
    }

    /// Submit a descriptor chain and notify the device (non-blocking).
    /// Consumes a `DescSlot` proving a descriptor is available.
    /// Returns the descriptor index used (for caller bookkeeping).
    pub fn submit(
        &mut self,
        slot: DescSlot,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) -> u16 {
        let size = self.size;
        let first_desc = slot.0;
        self.chain_bytes[first_desc as usize] = chain_bytes(bufs);
        for (i, (addr, len, dir)) in bufs.iter().enumerate() {
            let desc_idx = (first_desc + i as u16) % size;
            let is_last = i == bufs.len() - 1;
            let next_idx = (desc_idx + 1) % size;

            let mut flags: u16 = match dir {
                BufDir::Readable => 0,
                BufDir::Writable => VIRTQ_DESC_F_WRITE,
            };
            if !is_last {
                flags |= VIRTQ_DESC_F_NEXT;
            }

            let desc = VirtqDesc { addr: *addr, len: *len, flags, next: next_idx };
            let desc_ptr = self.desc.ptr_at(desc_idx as usize * core::mem::size_of::<VirtqDesc>()) as *mut VirtqDesc;
            unsafe { write_volatile(desc_ptr, desc); }
        }

        let avail_idx = unsafe { read_volatile(self.avail_idx_ptr()) };
        unsafe {
            write_volatile(self.avail_ring_ptr(avail_idx % size), first_desc);
            fence(Ordering::Release);
            write_volatile(self.avail_idx_ptr(), avail_idx.wrapping_add(1));
        }

        fence(Ordering::Release);
        let notify_off = self.notify_offset as u64 * notify_multiplier as u64;
        notify_mmio.write_u16(notify_off, queue_index);

        first_desc
    }

    /// Check if the device has completed any request.
    pub fn has_used(&self) -> bool {
        assert!(!self.used_split, "virtqueue: used ring split off");
        let used_idx = unsafe { read_volatile(self.used_idx_ptr()) };
        used_idx != self.last_used_idx
    }

    /// Non-blocking poll of the used ring. Returns `(DescSlot, written_len)` if
    /// the device has completed a request, or `None` if nothing new.
    /// The returned `DescSlot` can be reused for a new submission.
    ///
    /// **This is the one place a device-chosen number enters this kernel, and
    /// it is parsed here so that no consumer has to know it should.** Both
    /// fields of the element are the device's, and on virtio-sound's control
    /// and event queues the ring is inside memory a userland process maps
    /// writable — so an id is compared with the queue's size and a length with
    /// the bytes the chain it names was published with. A refused element is
    /// counted and *skipped*, never returned: `None` means the ring is empty,
    /// and one forged element must not hide the completions queued behind it.
    ///
    /// The slot a refused element named is forfeit for the life of the boot —
    /// the safe direction, since a `DescSlot` is a token and losing one costs
    /// throughput where believing a bad one costs memory.
    ///
    /// **Nothing here logs.** `virtio_console::try_read_byte_locked` calls
    /// this while holding `serial::BackendGuard`, which is the lock the log
    /// backend itself takes; [`refused`](Self::refused) is how a caller in a
    /// context that *can* log reads the count.
    pub fn poll_used(&mut self) -> Option<(DescSlot, u32)> {
        assert!(!self.used_split, "virtqueue: used ring split off");
        loop {
            let used_idx = unsafe { read_volatile(self.used_idx_ptr()) };
            if used_idx == self.last_used_idx {
                return None;
            }
            fence(Ordering::Acquire);
            let slot = self.last_used_idx % self.size;
            let id = self.used_ring_id(slot);
            let len = self.used_ring_len(slot);
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            match self.parse_used(id, len) {
                Ok(elem) => return Some(elem),
                Err(_) => {
                    self.refused = self.refused.saturating_add(1);
                    continue;
                }
            }
        }
    }

    /// The whole of what a used-ring element has to satisfy, separated from
    /// the volatile reads that produce it so the self-test can run the shipped
    /// decision over elements no device on this host will write.
    fn parse_used(
        &self,
        id: Untrusted<u32>,
        len: Untrusted<u32>,
    ) -> Result<(DescSlot, u32), UsedRefusal> {
        // `chain_bytes` is exactly `size` long, so this is the descriptor
        // table's own bound and not a constant written out beside it. Exact
        // narrowing: `index` proved `head < size`, and `size` is a `u16`.
        let head = id.index(self.chain_bytes.len()).map_err(UsedRefusal::Head)?;
        let id = head as u16;
        let chain = self.chain_bytes[head];
        if chain == 0 {
            return Err(UsedRefusal::NoChain { id });
        }
        let written = len
            .at_most(chain as u64)
            .map_err(|refused| UsedRefusal::Written { id, refused })?;
        // Exact: `at_most` proved it is no more than `chain`, a `u32`.
        Ok((DescSlot(id), written as u32))
    }

    /// How many used-ring elements this queue has refused, for the life of the
    /// boot. A driver in a context that can log reads it and says so.
    pub fn refused(&self) -> u32 {
        self.refused
    }

    /// Write one used-ring element and bump the used index, the way a device
    /// does — the only writer of a used ring in this kernel, and it exists for
    /// [`used_selftest`] alone.
    ///
    /// `at` is what this queue's own `last_used_idx` will be when it reads the
    /// element. Compiled only into the actuator kernel: the shipping kernel
    /// has no way to write a used ring at all, which is what keeps
    /// [`used_ring_id`](Self::used_ring_id)'s claim — that every used-ring
    /// number arrives wrapped — true of the kernel anyone runs.
    #[cfg(feature = "boot-actuators")]
    fn write_used_as_a_device_would(&self, at: u16, id: u32, len: u32) {
        let slot = at % self.size;
        unsafe {
            write_volatile(self.used.ptr_at(self.used_elem_at(slot)) as *mut u32, id);
            write_volatile(self.used.ptr_at(self.used_elem_at(slot) + 4) as *mut u32, len);
            fence(Ordering::Release);
            write_volatile(self.used_idx_ptr() as *mut u16, at.wrapping_add(1));
        }
    }

    /// Submit a descriptor chain and wait for the device to complete it.
    /// Consumes a `DescSlot` and returns the one recovered from the used ring.
    pub fn submit_and_wait(
        &mut self,
        slot: DescSlot,
        bufs: &[(u64, u32, BufDir)],
        notify_mmio: Mmio,
        notify_multiplier: u32,
        queue_index: u16,
    ) -> DescSlot {
        self.submit(slot, bufs, notify_mmio, notify_multiplier, queue_index);
        loop {
            if let Some((slot, _)) = self.poll_used() {
                return slot;
            }
            core::hint::spin_loop();
        }
    }
}

/// Run [`Virtqueue::poll_used`] over eleven crafted used-ring elements.
///
/// **What nothing else on this host reaches.** Both fields of a used-ring
/// element are written by the device, and every virtio device QEMU implements
/// writes correct ones — there is no device property, machine property or
/// backend that makes one report a head descriptor it was never given or a
/// length past the buffer it was posted. So without this the refusals below
/// would ship never having executed, which is what
/// `issues/hardware/device-shape-and-lifecycle-have-no-coverage.md` asks a
/// driver-side bound to answer for.
///
/// Nothing is simulated: the queue is a real [`Virtqueue`] over a real DMA
/// page, the elements are written into its used ring the way a device writes
/// them, and what runs is the shipped `poll_used` including its volatile reads.
/// Only the writer of the ring is us instead of a device.
#[cfg(feature = "boot-actuators")]
pub fn used_selftest() {
    use super::DmaPool;

    const SIZE: u16 = 16;
    /// The chain the self-test publishes at descriptor 3, in bytes.
    const CHAIN: u32 = 256;
    /// A descriptor inside the queue that no chain was ever built at.
    const UNBUILT: u32 = 5;
    const CASES: usize = 11;

    let pool = DmaPool::alloc(0x1000);
    let mut q = Virtqueue::new(pool.slice().subslice(0, 0x1000), SIZE);
    q.write_chain(3, &[(pool.slice().phys(), CHAIN, BufDir::Writable)]);

    // Write one element where the device would write it and bump the index the
    // device bumps. `at` is what the queue's own `last_used_idx` will be.
    let publish = Virtqueue::write_used_as_a_device_would;

    /// One table row: name, head descriptor id, completion length, and what
    /// `poll_used` must answer for it (`None` means "must refuse").
    type Case = (&'static str, u32, u32, Option<(u16, u32)>);

    /// One element, and what `poll_used` must answer for it.
    const TABLE: [Case; 9] = [
        ("a chain the device filled", 3, CHAIN, Some((3, CHAIN))),
        ("a chain the device part-filled", 3, 1, Some((3, 1))),
        // A readable-only chain: the device wrote nothing into it and says so.
        ("a chain the device wrote nothing into", 3, 0, Some((3, 0))),
        ("a head past the queue", SIZE as u32, 0, None),
        // The one the `id as u16` cast could not see: 0x1_0003 narrows to 3, a
        // descriptor this queue really has, so a driver that truncated before
        // comparing would accept it.
        ("a head whose low 16 bits are in range", 0x1_0003, CHAIN, None),
        ("a head of every bit", u32::MAX, 0, None),
        ("one byte more than the chain", 3, CHAIN + 1, None),
        ("a length of every bit", 3, u32::MAX, None),
        ("a completion for a chain never published", UNBUILT, 0, None),
    ];

    let mut passed = 0usize;
    let mut at = 0u16;
    for (name, id, len, want) in TABLE {
        publish(&q, at, id, len);
        at = at.wrapping_add(1);
        let got = q.poll_used().map(|(slot, len)| (slot.id(), len));
        if got == want {
            passed += 1;
        } else {
            log!("virtio: used-ring selftest FAILED on {name}: got {got:?}, want {want:?}");
        }
    }

    // A completion behind a refused element is still delivered: `None` means
    // the ring is empty, and a device that forges one element must not be able
    // to hide the ones queued behind it.
    publish(&q, at, u32::MAX, u32::MAX);
    publish(&q, at.wrapping_add(1), 3, CHAIN);
    let got = q.poll_used().map(|(slot, len)| (slot.id(), len));
    if got == Some((3, CHAIN)) {
        passed += 1;
    } else {
        log!("virtio: used-ring selftest FAILED on a chain behind a refused element: got {got:?}");
    }

    // Eleven elements published, seven refused. The count is checked because
    // every case above would pass just as well against a `poll_used` that
    // refused correctly and counted nothing, and the count is what a driver in
    // a loggable context reports.
    let refused = q.refused();
    if refused != 7 {
        log!("virtio: used-ring selftest FAILED on the count: refused {refused}, want 7");
    } else {
        passed += 1;
    }
    log!("virtio: used-ring selftest {passed}/{CASES}");
}

/// Which of a device's two interrupt sources it declined to bind. Not a
/// driver bug and not a kernel bug: this device, on this machine, saying it
/// has no resources to deliver with.
#[derive(Debug, Clone, Copy)]
pub enum NoVector {
    Config,
    Queue(u16),
}

impl core::fmt::Display for NoVector {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Config => write!(f, "its configuration-change interrupt"),
            Self::Queue(queue) => write!(f, "queue {queue}'s interrupt"),
        }
    }
}

/// A fully initialized VirtIO device.
pub struct VirtioDevice {
    config: VirtioPciConfig,
}

impl VirtioDevice {
    /// Initialize a VirtIO PCI device: reset, negotiate features, prepare for queue setup.
    pub fn init(pci_dev: &PciDevice, accepted_features: u64) -> Self {
        pci_dev.enable_bus_master();

        let config = VirtioPciConfig::parse(pci_dev);
        let common = config.common;

        // 1. Reset
        common.write_u32(COMMON_DEVICE_STATUS, 0);
        while common.read_u32(COMMON_DEVICE_STATUS) != 0 {
            core::hint::spin_loop();
        }

        // 2. ACKNOWLEDGE
        common.write_u32(COMMON_DEVICE_STATUS, STATUS_ACKNOWLEDGE as u32);

        // 3. DRIVER
        common.write_u32(COMMON_DEVICE_STATUS,
            STATUS_ACKNOWLEDGE as u32 | STATUS_DRIVER as u32);

        // 4. Negotiate features
        // Read device features (low 32 bits)
        common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 0);
        let device_features_lo = common.read_u32(COMMON_DEVICE_FEATURE);
        // Read device features (high 32 bits)
        common.write_u32(COMMON_DEVICE_FEATURE_SELECT, 1);
        let device_features_hi = common.read_u32(COMMON_DEVICE_FEATURE);
        let device_features = (device_features_hi as u64) << 32 | device_features_lo as u64;

        let features = device_features & accepted_features;
        log!("VirtIO: device features={:#x} negotiated={:#x}", device_features, features);

        common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 0);
        common.write_u32(COMMON_DRIVER_FEATURE, features as u32);
        common.write_u32(COMMON_DRIVER_FEATURE_SELECT, 1);
        common.write_u32(COMMON_DRIVER_FEATURE, (features >> 32) as u32);

        // 5. FEATURES_OK
        let status = STATUS_ACKNOWLEDGE as u32 | STATUS_DRIVER as u32 | STATUS_FEATURES_OK as u32;
        common.write_u32(COMMON_DEVICE_STATUS, status);

        // 6. Verify FEATURES_OK stuck
        assert!(
            common.read_u32(COMMON_DEVICE_STATUS) & STATUS_FEATURES_OK as u32 != 0,
            "VirtIO: device rejected features"
        );

        Self { config }
    }

    /// Configure a virtqueue's addresses and size. Does NOT enable the queue —
    /// call `enable_queue()` after setting MSI-X vectors (if applicable).
    pub fn setup_queue(&self, index: u16, queue: &mut Virtqueue) {
        let common = self.config.common;

        common.write_u16(COMMON_QUEUE_SELECT, index);

        let max_size = common.read_u16(COMMON_QUEUE_SIZE);
        assert!(max_size >= queue.size, "VirtIO: queue {} too small (max={}, need={})", index, max_size, queue.size);
        common.write_u16(COMMON_QUEUE_SIZE, queue.size);

        common.write_u64(COMMON_QUEUE_DESC, queue.descs_phys());
        common.write_u64(COMMON_QUEUE_DRIVER, queue.avail_phys());
        common.write_u64(COMMON_QUEUE_DEVICE, queue.used_phys());

        queue.notify_offset = common.read_u16(COMMON_QUEUE_NOTIFY_OFF);
    }

    /// Enable a previously configured virtqueue.
    pub fn enable_queue(&self, index: u16) {
        let common = self.config.common;
        common.write_u16(COMMON_QUEUE_SELECT, index);
        common.write_u16(COMMON_QUEUE_ENABLE, 1);
    }

    /// Point this device's configuration-change interrupt and `queue`'s
    /// used-ring interrupt at `pci::MSIX_ENTRY` — the table entry
    /// `PciDevice::enable_msix` armed.
    ///
    /// Deliberately not part of that call: the table is PCI's and this is
    /// virtio's own protocol, and a device given the first without the second
    /// leaves every queue silent. Both halves are needed and neither implies
    /// the other, so a driver that wants interrupts makes both calls.
    pub fn bind_msix(&self, queue: u16) -> Result<(), NoVector> {
        let common = self.config.common;
        common.write_u16(COMMON_MSIX_CONFIG, super::pci::MSIX_ENTRY);
        if common.read_u16(COMMON_MSIX_CONFIG) == NO_VECTOR {
            return Err(NoVector::Config);
        }
        common.write_u16(COMMON_QUEUE_SELECT, queue);
        common.write_u16(COMMON_QUEUE_MSIX, super::pci::MSIX_ENTRY);
        if common.read_u16(COMMON_QUEUE_MSIX) == NO_VECTOR {
            return Err(NoVector::Queue(queue));
        }
        Ok(())
    }

    /// Set DRIVER_OK — device is now live.
    pub fn activate(&self) {
        let status = STATUS_ACKNOWLEDGE as u32
            | STATUS_DRIVER as u32
            | STATUS_FEATURES_OK as u32
            | STATUS_DRIVER_OK as u32;
        self.config.common.write_u32(COMMON_DEVICE_STATUS, status);
    }

    pub fn notify_mmio(&self) -> Mmio {
        self.config.notify
    }

    pub fn notify_off_multiplier(&self) -> u32 {
        self.config.notify_off_multiplier
    }

    pub fn device_config(&self) -> Mmio {
        self.config.device
    }
}
