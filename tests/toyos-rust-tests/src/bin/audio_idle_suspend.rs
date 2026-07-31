//! §5.8 idle-suspend certification: on a boot where no audio client ever
//! connects, soundd's CPU cost is exactly zero. Two sysinfo samples ~1s apart
//! must show no cpu_ns movement on any soundd thread — a suspended soundd
//! holds no timer and takes no wakes, so any nonzero delta is the mix or
//! control loop running without a reason. This is the one §5.8 claim gate A
//! structurally cannot see: its counters are streaming-scoped, and its boots
//! always connect a client. No wav analysis — there is no signal, and the
//! capture freezes while the voice is stopped anyway.

use toyos::system;

const HEADER: usize = system::SYSINFO_HEADER_SIZE;
const ENTRY: usize = system::SYSINFO_ENTRY_SIZE;

/// Sum of live cpu_ns over every soundd thread. The mix thread reports under
/// the process name ("soundd"), the control thread under its own
/// ("soundd-ctrl"); matching the prefix covers both and fails loudly if the
/// daemon is missing.
fn soundd_cpu_ns() -> u64 {
    let mut buf = vec![0u8; HEADER + ENTRY * 128];
    let n = system::sysinfo(&mut buf);
    assert!(n >= HEADER, "sysinfo failed");

    let mut total = 0u64;
    let mut threads = 0u32;
    let mut pos = HEADER;
    while pos + ENTRY <= n {
        let name_bytes = &buf[pos + 32..pos + 60];
        let len = name_bytes.iter().position(|&b| b == 0).unwrap_or(28);
        if name_bytes[..len].starts_with(b"soundd") {
            total += u64::from_le_bytes(buf[pos + 24..pos + 32].try_into().unwrap());
            threads += 1;
        }
        pos += ENTRY;
    }
    assert!(
        threads >= 2,
        "expected soundd's mix and control threads in sysinfo, found {threads}"
    );
    total
}

fn main() {
    let before = soundd_cpu_ns();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    let after = soundd_cpu_ns();
    assert_eq!(
        after,
        before,
        "soundd consumed {}ns of CPU across ~1s with no client — it is not suspended",
        after - before
    );
    println!("soundd idle cpu delta: 0ns over ~1s");
}
