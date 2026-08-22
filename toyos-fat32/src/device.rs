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

/// The volume did not do it, and which of two kinds of "did not" it was.
///
/// **Two variants, because one of them is not about the device.** Everything a
/// device can say about itself — a transfer it refused, a controller that gave
/// up, a request past the end — is one fact this crate can act on none of; that
/// is [`IoError::Device`], and it carries no detail for the reason the single
/// unit struct this replaces carried none. But an implementor of
/// [`BlockAccess`] may also have a *bound of its own* on how long one call may
/// take, and reaching it is a statement about the caller's clock rather than
/// about the volume: nothing was attempted, nothing is in flight, and asking
/// again later is the honest response. Flattening the two costs the caller the
/// only decision it could make.
///
/// The kernel's implementor is `kernel/src/fat32_adapter.rs` over
/// `block::BlockDevice`, whose `block::OPERATION` budget is that bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoError {
    /// The device refused, failed, or would not answer.
    Device,
    /// The implementor's own bound on the operation expired before it was
    /// attempted. The volume is untouched and the caller may ask again.
    BudgetExpired,
}
