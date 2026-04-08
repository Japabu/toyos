//! Raw Ethernet frame access (for the network daemon).

use toyos_abi::syscall;

pub fn mac_address() -> Option<[u8; 6]> {
    syscall::net_mac()
}

pub fn send_frame(frame: &[u8]) {
    syscall::net_send(frame);
}

pub fn recv_frame(buf: &mut [u8]) -> usize {
    syscall::net_recv(buf)
}

pub fn recv_frame_timeout(buf: &mut [u8], timeout_nanos: Option<u64>) -> usize {
    syscall::net_recv_timeout(buf, timeout_nanos)
}

pub fn nic_rx_poll() -> Option<(usize, usize)> {
    let v = syscall::nic_rx_poll();
    if v == 0 { None } else { Some(((v >> 16) as usize, (v & 0xFFFF) as usize)) }
}

pub fn nic_rx_done(buf_index: usize) {
    syscall::nic_rx_done(buf_index as u64);
}

pub fn nic_tx(total_len: usize) {
    syscall::nic_tx(total_len as u64);
}
