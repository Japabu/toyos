pub use self::event_loop::{EventLoop, PlatformSpecificEventLoopAttributes};

#[allow(unused_macros)]
macro_rules! os_error {
    ($error:expr) => {{ winit_core::error::OsError::new(line!(), file!(), $error) }};
}

pub mod event_loop;
pub mod window;
