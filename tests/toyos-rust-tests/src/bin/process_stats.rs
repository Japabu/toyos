use std::process::Command;
use toyos_abi::Pid;
use toyos_abi::syscall::{ProcessStats, process_stats};

fn main() {
    test_basic_stats();
    test_stats_not_found();
    test_stats_consumed_once();
    println!("all process_stats tests passed");
}

fn test_basic_stats() {
    let mut child = Command::new("/bin/echo")
        .arg("hello")
        .spawn()
        .expect("failed to spawn echo");
    let child_pid = Pid(child.id());
    let status = child.wait().expect("failed to wait");
    assert!(status.success());

    let mut s = ProcessStats::default();
    process_stats(child_pid, &mut s).expect("process_stats should succeed for exited child");

    // wall_ns should be > 0 (process ran for some time)
    assert!(s.wall_ns > 0, "wall_ns should be > 0, got {}", s.wall_ns);
    // cpu_ns should be > 0
    assert!(s.cpu_ns > 0, "cpu_ns should be > 0, got {}", s.cpu_ns);
    // echo should have at least 1 syscall (write)
    assert!(s.syscall_total > 0, "syscall_total should be > 0, got {}", s.syscall_total);
    // echo should have at least 1 demand fault (loading the binary)
    assert!(s.fault_demand_count > 0 || s.fault_zero_count > 0,
        "should have at least one fault, got demand={} zero={}", s.fault_demand_count, s.fault_zero_count);
    // peak_memory should be > 0
    assert!(s.peak_memory > 0, "peak_memory should be > 0, got {}", s.peak_memory);

    println!("  basic stats: ok (wall={}ns cpu={}ns syscalls={} faults={} peak={})",
        s.wall_ns, s.cpu_ns, s.syscall_total,
        s.fault_demand_count + s.fault_zero_count, s.peak_memory);
}

fn test_stats_not_found() {
    let mut s = ProcessStats::default();
    let result = process_stats(Pid(99999), &mut s);
    assert!(result.is_err(), "should fail for non-existent pid");
    println!("  stats not found: ok");
}

fn test_stats_consumed_once() {
    let mut child = Command::new("/bin/echo")
        .arg("once")
        .spawn()
        .expect("failed to spawn");
    let child_pid = Pid(child.id());
    child.wait().expect("failed to wait");

    let mut s = ProcessStats::default();
    // First read should succeed
    process_stats(child_pid, &mut s).expect("first read should succeed");
    // Second read should fail (consumed)
    let result = process_stats(child_pid, &mut s);
    assert!(result.is_err(), "second read should fail (stats consumed)");
    println!("  stats consumed once: ok");
}
