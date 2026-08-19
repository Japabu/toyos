use alloc::boxed::Box;
use alloc::vec::Vec;
use crate::io_uring::RingId;
use crate::sync::Lock;
use toyos_abi::syscall::SyscallError;

pub use toyos_abi::net::NicInfo;

/// Hardware-agnostic network interface. Implement this for any NIC driver
/// (virtio-net, RTL8125, Intel i225, etc.) and register it with `net::register()`.
pub trait Nic: Send {
    fn has_packet(&self) -> bool;

    /// Poll for a received frame without copying. Returns (buf_index, frame_len).
    fn poll_rx(&mut self) -> Option<(usize, usize)> { None }
    /// Resubmit an RX buffer to the hardware after the frame has been consumed.
    ///
    /// `buf_index` is a raw syscall argument and the slot it names is only
    /// populated by a prior `poll_rx`, so both arms are untrusted input
    /// rather than driver invariants. The default refuses, so a driver that
    /// implements `poll_rx` and forgets this fails closed.
    fn refill_rx_buf(&mut self, _buf_index: usize) -> Result<(), SyscallError> {
        Err(SyscallError::NotSupported)
    }
    /// Submit the TX buffer to hardware. Frame data (with net header) must already be written.
    ///
    /// `total_len` becomes the DMA descriptor length verbatim. Callers must
    /// have bounded it by `tx_buf_len` — see `net::submit_tx`.
    fn submit_tx(&mut self, _total_len: usize) {}

    /// Size in bytes of the TX buffer userland writes into. The submitted
    /// length is a descriptor length starting at that buffer, so this is the
    /// only thing standing between a `u64` from userland and the device
    /// reading adjacent kernel memory onto the wire.
    fn tx_buf_len(&self) -> usize { 0 }
}

static NIC: Lock<Option<Box<dyn Nic>>> = Lock::new(None);
static NIC_INFO: Lock<Option<(NicInfo, crate::object::shm::Region)>> = Lock::new(None);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

/// Wake every thread blocked on an incoming frame.
pub fn wake_waiters() {
    crate::sched::waitqs::wake_all(&crate::sched::waitqs::NETWORK);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

pub fn register(nic: Box<dyn Nic>) {
    *NIC.lock() = Some(nic);
}

pub fn set_nic_info(info: NicInfo, dma: crate::object::shm::Region) {
    *NIC_INFO.lock() = Some((info, dma));
}

pub fn nic_info() -> Option<(NicInfo, crate::object::shm::Region)> {
    NIC_INFO.lock().clone()
}

pub fn has_packet() -> bool {
    NIC.lock().as_ref().is_some_and(|nic| nic.has_packet())
}

pub fn poll_rx() -> Option<(usize, usize)> {
    NIC.lock().as_mut().and_then(|nic| nic.poll_rx())
}

pub fn refill_rx_buf(buf_index: usize) -> Result<(), SyscallError> {
    let mut guard = NIC.lock();
    let Some(nic) = guard.as_mut() else { return Err(SyscallError::NotFound) };
    nic.refill_rx_buf(buf_index)
}

/// Hand the device the TX buffer's first `total_len` bytes.
///
/// The length arrives from userland as a bare `u64` and there is no pointer
/// and no copy on this path — the frame was written straight into the shared
/// DMA buffer — so the destination size cannot bound it the way a copy would.
/// Bounding it here is the only bound there is.
pub fn submit_tx(total_len: usize) -> Result<(), SyscallError> {
    let mut guard = NIC.lock();
    let Some(nic) = guard.as_mut() else { return Err(SyscallError::NotFound) };
    if total_len == 0 || total_len > nic.tx_buf_len() {
        return Err(SyscallError::InvalidArgument);
    }
    nic.submit_tx(total_len);
    Ok(())
}
