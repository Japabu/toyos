use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::Hash;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
use crate::sys::time::SystemTime;

// Open flags (must match kernel)
const O_READ: u64 = 1;
const O_WRITE: u64 = 2;
const O_CREATE: u64 = 4;
const O_TRUNCATE: u64 = 8;

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "not supported on toyos"))
}

pub struct File(u64); // file descriptor

#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    file_type: u64, // 1 = regular file
}

pub struct ReadDir(!);

pub struct DirEntry(!);

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions {
    readonly: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    is_file: bool,
    is_dir: bool,
}

#[derive(Debug)]
pub struct DirBuilder {}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions { readonly: false }
    }

    pub fn file_type(&self) -> FileType {
        FileType {
            is_file: self.file_type == 1,
            is_dir: false,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::now())
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::now())
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::now())
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        self.is_file
    }

    pub fn is_symlink(&self) -> bool {
        false
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        self.0
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.0
    }

    pub fn file_name(&self) -> OsString {
        self.0
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        self.0
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        self.0
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) { self.read = read; }
    pub fn write(&mut self, write: bool) { self.write = write; }
    pub fn append(&mut self, append: bool) { self.append = append; }
    pub fn truncate(&mut self, truncate: bool) { self.truncate = truncate; }
    pub fn create(&mut self, create: bool) { self.create = create; }
    pub fn create_new(&mut self, create_new: bool) { self.create_new = create_new; }

    fn to_flags(&self) -> u64 {
        let mut flags = 0u64;
        if self.read { flags |= O_READ; }
        if self.write || self.append { flags |= O_WRITE; }
        if self.create || self.create_new { flags |= O_CREATE; }
        if self.truncate { flags |= O_TRUNCATE; }
        flags
    }
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let flags = opts.to_flags();
        let path_bytes = path.as_os_str().as_encoded_bytes();
        let fd = unsafe { crate::sys::toyos_open(path_bytes.as_ptr(), path_bytes.len(), flags) };
        if fd == u64::MAX {
            Err(io::Error::new(io::ErrorKind::NotFound, "file not found"))
        } else {
            Ok(File(fd))
        }
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let result = unsafe { crate::sys::toyos_fstat(self.0) };
        if result == u64::MAX {
            return Err(io::Error::new(io::ErrorKind::Other, "fstat failed"));
        }
        let file_type = result >> 32;
        let size = result & 0xFFFF_FFFF;
        Ok(FileAttr { size, file_type })
    }

    pub fn fsync(&self) -> io::Result<()> {
        unsafe { crate::sys::toyos_fsync(self.0) };
        Ok(())
    }

    pub fn datasync(&self) -> io::Result<()> {
        self.fsync()
    }

    pub fn lock(&self) -> io::Result<()> { Ok(()) }
    pub fn lock_shared(&self) -> io::Result<()> { Ok(()) }
    pub fn try_lock(&self) -> Result<(), TryLockError> { Ok(()) }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> { Ok(()) }
    pub fn unlock(&self) -> io::Result<()> { Ok(()) }

    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { crate::sys::toyos_read_file(self.0, buf.as_mut_ptr(), buf.len()) };
        if n == u64::MAX {
            Err(io::Error::new(io::ErrorKind::Other, "read failed"))
        } else {
            Ok(n as usize)
        }
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            match self.read(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) => if total == 0 { return Err(e) } else { break },
            }
        }
        Ok(total)
    }

    pub fn is_read_vectored(&self) -> bool { false }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let n = self.read(cursor.ensure_init().init_mut())?;
        cursor.advance(n);
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let n = unsafe { crate::sys::toyos_write_file(self.0, buf.as_ptr(), buf.len()) };
        if n == u64::MAX {
            Err(io::Error::new(io::ErrorKind::Other, "write failed"))
        } else {
            Ok(n as usize)
        }
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            match self.write(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) => if total == 0 { return Err(e) } else { break },
            }
        }
        Ok(total)
    }

    pub fn is_write_vectored(&self) -> bool { false }

    pub fn flush(&self) -> io::Result<()> {
        self.fsync()
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(n) => (n as i64, 0u64),
            SeekFrom::Current(n) => (n, 1u64),
            SeekFrom::End(n) => (n, 2u64),
        };
        let result = unsafe { crate::sys::toyos_seek(self.0, offset, whence) };
        if result == u64::MAX {
            Err(io::Error::new(io::ErrorKind::Other, "seek failed"))
        } else {
            Ok(result)
        }
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size))
    }

    pub fn tell(&self) -> io::Result<u64> {
        self.seek(SeekFrom::Current(0))
    }

    pub fn duplicate(&self) -> io::Result<File> {
        unsupported()
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        Ok(())
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { crate::sys::toyos_close(self.0) };
    }
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }

    pub fn mkdir(&self, _p: &Path) -> io::Result<()> {
        unsupported()
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "File({})", self.0)
    }
}

pub fn readdir(_p: &Path) -> io::Result<ReadDir> {
    unsupported()
}

pub fn unlink(_p: &Path) -> io::Result<()> {
    unsupported()
}

pub fn rename(_old: &Path, _new: &Path) -> io::Result<()> {
    unsupported()
}

pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    Ok(())
}

pub fn rmdir(_p: &Path) -> io::Result<()> {
    unsupported()
}

pub fn remove_dir_all(_path: &Path) -> io::Result<()> {
    unsupported()
}

pub fn exists(path: &Path) -> io::Result<bool> {
    let path_bytes = path.as_os_str().as_encoded_bytes();
    let fd = unsafe { crate::sys::toyos_open(path_bytes.as_ptr(), path_bytes.len(), O_READ) };
    if fd == u64::MAX {
        Ok(false)
    } else {
        unsafe { crate::sys::toyos_close(fd) };
        Ok(true)
    }
}

pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

pub fn stat(path: &Path) -> io::Result<FileAttr> {
    let path_bytes = path.as_os_str().as_encoded_bytes();
    let fd = unsafe { crate::sys::toyos_open(path_bytes.as_ptr(), path_bytes.len(), O_READ) };
    if fd == u64::MAX {
        return Err(io::Error::new(io::ErrorKind::NotFound, "file not found"));
    }
    let result = unsafe { crate::sys::toyos_fstat(fd) };
    unsafe { crate::sys::toyos_close(fd) };
    if result == u64::MAX {
        return Err(io::Error::new(io::ErrorKind::Other, "fstat failed"));
    }
    let file_type = result >> 32;
    let size = result & 0xFFFF_FFFF;
    Ok(FileAttr { size, file_type })
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p) // no symlinks on toyos
}

pub fn canonicalize(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn copy(_from: &Path, _to: &Path) -> io::Result<u64> {
    unsupported()
}
