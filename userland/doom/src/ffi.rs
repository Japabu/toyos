/// Runs `body` with any panic stopped at the `extern "C"` frame it would
/// otherwise cross.
///
/// `extern "C"` has no unwind path, so a panic reaching one is an immediate
/// `abort` — the process dies whatever the panic was about, and on the T14 a
/// positional-audio update that could not be delivered took the game down and
/// the desktop with it. `absent` is the answer the callback already gives when
/// its subsystem is missing, which is a case every caller in doomgeneric
/// handles: `I_InitSound` moves on to the next sound module, and the rest go
/// through a `sound_module != NULL` guard.
pub fn boundary<T>(name: &str, absent: T, body: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("[doom] panic escaped {name}; answering as if the subsystem were absent");
            absent
        }
    }
}
