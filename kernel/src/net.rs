use alloc::boxed::Box;
use crate::sync::Lock;

/// Hardware-agnostic network interface. Implement this for any NIC driver
/// (virtio-net, RTL8125, Intel i225, etc.) and register it with `net::register()`.
pub trait Nic: Send {
    fn mac(&self) -> [u8; 6];
    fn send(&mut self, frame: &[u8]);
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    fn has_packet(&self) -> bool;
}

static NIC: Lock<Option<Box<dyn Nic>>> = Lock::new(None);

pub fn register(nic: Box<dyn Nic>) {
    *NIC.lock() = Some(nic);
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
