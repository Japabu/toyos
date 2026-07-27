use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;

use crate::file_backing::FileBacking;
use crate::sync::Lock;

pub type FileId = u64;

const PAGE_SIZE: usize = 4096;

struct CachedFile {
    pages: BTreeMap<u32, Box<[u8; PAGE_SIZE]>>,
    dirty: BTreeSet<u32>,
    size: u64,
    evictable: bool,
    ref_count: u32,
    deleted: bool,
}

struct FileCache {
    files: BTreeMap<FileId, CachedFile>,
    next_id: u64,
    total_pages: usize,
    max_pages: usize,
}

static FILE_CACHE: Lock<FileCache> = Lock::new(FileCache {
    files: BTreeMap::new(),
    next_id: 1,
    total_pages: 0,
    max_pages: usize::MAX,
});

/// Initialize the file cache with a memory budget.
pub fn init(max_pages: usize) {
    FILE_CACHE.lock().max_pages = max_pages;
}

/// Allocate a new FileId. The file cache is the sole allocator.
pub fn create_file(evictable: bool) -> FileId {
    let mut cache = FILE_CACHE.lock();
    let id = cache.next_id;
    cache.next_id += 1;
    cache.files.insert(id, CachedFile {
        pages: BTreeMap::new(),
        dirty: BTreeSet::new(),
        size: 0,
        evictable,
        ref_count: 1,
        deleted: false,
    });
    id
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
            let removed = cache.files.remove(&file_id).unwrap();
            cache.total_pages -= removed.pages.len();
        }
        // Non-evictable (tmpfs) files with ref_count 0 keep their pages.
        true
    } else {
        false
    }
}

/// Read from a file page into `buf`. Handles cache miss via backing.
/// Lock is NOT held during disk I/O (unlock-fetch-relock pattern).
pub fn read_page(
    file_id: FileId,
    page_idx: u32,
    offset: usize,
    buf: &mut [u8],
    backing: Option<&dyn FileBacking>,
) {
    let file_size;
    {
        let cache = FILE_CACHE.lock();
        let Some(file) = cache.files.get(&file_id) else { return };
        file_size = file.size;

        // Beyond file size: zero-fill, no cache insert.
        if (page_idx as u64) * PAGE_SIZE as u64 >= file_size {
            buf.fill(0);
            return;
        }

        // Cache hit: copy out.
        if let Some(page) = file.pages.get(&page_idx) {
            let avail = valid_bytes_in_page(page_idx, file_size);
            copy_page_region_to_buf(&page[..], offset, buf, avail);
            return;
        }
    }
    // Cache miss: unlock, fetch from backing, re-lock, insert if still absent.

    let mut fetched = [0u8; PAGE_SIZE];
    if let Some(backing) = backing {
        backing.read_page(page_idx as u64 * PAGE_SIZE as u64, &mut fetched);
    }
    // else: tmpfs miss → zero-filled page (fetched is already zeroed)

    let mut cache = FILE_CACHE.lock();
    let mut inserted = false;
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        if !file.pages.contains_key(&page_idx) {
            file.pages.insert(page_idx, Box::new(fetched));
            inserted = true;
        }
        let page = &**file.pages.get(&page_idx).unwrap();
        let avail = valid_bytes_in_page(page_idx, file.size);
        copy_page_region_to_buf(page, offset, buf, avail);
    }
    if inserted { cache.total_pages += 1; }
    evict_if_needed(&mut cache);
}

/// Write data into a file page. Handles cache miss via backing.
/// Lock is NOT held during disk I/O for cache misses.
pub fn write_page(
    file_id: FileId,
    page_idx: u32,
    offset: usize,
    data: &[u8],
    backing: Option<&dyn FileBacking>,
) {
    // Check if page is already cached.
    let need_fetch;
    {
        let cache = FILE_CACHE.lock();
        let Some(file) = cache.files.get(&file_id) else { return };
        need_fetch = !file.pages.contains_key(&page_idx);
    }

    if need_fetch {
        // Determine if we need to load existing data (partial write to existing page).
        let page_start = page_idx as u64 * PAGE_SIZE as u64;
        let mut fetched = [0u8; PAGE_SIZE];
        if let Some(backing) = backing {
            let backing_size = backing.file_size();
            if page_start < backing_size {
                backing.read_page(page_start, &mut fetched);
            }
        }

        let mut cache = FILE_CACHE.lock();
        let mut inserted = false;
        if let Some(file) = cache.files.get_mut(&file_id) {
            if !file.pages.contains_key(&page_idx) {
                file.pages.insert(page_idx, Box::new(fetched));
                inserted = true;
            }
        }
        if inserted { cache.total_pages += 1; }
    }

    let mut cache = FILE_CACHE.lock();
    {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        let page = file.pages.get_mut(&page_idx).unwrap();
        let end = (offset + data.len()).min(PAGE_SIZE);
        page[offset..end].copy_from_slice(&data[..end - offset]);
        file.dirty.insert(page_idx);

        // Update size if write extends past current end.
        let write_end = page_idx as u64 * PAGE_SIZE as u64 + end as u64;
        if write_end > file.size {
            file.size = write_end;
        }
    }
    evict_if_needed(&mut cache);
}

