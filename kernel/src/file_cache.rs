use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::sync::Arc;

use crate::block;
use crate::file_backing::FileBacking;
use crate::sync::Lock;

pub type FileId = u64;

const PAGE_SIZE: usize = 4096;

struct CachedPage {
    data: Box<[u8; PAGE_SIZE]>,
    dirty: bool,
    /// CLOCK's second-chance bit: set on every hit, cleared when the sweep
    /// passes it over.
    referenced: bool,
}

struct CachedFile {
    pages: BTreeMap<u32, CachedPage>,
    size: u64,
    evictable: bool,
    /// Where an evicted page comes back from. A file with no backing is one
    /// nothing can re-read — a tmpfs file, or a disk file created in this
    /// boot whose blocks the filesystem has not allocated yet — and dropping
    /// one of its pages loses the only copy.
    backing: Option<Arc<dyn FileBacking>>,
    ref_count: u32,
    deleted: bool,
}

impl CachedFile {
    /// Whether this file's pages are a *copy* of something on disk. Only
    /// those are governed by the budget and only those may be evicted:
    /// a tmpfs page is the file, not a cache of it.
    fn is_cache(&self) -> bool {
        self.evictable && self.backing.is_some()
    }
}

struct FileCache {
    files: BTreeMap<FileId, CachedFile>,
    next_id: u64,
    /// Resident pages belonging to files that satisfy `is_cache`.
    cached_pages: usize,
    max_pages: usize,
    evictions: u64,
    /// CLOCK hand, in (file, page) key order. Kept across calls so the sweep
    /// costs one step per eviction rather than a scan of the whole cache.
    hand: (FileId, u32),
}

static FILE_CACHE: Lock<FileCache> = Lock::new(FileCache {
    files: BTreeMap::new(),
    next_id: 1,
    cached_pages: 0,
    // Zero, not `usize::MAX`: a budget that was never installed has to be a
    // loud kernel bug, and this one shipped for the life of the boot as a
    // ceiling nothing could reach.
    max_pages: 0,
    evictions: 0,
    hand: (0, 0),
});

/// Install the memory budget. Must run after the PMM knows how much RAM the
/// machine has and before any file is opened.
pub fn init() {
    let max_pages = block::file_cache_pages();
    FILE_CACHE.lock().max_pages = max_pages;
    log!("file cache: budget {} pages ({} MiB)", max_pages, max_pages * PAGE_SIZE / (1024 * 1024));
}

/// Allocate a new FileId. The file cache is the sole allocator.
pub fn create_file(evictable: bool) -> FileId {
    let mut cache = FILE_CACHE.lock();
    let id = cache.next_id;
    cache.next_id += 1;
    cache.files.insert(id, CachedFile {
        pages: BTreeMap::new(),
        size: 0,
        evictable,
        backing: None,
        ref_count: 1,
        deleted: false,
    });
    id
}

/// Point a file at the store its evicted pages come back from. Idempotent:
/// every open of the same file hands over an equivalent backing.
pub fn set_backing(file_id: FileId, backing: Arc<dyn FileBacking>) {
    let mut cache = FILE_CACHE.lock();
    let now_governed;
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        let was_cache = file.is_cache();
        file.backing = Some(backing);
        now_governed = if !was_cache && file.is_cache() { file.pages.len() } else { 0 };
    }
    cache.cached_pages += now_governed;
    evict_if_needed(&mut cache);
}

/// Whether an evicted page of this file could be read back. False for tmpfs,
/// and for a disk file created in this boot until its blocks exist.
pub fn has_backing(file_id: FileId) -> bool {
    FILE_CACHE.lock().files.get(&file_id).is_some_and(|f| f.backing.is_some())
}

/// Increment ref_count for an open fd.
pub fn open(file_id: FileId) {
    let mut cache = FILE_CACHE.lock();
    if let Some(file) = cache.files.get_mut(&file_id) {
        file.ref_count += 1;
    }
}

/// Decrement ref_count. Returns true if this was the last reference.
pub fn release(file_id: FileId) -> bool {
    let mut cache = FILE_CACHE.lock();
    let Some(file) = cache.files.get_mut(&file_id) else { return false };
    file.ref_count = file.ref_count.saturating_sub(1);
    if file.ref_count == 0 {
        if file.deleted || file.evictable {
            drop_file(&mut cache, file_id);
        }
        // Non-evictable (tmpfs) files with ref_count 0 keep their pages.
        true
    } else {
        false
    }
}

