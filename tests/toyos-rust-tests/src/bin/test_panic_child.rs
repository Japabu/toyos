fn main() {
    let action: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    toyos_abi::syscall::debug(action);
    // Should never get here — if we do, the kernel didn't kill us.
    eprintln!("ERROR: debug syscall returned, kernel did not kill the process");
    std::process::exit(0);
}
