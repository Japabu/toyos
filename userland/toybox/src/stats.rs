use std::process::Command;
use toyos_abi::Pid;
use toyos_abi::syscall::{ProcessStats, process_stats};

pub fn main(args: Vec<String>) {
    if args.is_empty() {
        eprintln!("usage: stats <command> [args...]");
        std::process::exit(1);
    }

    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("stats: failed to spawn {}: {e}", args[0]);
            std::process::exit(1);
        });

    let child_pid = Pid(child.id());
    let status = child.wait().unwrap_or_else(|e| {
        eprintln!("stats: wait failed: {e}");
        std::process::exit(1);
    });

    let mut s = ProcessStats::default();
    if process_stats(child_pid, &mut s).is_err() {
        eprintln!("stats: no stats for pid {}", child_pid.0);
        std::process::exit(1);
    }

    let code = status.code().unwrap_or(-1);
    eprintln!();
    eprintln!("── process stats (pid {}, exit {code}) ──────────", child_pid.0);
    eprintln!("  wall      {}", fmt_ns(s.wall_ns));
    eprintln!("  cpu       {}", fmt_ns(s.cpu_ns));
    eprintln!();

    let fault_total = s.fault_demand_count + s.fault_zero_count;
    if fault_total > 0 || s.fault_ns > 0 {
        eprintln!("  faults    {:>6}  ({} demand, {} zero)",
            fault_total, s.fault_demand_count, s.fault_zero_count);
        eprintln!("  fault time  {}", fmt_ns(s.fault_ns));
        eprintln!();
    }

    if s.io_read_ops > 0 {
        eprintln!("  io ops    {:>6}", s.io_read_ops);
        eprintln!("  io bytes  {}", fmt_bytes(s.io_read_bytes));
        eprintln!();
    }

    let blocked_total = s.blocked_io_ns + s.blocked_futex_ns + s.blocked_pipe_ns
        + s.blocked_ipc_ns + s.blocked_other_ns;
    if blocked_total > 0 {
        eprint!("  blocked   {}", fmt_ns(blocked_total));
        let mut parts = Vec::new();
        if s.blocked_io_ns > 0 { parts.push(format!("io {}", fmt_ns(s.blocked_io_ns))); }
        if s.blocked_futex_ns > 0 { parts.push(format!("futex {}", fmt_ns(s.blocked_futex_ns))); }
        if s.blocked_pipe_ns > 0 { parts.push(format!("pipe {}", fmt_ns(s.blocked_pipe_ns))); }
        if s.blocked_ipc_ns > 0 { parts.push(format!("ipc {}", fmt_ns(s.blocked_ipc_ns))); }
        if s.blocked_other_ns > 0 { parts.push(format!("other {}", fmt_ns(s.blocked_other_ns))); }
        if !parts.is_empty() {
            eprint!("  ({})", parts.join(", "));
        }
        eprintln!();
    }

    if s.runqueue_wait_ns > 0 {
        eprintln!("  runqueue  {}", fmt_ns(s.runqueue_wait_ns));
    }

    if blocked_total > 0 || s.runqueue_wait_ns > 0 {
        eprintln!();
    }

    eprintln!("  syscalls  {:>6}  (wall {})", s.syscall_total, fmt_ns(s.syscall_total_ns));
    eprintln!("  peak mem  {}  ({} allocs)", fmt_bytes(s.peak_memory), s.alloc_count);
}

fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.1}s", ns as f64 / 1_000_000_000.0)
    } else if ns >= 1_000_000 {
        format!("{:.1}ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.1}us", ns as f64 / 1_000.0)
    } else {
        format!("{}ns", ns)
    }
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1024 * 1024 * 1024 {
        format!("{:.1}GB", b as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024 * 1024 {
        format!("{:.1}MB", b as f64 / (1024.0 * 1024.0))
    } else if b >= 1024 {
        format!("{:.1}KB", b as f64 / 1024.0)
    } else {
        format!("{}B", b)
    }
}
