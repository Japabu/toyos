//! Raw Ethernet frame access (for the network daemon).

use crate::syscall;

/// Get the MAC address of the network interface.
pub fn mac_address() -> Option<[u8; 6]> {
    syscall::net_mac()
}

/// Send a raw Ethernet frame.
pub fn send_frame(frame: &[u8]) {
    syscall::net_send(frame);
}

/// Receive a raw Ethernet frame. Blocks until a frame arrives.
/// Returns the number of bytes written to `buf`.
pub fn recv_frame(buf: &mut [u8]) -> usize {
    syscall::net_recv(buf)
}

/// Receive a raw Ethernet frame with a timeout in nanoseconds.
/// Returns the number of bytes written, or 0 on timeout.
pub fn recv_frame_timeout(buf: &mut [u8], timeout_nanos: Option<u64>) -> usize {
    syscall::net_recv_timeout(buf, timeout_nanos)
}

/// Poll for a received frame in the DMA buffer (zero-copy path).
/// Returns `Some((buf_index, frame_len))` or `None`.
pub fn nic_rx_poll() -> Option<(usize, usize)> {
    let v = syscall::nic_rx_poll();
    if v == 0 { None } else { Some(((v >> 16) as usize, (v & 0xFFFF) as usize)) }
}

/// Refill an RX DMA buffer after consuming the frame.
pub fn nic_rx_done(buf_index: usize) {
    syscall::nic_rx_done(buf_index as u64);
}

/// Submit the TX DMA buffer to hardware. `total_len` includes the net header.
pub fn nic_tx(total_len: usize) {
    syscall::nic_tx(total_len as u64);
}
