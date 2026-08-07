//! Where everything goes inside a thread's TLS allocation.
//!
//! x86-64 variant II, plus the DTV the kernel writes in front of it:
//!
//! ```text
//! [DTV] [alignment padding] [TLS data (.tdata + .tbss)] [TCB]
//!                            ^ data_start                ^ thread pointer
//! ```
//!
//! The linker computes `TPOFF = sym_offset - memsz` raw, so the thread pointer
//! sits at `data_start + memsz` and `data_start` must carry the largest
//! alignment any module asked for. Both of those are sums of numbers a file
//! declared, which is why every step here is checked and the whole thing is a
//! pure function: `dtv_bytes <= tls_start` is the property, and it used to be
//! an assertion in the kernel reached from a crafted `PT_TLS`.

/// A planned TLS allocation, in offsets from the base of one block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsBlock {
    /// Bytes to allocate, a whole number of granules.
    pub alloc_size: usize,
    /// Where the first module's TLS data begins. Aligned to the requested
    /// alignment, and never below `dtv_bytes`.
    pub tls_start: usize,
    /// Where the thread pointer goes: `tls_start + total_memsz`.
    pub tp_offset: usize,
}

/// Lay out one thread's TLS block, or `None` for a layout no allocation can
/// hold.
///
/// `align` is `PT_TLS`'s `p_align` — zero or one mean "no constraint" and
/// become 8. It has already been established a power of two no larger than
/// [`crate::MAX_TLS_ALIGN`] by [`crate::Layout::parse`], which is what makes
/// `!(align - 1)` a mask here.
///
/// `granule` is the allocation unit the block is rounded up to, and
/// `dtv_bytes` the fixed header the kernel writes at offset zero. Both are
/// kernel constants; everything else came out of a file.
pub fn plan(
    total_memsz: usize,
    align: usize,
    tcb_size: usize,
    dtv_bytes: usize,
    granule: usize,
) -> Option<TlsBlock> {
    debug_assert!(granule.is_power_of_two());
    if align != 0 && !align.is_power_of_two() {
        return None;
    }
    let align = if align > 1 { align } else { 8 };

    let block_size = total_memsz.checked_add(tcb_size)?;
    // The DTV goes at the start of this same allocation and the TLS data is
    // placed `align`-aligned above it, so both belong in the size. Sizing from
    // the block and the alignment alone left `tls_start` free to land inside
    // the DTV.
    let alloc_size = align_up(block_size.checked_add(dtv_bytes)?.checked_add(align)?, granule)?;

    // Rounding *down* by `align` loses less than `align`, and `align` was one
    // of the addends — so this is at least `dtv_bytes + 1` and the DTV can
    // never be overwritten by TLS data.
    let tls_start = (alloc_size - block_size) & !(align - 1);

    Some(TlsBlock {
        alloc_size,
        tls_start,
        tp_offset: tls_start + total_memsz,
    })
}

/// One module's placement within a combined block: cursor rounded to 16, then
/// advanced by the module's own size.
///
/// The exe and every startup library share one block, and each module's
/// `base_offset` is what its `TPOFF` relocations are computed against.
pub fn place_module(cursor: usize, memsz: usize) -> Option<(usize, usize)> {
    let base = if cursor > 0 { align_up(cursor, 16)? } else { 0 };
    Some((base, base.checked_add(memsz)?))
}

fn align_up(value: usize, granule: usize) -> Option<usize> {
    value.checked_add(granule - 1).map(|v| v & !(granule - 1))
}
