use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// Fallback implementation for platforms without mmap.
/// Reads the file into a heap-allocated buffer instead.
pub struct MmapInner {
    data: Vec<u8>,
}

impl MmapInner {
    fn read_file(len: usize, file: &File, offset: u64) -> io::Result<MmapInner> {
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = vec![0u8; len];
        file.read_exact(&mut data)?;
        Ok(MmapInner { data })
    }

    pub fn map(len: usize, file: &File, offset: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Self::read_file(len, file, offset)
    }

    pub fn map_exec(len: usize, file: &File, offset: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Self::read_file(len, file, offset)
    }

    pub fn map_mut(len: usize, file: &File, offset: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Self::read_file(len, file, offset)
    }

    pub fn map_copy(len: usize, file: &File, offset: u64, _: bool, _: bool) -> io::Result<MmapInner> {
        Self::read_file(len, file, offset)
    }

    pub fn map_copy_read_only(
        len: usize,
        file: &File,
        offset: u64,
        _: bool,
        _: bool,
    ) -> io::Result<MmapInner> {
        Self::read_file(len, file, offset)
    }

    pub fn map_anon(len: usize, _: bool, _: bool, fill: Option<u8>, _: bool) -> io::Result<MmapInner> {
        let byte = fill.unwrap_or(0);
        Ok(MmapInner { data: vec![byte; len] })
    }

    pub fn flush(&self, _: usize, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub fn flush_async(&self, _: usize, _: usize) -> io::Result<()> {
        Ok(())
    }

    pub fn make_read_only(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn make_exec(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn make_mut(&mut self) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    pub fn ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    #[inline]
    pub fn mut_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }
}

pub fn file_len(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}
