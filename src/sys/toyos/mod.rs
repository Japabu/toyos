mod selector;
pub(crate) use self::selector::{event, Event, Events, Selector};

mod waker;
pub(crate) use self::waker::Waker;

cfg_io_source! {
    use std::io;

    pub(crate) struct IoSourceState;

    impl IoSourceState {
        pub fn new() -> IoSourceState {
            IoSourceState
        }

        pub fn do_io<T, F, R>(&self, f: F, io: &T) -> io::Result<R>
        where
            F: FnOnce(&T) -> io::Result<R>,
        {
            f(io)
        }
    }
}

pub(crate) const LISTEN_BACKLOG_SIZE: i32 = 128;
