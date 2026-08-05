//! What the two clock syscalls answer, from inside the machine.
//!
//! The host stages the RTC with `-rtc base=` and reads the *name* of the file
//! the kernel writes, which is local time. Neither of those can see
//! `SYS_CLOCK_EPOCH`, which serves UTC — and the difference between the two is
//! the whole of the timezone question. This prints both so the host can put
//! them against the instant it set, in `tests/common/wallclock.rs`.
//!
//! What it asserts itself is only what holds on *every* machine, because it
//! runs on four of them: the shared boot, and the three whose clocks are staged
//! broken. A machine with no wall clock is not a failure here — printing that
//! it has none is the answer the host is checking for.

use std::time::Instant;

/// Enough calls that a per-call port handshake would be unmistakable, and few
/// enough that the loop itself is free. The kernel used to read the CMOS on
/// every one of these — two port accesses per register, after a wait on the
/// update-in-progress flag that could take a second.
const CALLS: u32 = 1000;

fn main() {
    let epoch = toyos::system::clock_epoch();
    let time = toyos::system::clock_realtime();

    // The two come from one anchor the kernel took at boot, so a machine that
    // has one and not the other has a kernel bug rather than a broken clock.
    // True on every machine this runs on, which is what makes it worth
    // asserting here rather than on the host.
    assert_eq!(
        epoch.is_some(),
        time.is_some(),
        "one clock syscall answered and the other did not: epoch={epoch:?} time={time:?}"
    );

    let (Some(epoch), Some(time)) = (epoch, time) else {
        println!("wall-clock: no epoch");
        return;
    };
    println!(
        "wall-clock: epoch={epoch} local={:02}:{:02}:{:02}",
        time.hours, time.minutes, time.seconds
    );

    let began = Instant::now();
    let mut last = 0;
    for _ in 0..CALLS {
        last = toyos::system::clock_epoch().expect("the clock answered once and then stopped");
    }
    let elapsed = began.elapsed();
    println!("wall-clock: {CALLS} calls in {}us, last={last}", elapsed.as_micros());

    // Monotonic-plus-offset cannot go backwards inside a boot.
    assert!(last >= epoch, "the wall clock went backwards: {epoch} then {last}");
}
