use std::sync::atomic::{AtomicBool, Ordering};

fn main() {
    // 1. catch_unwind catches panic
    let result = std::panic::catch_unwind(|| {
        panic!("test panic");
    });
    assert!(result.is_err(), "catch_unwind should catch panic");
    println!("catch_unwind: ok");

    // 2. Drop runs during unwind
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::SeqCst);
        }
    }
    let _ = std::panic::catch_unwind(|| {
        let _g = Guard;
        panic!("drop test");
    });
    assert!(DROPPED.load(Ordering::SeqCst), "Drop should run during unwind");
    println!("drop during unwind: ok");

    // 3. Backtrace captures frames
    let bt = std::backtrace::Backtrace::force_capture();
    let s = format!("{bt}");
    assert!(!s.is_empty(), "backtrace should not be empty");
    println!("backtrace captured ({} bytes)", s.len());

    // 4. Panic in thread doesn't kill main
    let h = std::thread::spawn(|| {
        panic!("thread panic");
    });
    assert!(h.join().is_err(), "thread panic should be joinable as Err");
    println!("thread panic isolation: ok");

    println!("all unwind tests passed");
}