/// Read from a file page into `buf`. Handles cache miss via the file's backing.
/// Lock is NOT held during disk I/O (unlock-fetch-relock pattern).
pub fn read_page(file_id: FileId, page_idx: u32, offset: usize, buf: &mut [u8]) {
    let backing;
    {
        let mut cache = FILE_CACHE.lock();
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        let file_size = file.size;

        // Beyond file size: zero-fill, no cache insert.
        if (page_idx as u64) * PAGE_SIZE as u64 >= file_size {
            buf.fill(0);
            return;
        }

        if let Some(page) = file.pages.get_mut(&page_idx) {
            page.referenced = true;
            let avail = valid_bytes_in_page(page_idx, file_size);
            copy_page_region_to_buf(&page.data[..], offset, buf, avail);
            return;
        }
        backing = file.backing.clone();
    }
    // Cache miss: unlock, fetch from backing, re-lock, insert if still absent.

    let mut fetched = blank_page();
    if let Some(backing) = &backing {
        // A fetch that failed must not become a resident page. `buf` gets the
        // zeros either way — this path has no error channel and adding one is
        // an ABI change — but caching them would let the next partial write
        // through `write_page` find the page resident, merge into the zeros
        // and flush them back over the file.
        if backing.read_page(page_idx as u64 * PAGE_SIZE as u64, &mut fetched).is_err() {
            buf.fill(0);
            return;
        }
    }
    // else: tmpfs miss → zero-filled page (fetched is already zeroed)

    let mut cache = FILE_CACHE.lock();
    let mut added = 0;
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        let is_cache = file.is_cache();
        if !file.pages.contains_key(&page_idx) {
            file.pages.insert(page_idx, CachedPage::new(fetched));
            added = is_cache as usize;
        }
        let file_size = file.size;
        let page = file.pages.get_mut(&page_idx).unwrap();
        page.referenced = true;
        let avail = valid_bytes_in_page(page_idx, file_size);
        copy_page_region_to_buf(&page.data[..], offset, buf, avail);
    }
    cache.cached_pages += added;
    evict_if_needed(&mut cache);
}

/// Write data into a file page. Handles cache miss via the file's backing.
/// Lock is NOT held during disk I/O for cache misses.
///
/// `Err` means the page could not be re-read and **nothing was written**. A
/// partial write into a page the cache does not hold has to fetch the bytes it
/// is not overwriting; if that fetch fails, the only two options are to merge
/// into zeros — which `flush_file` then persists, destroying 4 KiB of a file
/// that was fine — or to refuse. It refuses. The caller decides what to do
/// about a write that did not happen, which is a decision this layer does not
/// have the standing to make silently.
pub fn write_page(
    file_id: FileId,
    page_idx: u32,
    offset: usize,
    data: &[u8],
) -> Result<(), block::BlockError> {
    // A resident page is written under the acquisition that found it. The
    // fetch path below drops the lock, and a sibling CPU's eviction inside
    // that window would otherwise leave the write merging into a blank page.
    let backing;
    {
        let mut cache = FILE_CACHE.lock();
        {
            let Some(file) = cache.files.get_mut(&file_id) else { return Ok(()) };
            if file.pages.contains_key(&page_idx) {
                apply_write(file, page_idx, offset, data);
                backing = None;
            } else {
                backing = Some(file.backing.clone());
            }
        }
        if backing.is_none() {
            evict_if_needed(&mut cache);
            return Ok(());
        }
    }
    let backing = backing.unwrap();

    let mut fetched = blank_page();
    if let Some(backing) = &backing {
        let page_start = page_idx as u64 * PAGE_SIZE as u64;
        // Past the end there is nothing to preserve, so no fetch and no way
        // for one to fail: this is a pure extension of the file.
        if page_start < backing.file_size() {
            backing.read_page(page_start, &mut fetched)?;
        }
    }

    // Re-fetching after a sibling's eviction is always correct: only clean
    // pages are ever evicted, and a clean page is by definition what the
    // backing returns.
    let mut cache = FILE_CACHE.lock();
    let mut added = 0;
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return Ok(()) };
        if !file.pages.contains_key(&page_idx) {
            file.pages.insert(page_idx, CachedPage::new(fetched));
            added = file.is_cache() as usize;
        }
        apply_write(file, page_idx, offset, data);
    }
    cache.cached_pages += added;
    evict_if_needed(&mut cache);
    Ok(())
}

