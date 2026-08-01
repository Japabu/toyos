/// The volume this crate reads and writes.
///
/// Byte-addressed, and deliberately so. Two block sizes meet here and neither
/// belongs in this crate: the kernel's `BlockDevice` does I/O in 4096-byte
/// blocks, while an ESP's sectors are whatever its BPB says — usually 512. If
/// this trait spoke sectors, the implementor would have to know the BPB's
/// sector size to serve a request, and it cannot: parsing the BPB is what the
/// first call to this trait is *for*. Bytes are the only unit both sides agree
/// on before anything has been read.
///
/// So an implementor bridges to its own block size, including read-modify-write
/// for a partial block. `Fat32` reasons in BPB sectors and converts to byte
/// offsets at the boundary; nothing here assumes 512, or 4096, or any
/// particular alignment of a request.
///
/// Offsets are relative to the start of the volume, not the disk. A partition
/// is the implementor's business.
pub trait BlockAccess {
    /// Bytes in the volume. Used once, at mount, to reject a boot sector that
    /// describes more volume than exists.
    fn capacity(&self) -> u64;

    /// Fill `buf` from `offset`. Reading past [`capacity`](Self::capacity) is
    /// an [`IoError`], not a short read — this crate never asks for bytes it
    /// has not already bounded against the volume, so a truncated answer would
    /// mean a bug on one side or the other.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError>;

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), IoError>;

    /// Make every prior write durable.
    fn flush(&mut self) -> Result<(), IoError>;
}

/// The device could not do it.
///
/// Carries no detail because there is no detail this crate could act on: every
/// failure here becomes [`Error::Io`](crate::Error::Io) and propagates to a
/// caller that owns the device and already knows more about it than a code
/// could say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoError;
