use toyos::system;

pub fn main(_args: Vec<String>) {
    let mut buf = [0u8; 48];
    let n = system::sysinfo(&mut buf);
    if n < 48 {
        eprintln!("free: sysinfo failed");
        return;
    }
    let total = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let used = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let free = total - used;

    println!("{:>12} {:>12} {:>12}", "total", "used", "free");
    println!("{:>11}M {:>11}M {:>11}M", total >> 20, used >> 20, free >> 20);
}
