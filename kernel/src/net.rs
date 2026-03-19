use alloc::boxed::Box;
use alloc::vec::Vec;
use crate::io_uring::RingId;
use crate::sync::Lock;

pub use toyos_abi::net::NicInfo;

/// Hardware-agnostic network interface. Implement this for any NIC driver
/// (virtio-net, RTL8125, Intel i225, etc.) and register it with `net::register()`.
pub trait Nic: Send {
    fn mac(&self) -> [u8; 6];
    fn send(&mut self, frame: &[u8]);
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    fn has_packet(&self) -> bool;

    /// Poll for a received frame without copying. Returns (buf_index, frame_len).
    fn poll_rx(&mut self) -> Option<(usize, usize)> { None }
    /// Resubmit an RX buffer to the hardware after the frame has been consumed.
    fn refill_rx_buf(&mut self, _buf_index: usize) {}
    /// Submit the TX buffer to hardware. Frame data (with net header) must already be written.
    fn submit_tx(&mut self, _total_len: usize) {}
}

static NIC: Lock<Option<Box<dyn Nic>>> = Lock::new(None);
static NIC_INFO: Lock<Option<NicInfo>> = Lock::new(None);
static IO_URING_WATCHERS: Lock<Vec<RingId>> = Lock::new(Vec::new());

pub fn add_io_uring_watcher(id: RingId) {
    let mut w = IO_URING_WATCHERS.lock();
    if !w.contains(&id) { w.push(id); }
}

pub fn remove_io_uring_watcher(id: RingId) {
    IO_URING_WATCHERS.lock().retain(|&x| x != id);
}

pub fn io_uring_watchers() -> Vec<RingId> {
    IO_URING_WATCHERS.lock().clone()
}

pub fn register(nic: Box<dyn Nic>) {
    *NIC.lock() = Some(nic);
}

pub fn set_nic_info(info: NicInfo) {
    *NIC_INFO.lock() = Some(info);
}

pub fn nic_info() -> Option<NicInfo> {
    *NIC_INFO.lock()
}

pub fn mac() -> Option<[u8; 6]> {
    NIC.lock().as_ref().map(|n| n.mac())
}

pub fn send(frame: &[u8]) {
    if let Some(nic) = NIC.lock().as_mut() {
        nic.send(frame);
    }
}

pub fn recv(buf: &mut [u8]) -> Option<usize> {
    NIC.lock().as_mut().and_then(|nic| nic.recv(buf))
}

pub fn has_packet() -> bool {
    NIC.lock().as_ref().map_or(false, |nic| nic.has_packet())
}

pub fn poll_rx() -> Option<(usize, usize)> {
    NIC.lock().as_mut().and_then(|nic| nic.poll_rx())
}

pub fn refill_rx_buf(buf_index: usize) {
    if let Some(nic) = NIC.lock().as_mut() {
        nic.refill_rx_buf(buf_index);
    }
}

pub fn submit_tx(total_len: usize) {
    if let Some(nic) = NIC.lock().as_mut() {
        nic.submit_tx(total_len);
    }
}
