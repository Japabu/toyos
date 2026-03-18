use std::cell::Cell;

/// const-initialized TLS — like cranelift's CURRENT_PASS
thread_local! {
    static CURRENT: Cell<u8> = const { Cell::new(0) };
}

/// Lazy-initialized TLS with a Box<dyn Trait> — like cranelift's PROFILER
trait MyTrait: Send {
    fn value(&self) -> u64;
}
struct MyImpl(u64);
impl MyTrait for MyImpl {
    fn value(&self) -> u64 { self.0 }
}

thread_local! {
    static LAZY_BOX: std::cell::RefCell<Box<dyn MyTrait>> =
        std::cell::RefCell::new(Box::new(MyImpl(42)));
}

/// Push a value to CURRENT, return old value (like cranelift timing start_pass)
#[no_mangle]
pub extern "C" fn mc_push(val: u8) -> u8 {
    // First touch the dependency's global (like log::max_level())
    tls_dep::bump_global();
    // Then access TLS
    CURRENT.with(|c| c.replace(val))
}

/// Pop (restore) CURRENT, return what it was (like cranelift timing Drop)
#[no_mangle]
pub extern "C" fn mc_pop(restore: u8) -> u8 {
    CURRENT.with(|c| c.replace(restore))
}

/// Access the lazy Box<dyn Trait> TLS (like cranelift's PROFILER access)
#[no_mangle]
pub extern "C" fn mc_lazy_value() -> u64 {
    LAZY_BOX.with(|r| r.borrow().value())
}

/// Get the dependency's global counter
#[no_mangle]
pub extern "C" fn mc_global_count() -> u64 {
    tls_dep::GLOBAL_COUNTER.load(std::sync::atomic::Ordering::Relaxed) as u64
}

/// Get dependency's TLS value
#[no_mangle]
pub extern "C" fn mc_dep_tls() -> u64 {
    tls_dep::get_dep_tls()
}

/// Set dependency's TLS value
#[no_mangle]
pub extern "C" fn mc_dep_tls_set(v: u64) {
    tls_dep::set_dep_tls(v);
}