fn apply_write(file: &mut CachedFile, page_idx: u32, offset: usize, data: &[u8]) {
    let page = file.pages.get_mut(&page_idx).expect("write_page: page not resident");
    let end = (offset + data.len()).min(PAGE_SIZE);
    page.data[offset..end].copy_from_slice(&data[..end - offset]);
    page.dirty = true;
    page.referenced = true;

    let write_end = page_idx as u64 * PAGE_SIZE as u64 + end as u64;
    if write_end > file.size {
        file.size = write_end;
    }
}

/// Copy a resident page out. `false`, and `buf` untouched, when the page is not
/// resident.
///
/// The answer is a return value and not a zero-filled buffer because the two
/// callers want opposite things from an absent page: a tmpfs read is looking at
/// a hole, and a flush is looking at a page a truncate took away between
/// `clone_dirty` and here. Zeros would be a page of data to both of them, and
/// the flush would put them on the device.
#[must_use]
pub fn copy_page_out(file_id: FileId, page_idx: u32, buf: &mut [u8; PAGE_SIZE]) -> bool {
    let cache = FILE_CACHE.lock();
    let Some(file) = cache.files.get(&file_id) else { return false };
    let Some(page) = file.pages.get(&page_idx) else { return false };
    *buf = *page.data;
    true
}

/// Clone the dirty set (non-destructive). Used by fsync to iterate.
pub fn clone_dirty(file_id: FileId) -> BTreeSet<u32> {
    let cache = FILE_CACHE.lock();
    cache.files.get(&file_id)
        .map(|f| f.pages.iter().filter(|(_, p)| p.dirty).map(|(&i, _)| i).collect())
        .unwrap_or_default()
}

/// Mark the pages a flush actually wrote as clean.
///
/// Only those: the flush drops the lock between reading the dirty set and
/// writing each page, so a page dirtied in that window has not reached disk.
/// Clearing the whole file marks it clean, and a clean page is one eviction
/// is free to drop — which turns a lost write into a silent one.
pub fn clear_dirty(file_id: FileId, flushed: &BTreeSet<u32>) {
    let mut cache = FILE_CACHE.lock();
    if let Some(file) = cache.files.get_mut(&file_id) {
        for page_idx in flushed {
            if let Some(page) = file.pages.get_mut(page_idx) {
                page.dirty = false;
            }
        }
    }
}

/// Get the authoritative file size.
pub fn size(file_id: FileId) -> u64 {
    FILE_CACHE.lock().files.get(&file_id).map_or(0, |f| f.size)
}

/// Set file size. Removes pages past the new size on truncation.
pub fn set_size(file_id: FileId, new_size: u64) {
    let mut cache = FILE_CACHE.lock();
    let dropped;
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        dropped = if new_size < file.size {
            let is_cache = file.is_cache();
            let first_removed = (new_size as usize).div_ceil(PAGE_SIZE) as u32;
            let removed: alloc::vec::Vec<u32> = file.pages.range(first_removed..)
                .map(|(&k, _)| k).collect();
            for k in &removed {
                file.pages.remove(k);
            }
            if is_cache { removed.len() } else { 0 }
        } else {
            0
        };
        file.size = new_size;
    }
    cache.cached_pages -= dropped;
}

/// What the cache holds for a file after an operation that may have freed it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Open fds still hold it, so its pages and its id are still live.
    Held,
    /// The cache holds nothing for this id, and a filesystem may drop whatever
    /// it keeps alongside.
    Gone,
}

/// Mark a file as deleted (unlink). If no fds hold it, free immediately.
///
/// The verdict is returned rather than left to be re-derived from a refcount,
/// because a refcount read after the unlock is a different question asked at a
/// different moment: every caller wants to know what *this* unlink did.
#[must_use]
pub fn mark_deleted(file_id: FileId) -> Residency {
    let mut cache = FILE_CACHE.lock();
    let Some(file) = cache.files.get_mut(&file_id) else { return Residency::Gone };
    file.deleted = true;
    if file.ref_count > 0 {
        return Residency::Held;
    }
    drop_file(&mut cache, file_id);
    Residency::Gone
}

impl CachedPage {
    fn new(data: Box<[u8; PAGE_SIZE]>) -> Self {
        Self { data, dirty: false, referenced: false }
    }
}

