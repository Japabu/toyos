use std::sync::atomic::{AtomicUsize, Ordering};

/// A regular (non-TLS) global static — like log::MAX_LOG_LEVEL_FILTER.
/// If the linker overlaps this with TLS data, writing to it corrupts TLS.
pub static GLOBAL_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A thread_local from this dependency — like log::__private_api::STATE.
use std::cell::Cell;
thread_local! {
    pub static DEP_TLS_VALUE: Cell<u64> = const { Cell::new(0xBEEF) };
}

pub fn bump_global() -> usize {
    GLOBAL_COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub fn get_dep_tls() -> u64 {
    DEP_TLS_VALUE.with(|c| c.get())
}

pub fn set_dep_tls(v: u64) {
    DEP_TLS_VALUE.with(|c| c.set(v));
}
