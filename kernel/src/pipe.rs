use crate::sync::SyncCell;

const PIPE_BUF_SIZE: usize = 4096;
const MAX_PIPES: usize = 32;

struct Pipe {
    buffer: [u8; PIPE_BUF_SIZE],
    read_pos: usize,
    write_pos: usize,
    len: usize,
    readers: u32,
    writers: u32,
}

impl Pipe {
    const fn new() -> Self {
        Self {
            buffer: [0; PIPE_BUF_SIZE],
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 1,
            writers: 1,
        }
    }

    fn available(&self) -> usize {
        self.len
    }

    fn space(&self) -> usize {
        PIPE_BUF_SIZE - self.len
    }

    fn read(&mut self, buf: &mut [u8]) -> usize {
        let count = buf.len().min(self.len);
        for i in 0..count {
            buf[i] = self.buffer[self.read_pos];
            self.read_pos = (self.read_pos + 1) % PIPE_BUF_SIZE;
        }
        self.len -= count;
        count
    }

    fn write(&mut self, buf: &[u8]) -> usize {
        let count = buf.len().min(self.space());
        for i in 0..count {
            self.buffer[self.write_pos] = buf[i];
            self.write_pos = (self.write_pos + 1) % PIPE_BUF_SIZE;
        }
        self.len += count;
        count
    }
}

static PIPES: SyncCell<[Option<Pipe>; MAX_PIPES]> = SyncCell::new([const { None }; MAX_PIPES]);

/// Create a new pipe. Returns the pipe index, or `None` if the table is full.
pub fn create() -> Option<usize> {
    let table = PIPES.get_mut();
    for (i, slot) in table.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(Pipe::new());
            return Some(i);
        }
    }
    None
}

/// Read from a pipe. Returns bytes read, 0 for EOF.
/// Caller must handle blocking when this returns `None` (pipe empty but writers exist).
pub fn try_read(pipe_id: usize, buf: &mut [u8]) -> Option<usize> {
    let pipe = PIPES.get_mut()[pipe_id].as_mut()?;
    if pipe.available() > 0 {
        Some(pipe.read(buf))
    } else if pipe.writers == 0 {
        Some(0) // EOF
    } else {
        None // would block
    }
}

/// Write to a pipe. Returns bytes written.
/// Caller must handle blocking when this returns `None` (pipe full but readers exist).
/// Returns `Some(u64::MAX)` for broken pipe (no readers).
pub fn try_write(pipe_id: usize, buf: &[u8]) -> Option<usize> {
    let pipe = PIPES.get_mut()[pipe_id].as_mut()?;
    if pipe.readers == 0 {
        Some(usize::MAX) // broken pipe
    } else if pipe.space() > 0 {
        Some(pipe.write(buf))
    } else {
        None // would block
    }
}

/// Check if a pipe has data available to read.
pub fn has_data(pipe_id: usize) -> bool {
    PIPES.get_mut()[pipe_id]
        .as_ref()
        .map_or(false, |p| p.available() > 0 || p.writers == 0)
}

/// Increment the reader count (when duplicating a PipeRead descriptor).
pub fn add_reader(pipe_id: usize) {
    if let Some(pipe) = &mut PIPES.get_mut()[pipe_id] {
        pipe.readers += 1;
    }
}

/// Increment the writer count (when duplicating a PipeWrite descriptor).
pub fn add_writer(pipe_id: usize) {
    if let Some(pipe) = &mut PIPES.get_mut()[pipe_id] {
        pipe.writers += 1;
    }
}

/// Close the read end of a pipe.
pub fn close_read(pipe_id: usize) {
    let table = PIPES.get_mut();
    if let Some(pipe) = &mut table[pipe_id] {
        pipe.readers -= 1;
        if pipe.readers == 0 && pipe.writers == 0 {
            table[pipe_id] = None;
        }
    }
}

/// Close the write end of a pipe.
pub fn close_write(pipe_id: usize) {
    let table = PIPES.get_mut();
    if let Some(pipe) = &mut table[pipe_id] {
        pipe.writers -= 1;
        if pipe.readers == 0 && pipe.writers == 0 {
            table[pipe_id] = None;
        }
    }
}