/// A blank page, built on the heap and never on the stack.
///
/// Both miss paths below used `[0u8; PAGE_SIZE]` and handed it to `Box::new`,
/// which is a 4 KiB stack frame and a 4 KiB copy per miss. The copy was waste;
/// the frame was a hazard, because `log_file` reaches `write_page` from the idle
/// loop, whose per-CPU stack is 16 KiB of ordinary heap with no guard page.
/// Measured there, at the block layer with the USB command path still below:
/// 11,505 bytes of the 16,384 with these two frames present, 6,209 without.
fn blank_page() -> Box<[u8; PAGE_SIZE]> {
    match alloc::vec![0u8; PAGE_SIZE].into_boxed_slice().try_into() {
        Ok(page) => page,
        Err(_) => unreachable!("a PAGE_SIZE slice is a [u8; PAGE_SIZE]"),
    }
}

fn drop_file(cache: &mut FileCache, file_id: FileId) {
    let Some(removed) = cache.files.remove(&file_id) else { return };
    if removed.is_cache() {
        cache.cached_pages -= removed.pages.len();
    }
}

fn valid_bytes_in_page(page_idx: u32, file_size: u64) -> usize {
    let page_start = page_idx as u64 * PAGE_SIZE as u64;
    if page_start >= file_size {
        0
    } else {
        ((file_size - page_start) as usize).min(PAGE_SIZE)
    }
}

fn copy_page_region_to_buf(page: &[u8], offset: usize, buf: &mut [u8], valid: usize) {
    let start = offset.min(valid);
    let end = (offset + buf.len()).min(valid);
    let count = end.saturating_sub(start);
    if count > 0 {
        buf[..count].copy_from_slice(&page[start..start + count]);
    }
    // Zero-fill remainder (past valid data or past file end).
    if count < buf.len() {
        buf[count..].fill(0);
    }
}

fn evict_if_needed(cache: &mut FileCache) {
    assert!(cache.max_pages != 0, "file cache used before init installed a budget");
    if cache.cached_pages <= cache.max_pages {
        return;
    }
    let before = cache.evictions;
    while cache.cached_pages > cache.max_pages {
        if !evict_one(cache) {
            // Everything resident is dirty. Write-back is the fd layer's job
            // (`vfs::flush_file` on fsync and on close), so the only bound on
            // dirty pages is the writer's un-flushed working set.
            break;
        }
    }
    // One line per full turnover of the cache, so the series scales with the
    // bound instead of with a number picked here: it is the only evidence from
    // outside the kernel that residency stays flat while evictions climb.
    let turnover = cache.max_pages as u64;
    if cache.evictions != before && (before == 0 || before / turnover != cache.evictions / turnover) {
        log!("file cache: {} evictions, {}/{} pages resident",
            cache.evictions, cache.cached_pages, cache.max_pages);
    }
}

/// One CLOCK step-and-evict. Returns false when a full revolution found no
/// page it was allowed to take.
fn evict_one(cache: &mut FileCache) -> bool {
    // Two passes over the resident set: the first may spend itself clearing
    // reference bits, the second then cannot find every candidate referenced.
    // `+ 2` covers the wrap step at each end.
    let steps = cache.cached_pages * 2 + 2;
    for _ in 0..steps {
        let Some((fid, idx)) = seek_hand(cache) else { return false };
        cache.hand = match idx.checked_add(1) {
            Some(next) => (fid, next),
            None => (fid + 1, 0),
        };

        {
            let Some(file) = cache.files.get_mut(&fid) else { continue };
            let Some(page) = file.pages.get_mut(&idx) else { continue };
            if page.dirty {
                continue;
            }
            if page.referenced {
                page.referenced = false;
                continue;
            }
            file.pages.remove(&idx);
        }
        cache.cached_pages -= 1;
        cache.evictions += 1;
        return true;
    }
    false
}

/// The first resident page at or after the hand, wrapping once.
fn seek_hand(cache: &mut FileCache) -> Option<(FileId, u32)> {
    if let Some(found) = page_at_or_after(cache, cache.hand) {
        return Some(found);
    }
    cache.hand = (0, 0);
    page_at_or_after(cache, cache.hand)
}

fn page_at_or_after(cache: &FileCache, from: (FileId, u32)) -> Option<(FileId, u32)> {
    for (&fid, file) in cache.files.range(from.0..) {
        // Whole files, not pages at a time: a tmpfs file's pages can never be
        // taken, and stepping through a large one would exhaust the sweep's
        // budget before it reached a page it was allowed to evict.
        if !file.is_cache() {
            continue;
        }
        let start = if fid == from.0 { from.1 } else { 0 };
        if let Some((&idx, _)) = file.pages.range(start..).next() {
            return Some((fid, idx));
        }
    }
    None
}
