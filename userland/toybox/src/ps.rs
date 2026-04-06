use toyos_abi::system;

const HEADER: usize = system::SYSINFO_HEADER_SIZE;
const ENTRY: usize = system::SYSINFO_ENTRY_SIZE;

fn format_cpu_time(ns: u64) -> String {
    let total_ms = ns / 1_000_000;
    let secs = total_ms / 1000;
    let ms = total_ms % 1000;
    let mins = secs / 60;
    let secs = secs % 60;
    if mins > 0 {
        format!("{mins}:{secs:02}.{ms:03}")
    } else {
        format!("{secs}.{ms:03}")
    }
}

fn state_str(s: u8) -> &'static str {
    match s {
        0 => "R",
        1 => "R+",
        3 => "Z",
        _ => "S",
    }
}

pub fn main(_args: Vec<String>) {
    let mut buf = vec![0u8; HEADER + ENTRY * 128];
    let n = system::sysinfo(&mut buf);
    if n < HEADER {
        eprintln!("ps: sysinfo failed");
        return;
    }

    let uptime_ns = u64::from_le_bytes(buf[24..32].try_into().unwrap());

    println!("{:>5} {:>3} {:>5} {:>2} {:>8} {:>5} {:>5}  {}",
        "PID", "TID", "PPID", "S", "CPU", "%CPU", "MEM", "NAME");

    let mut pos = HEADER;
    while pos + ENTRY <= n {
        let pid = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap());
        let ppid = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
        let tid = u32::from_le_bytes(buf[pos + 8..pos + 12].try_into().unwrap());
        let state = buf[pos + 12];
        let is_thread = buf[pos + 13] != 0;
        let memory = u64::from_le_bytes(buf[pos + 16..pos + 24].try_into().unwrap());
        let cpu_ns = u64::from_le_bytes(buf[pos + 24..pos + 32].try_into().unwrap());
        let name_bytes = &buf[pos + 32..pos + 60];
        let name_len = name_bytes.iter().position(|&b| b == 0).unwrap_or(28);
        let name = std::str::from_utf8(&name_bytes[..name_len]).unwrap_or("?");

        let cpu_pct = if uptime_ns > 0 {
            (cpu_ns as f64 / uptime_ns as f64 * 100.0) as u32
        } else {
            0
        };

        let mem = if memory >= 1 << 20 {
            format!("{}M", memory >> 20)
        } else if memory >= 1 << 10 {
            format!("{}K", memory >> 10)
        } else {
            format!("{}B", memory)
        };

        let ppid_str = if ppid == u32::MAX { 0 } else { ppid };
        let kind = if is_thread { " (thread)" } else { "" };

        println!("{:>5} {:>3} {:>5} {:>2} {:>8} {:>4}% {:>5}  {}{}",
            pid, tid, ppid_str, state_str(state), format_cpu_time(cpu_ns),
            cpu_pct, mem, name, kind);

        pos += ENTRY;
    }
}