/// Copy a full page out for flushing. Lock held only for the copy.
pub fn copy_page_out(file_id: FileId, page_idx: u32, buf: &mut [u8; PAGE_SIZE]) {
    let cache = FILE_CACHE.lock();
    if let Some(file) = cache.files.get(&file_id) {
        if let Some(page) = file.pages.get(&page_idx) {
            *buf = **page;
            return;
        }
    }
    buf.fill(0);
}

/// Clone the dirty set (non-destructive). Used by fsync to iterate.
pub fn clone_dirty(file_id: FileId) -> BTreeSet<u32> {
    let cache = FILE_CACHE.lock();
    cache.files.get(&file_id)
        .map(|f| f.dirty.clone())
        .unwrap_or_default()
}

/// Clear the dirty set after a successful flush.
pub fn clear_dirty(file_id: FileId) {
    let mut cache = FILE_CACHE.lock();
    if let Some(file) = cache.files.get_mut(&file_id) {
        file.dirty.clear();
    }
}

/// Get the authoritative file size.
pub fn size(file_id: FileId) -> u64 {
    FILE_CACHE.lock().files.get(&file_id).map_or(0, |f| f.size)
}

/// Set file size. Removes pages past the new size on truncation.
pub fn set_size(file_id: FileId, new_size: u64) {
    let mut cache = FILE_CACHE.lock();
    let n_removed = {
        let Some(file) = cache.files.get_mut(&file_id) else { return };
        if new_size < file.size {
            let first_removed = (new_size as usize + PAGE_SIZE - 1) / PAGE_SIZE;
            let removed: alloc::vec::Vec<u32> = file.pages.range(first_removed as u32..)
                .map(|(&k, _)| k).collect();
            let n = removed.len();
            for k in &removed {
                file.pages.remove(k);
                file.dirty.remove(k);
            }
            n
        } else {
            0
        }
    };
    cache.total_pages -= n_removed;
    if let Some(file) = cache.files.get_mut(&file_id) {
        file.size = new_size;
    }
}

/// Mark a file as deleted (unlink). If no fds hold it, free immediately.
pub fn mark_deleted(file_id: FileId) {
    let mut cache = FILE_CACHE.lock();
    let Some(file) = cache.files.get_mut(&file_id) else { return };
    file.deleted = true;
    if file.ref_count == 0 {
        let removed = cache.files.remove(&file_id).unwrap();
        cache.total_pages -= removed.pages.len();
    }
}

/// Get the ref_count for a file (used by filesystem adapters on close_file).
pub fn ref_count(file_id: FileId) -> u32 {
    FILE_CACHE.lock().files.get(&file_id).map_or(0, |f| f.ref_count)
}

/// Check if a file exists in the cache.
pub fn exists(file_id: FileId) -> bool {
    FILE_CACHE.lock().files.contains_key(&file_id)
}

// --- Internal helpers ---

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
    if cache.total_pages <= cache.max_pages {
        return;
    }
    // Evict clean, evictable pages. Simple: scan files, drop first clean page found.
    let file_ids: alloc::vec::Vec<FileId> = cache.files.keys().copied().collect();
    for fid in file_ids {
        if cache.total_pages <= cache.max_pages {
            break;
        }
        let Some(file) = cache.files.get_mut(&fid) else { continue };
        if !file.evictable || file.ref_count > 0 { continue; }
        // Evict clean pages from this file.
        let clean: alloc::vec::Vec<u32> = file.pages.keys()
            .filter(|k| !file.dirty.contains(k))
            .copied()
            .collect();
        for page_idx in clean {
            file.pages.remove(&page_idx);
            cache.total_pages -= 1;
            if cache.total_pages <= cache.max_pages {
                break;
            }
        }
    }
}
