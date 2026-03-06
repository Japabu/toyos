use std::cell::Cell;

thread_local! {
    static COUNTER: Cell<u64> = const { Cell::new(0) };
    static LABEL: Cell<u64> = const { Cell::new(0xDEAD_BEEF) };
}

#[no_mangle]
pub extern "C" fn tls_increment() -> u64 {
    COUNTER.with(|c| {
        let val = c.get() + 1;
        c.set(val);
        val
    })
}

#[no_mangle]
pub extern "C" fn tls_get_counter() -> u64 {
    COUNTER.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn tls_get_label() -> u64 {
    LABEL.with(|c| c.get())
}

#[no_mangle]
pub extern "C" fn tls_set_label(val: u64) {
    LABEL.with(|c| c.set(val));
}
