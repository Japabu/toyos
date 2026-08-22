use crate::device::IoError;

/// Everything that can go wrong, as data rather than a panic.
///
/// Exhaustive on purpose: an adapter mapping these to `SyscallError` should
/// stop compiling when a new one appears, rather than sweeping it into a
/// catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device refused a read or a write.
    Io,
    /// The boot sector is not a FAT32 boot sector: bad signature, a field
    /// outside its legal set, or a cluster count that makes this FAT12/FAT16.
    NotFat32,
    /// The boot sector describes a volume larger than the device.
    Truncated,
    /// A cluster chain is cyclic, runs off the end of the FAT, or is longer
    /// than the structure it belongs to can possibly be.
    CorruptChain,
    /// A directory's contents are not directory entries: a long-name run that
    /// does not terminate, a directory longer than
    /// [`MAX_DIR_ENTRIES`](crate::MAX_DIR_ENTRIES), an entry whose first
    /// cluster is out of range.
    CorruptDirectory,
    NotFound,
    AlreadyExists,
    /// A path component that is not the last named a file.
    NotADirectory,
    /// The operation is defined only for files and the target is a directory.
    IsADirectory,
    DirectoryNotEmpty,
    /// A name is empty, too long, or contains a byte FAT cannot store.
    InvalidName,
    /// No free cluster, or no free directory slot and no room to make one.
    NoSpace,
    /// Past FAT32's 4 GiB − 1 file size, which is the width of the size field
    /// in a directory entry and not a policy this crate could relax.
    TooLarge,
    /// A caller-supplied bound, or one of this crate's own, was reached. The
    /// operation did nothing — a listing short of the truth is worse than no
    /// listing, because a caller checking that a name is absent gets a
    /// confident wrong answer.
    LimitExceeded,
    /// The device's implementor refused on *its own* bound, before attempting
    /// anything ([`IoError::BudgetExpired`]).
    ///
    /// **The one variant here that is not a fact about the volume**, and the
    /// only one a caller may honestly answer by asking again: nothing was
    /// written, nothing is half done, and the next call finds the volume as
    /// this one left it. Every other variant describes something true of the
    /// medium or of the request, which will be just as true next time.
    BudgetExpired,
}

impl From<IoError> for Error {
    fn from(e: IoError) -> Self {
        match e {
            IoError::Device => Error::Io,
            IoError::BudgetExpired => Error::BudgetExpired,
        }
    }
}

impl Error {
    pub fn as_str(&self) -> &'static str {
        match self {
            Error::Io => "device I/O failed",
            Error::NotFat32 => "not a FAT32 volume",
            Error::Truncated => "volume larger than device",
            Error::CorruptChain => "corrupt cluster chain",
            Error::CorruptDirectory => "corrupt directory",
            Error::NotFound => "no such file or directory",
            Error::AlreadyExists => "already exists",
            Error::NotADirectory => "not a directory",
            Error::IsADirectory => "is a directory",
            Error::DirectoryNotEmpty => "directory not empty",
            Error::InvalidName => "invalid name",
            Error::NoSpace => "no space left on volume",
            Error::TooLarge => "file too large for FAT32",
            Error::LimitExceeded => "limit exceeded",
            Error::BudgetExpired => "the device would not answer in the caller's own budget",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}
