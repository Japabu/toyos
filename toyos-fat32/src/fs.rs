use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::boot::{FsInfo, Geometry};
use crate::device::BlockAccess;
use crate::dir::{DirScan, RawEntry, ATTR_ARCHIVE, ATTR_DIRECTORY, ATTR_READ_ONLY, MAX_DIR_ENTRIES};
use crate::error::Error;
use crate::name;
use crate::time::FatTime;

/// FAT32's file size field is 32 bits wide. Not a policy this crate could
/// relax — a larger size has nowhere to be stored.
const MAX_FILE_SIZE: u64 = u32::MAX as u64;

/// Directory nesting [`Fat32::walk`] will descend.
///
/// A crafted volume can nest as deeply as it has clusters, and every level
/// costs a path string that is longer than the last. The visited set already
/// stops a cycle; this stops a very deep tree, which is the non-cyclic version
/// of the same allocation.
const MAX_WALK_DEPTH: usize = 32;

static ZEROS: [u8; 512] = [0u8; 512];

/// A mounted FAT32 volume.
pub struct Fat32<D: BlockAccess> {
    pub(crate) dev: D,
    pub(crate) geom: Geometry,
    pub(crate) fsinfo: FsInfo,
    pub(crate) scratch: Vec<u8>,
    /// Byte offset of the sector currently in `scratch`, when it holds a clean
    /// copy of one. Not a cache in any lasting sense — it exists so a
    /// directory scan reads one sector per sixteen entries instead of one per
    /// entry, and every write invalidates it, so it cannot go stale.
    pub(crate) scratch_at: Option<u64>,
}

/// What a path names, once resolved.
#[derive(Debug, Clone, Copy)]
struct Node {
    raw: RawEntry,
    first_cluster: u32,
    /// Where the entry lives, or `None` for the root, which has none.
    loc: Option<Loc>,
}

#[derive(Debug, Clone, Copy)]
struct Loc {
    dir_start: u32,
    first_index: u32,
    index: u32,
}

/// An open file: where its directory entry is, where its data is, and where
/// the last chain walk got to.
///
/// Plain data with no lifetime tie to the volume, so it can outlive what it
/// names. [`Fat32::flush_meta`] is where that is caught: it re-reads the entry
/// and refuses if the entry no longer holds the cluster this handle last saw
/// written there, so a stale handle reports [`Error::NotFound`] rather than
/// resizing whatever file took its slot.
#[derive(Debug, Clone, Copy)]
pub struct File {
    loc: Loc,
    first_cluster: u32,
    /// The first cluster as it currently stands *in the directory entry*,
    /// which differs from `first_cluster` between an allocating write and the
    /// flush that records it.
    entry_cluster: u32,
    size: u32,
    /// Last chain position reached, as (index in chain, cluster). Cluster 0
    /// means no position is known.
    hint: (u32, u32),
}

impl File {
    pub fn len(&self) -> u64 {
        self.size as u64
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub len: u64,
    pub is_dir: bool,
    pub read_only: bool,
    pub modified_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub len: u64,
    pub is_dir: bool,
    pub modified_unix: u64,
}

/// A contiguous run of a file's data, as a byte range on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}

fn components(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

/// Split a path into everything before the last component and the component
/// itself. A path with no separator has the root as its parent.
fn split_parent(path: &str) -> (&str, &str) {
    let trimmed = path.trim_matches('/');
    match trimmed.rfind('/') {
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => ("", trimmed),
    }
}

impl<D: BlockAccess> Fat32<D> {
    /// Read and validate the boot sector without taking ownership.
    ///
    /// The kernel decides what to do with a partition before it mounts it, and
    /// that decision must not require a mount that might already have written
    /// something. Nothing in this crate writes, so this is a total read.
    pub fn probe(dev: &mut D) -> Result<Geometry, Error> {
        let mut boot = [0u8; 512];
        dev.read_at(0, &mut boot)?;
        Geometry::parse(&boot, dev.capacity())
    }

    pub fn mount(mut dev: D) -> Result<Fat32<D>, Error> {
        let geom = Self::probe(&mut dev)?;
        let mut scratch = vec![0u8; geom.bytes_per_sector as usize];
        let fsinfo = FsInfo::read(&mut dev, &geom, &mut scratch);
        Ok(Fat32 { dev, geom, fsinfo, scratch, scratch_at: None })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geom
    }

    pub fn device(&mut self) -> &mut D {
        self.scratch_at = None;
        &mut self.dev
    }

    pub fn into_device(self) -> D {
        self.dev
    }

    pub(crate) fn load_sector(&mut self, offset: u64) -> Result<(), Error> {
        if self.scratch_at == Some(offset) {
            return Ok(());
        }
        let bps = self.geom.bytes_per_sector as usize;
        let buf = self.scratch.get_mut(..bps).ok_or(Error::Io)?;
        self.dev.read_at(offset, buf)?;
        self.scratch_at = Some(offset);
        Ok(())
    }

    pub(crate) fn sector(&self) -> &[u8] {
        self.scratch.get(..self.geom.bytes_per_sector as usize).unwrap_or(&[])
    }

    pub(crate) fn invalidate_sector(&mut self) {
        self.scratch_at = None;
    }

    // ---------------------------------------------------------------- lookup

    fn find_in_dir(
        &mut self,
        dir_start: u32,
        name: &str,
    ) -> Result<Option<(RawEntry, u32, u32)>, Error> {
        let mut scan = DirScan::new(dir_start);
        while let Some(loc) = scan.next(self)? {
            if scan.name_eq(&loc, name) {
                return Ok(Some((loc.raw, loc.first_index, loc.index)));
            }
        }
        Ok(None)
    }

    fn root_node(&self) -> Node {
        let mut raw = RawEntry::zeroed();
        raw.set_attr(ATTR_DIRECTORY);
        raw.set_first_cluster(self.geom.root_cluster);
        Node { raw, first_cluster: self.geom.root_cluster, loc: None }
    }

    /// Check an entry's first cluster before anything follows it.
    ///
    /// A directory, and any file with content, must name a cluster inside the
    /// volume. This is the single place a directory entry's cluster number
    /// crosses from bytes-off-a-stick to something this crate will compute an
    /// offset from.
    fn node_from_entry(&self, raw: &RawEntry, loc: Loc) -> Result<Node, Error> {
        let first = raw.first_cluster();
        let needs_cluster = raw.is_dir() || raw.size() > 0;
        if needs_cluster && !self.geom.valid_cluster(first) {
            return Err(Error::CorruptDirectory);
        }
        Ok(Node { raw: *raw, first_cluster: first, loc: Some(loc) })
    }

    fn resolve(&mut self, path: &str) -> Result<Node, Error> {
        let mut node = self.root_node();
        for comp in components(path) {
            if !node.raw.is_dir() {
                return Err(Error::NotADirectory);
            }
            let dir_start = node.first_cluster;
            let Some((raw, first_index, index)) = self.find_in_dir(dir_start, comp)? else {
                return Err(Error::NotFound);
            };
            node = self.node_from_entry(&raw, Loc { dir_start, first_index, index })?;
        }
        Ok(node)
    }

    fn resolve_dir(&mut self, path: &str) -> Result<u32, Error> {
        let node = self.resolve(path)?;
        if !node.raw.is_dir() {
            return Err(Error::NotADirectory);
        }
        Ok(node.first_cluster)
    }

    pub fn metadata(&mut self, path: &str) -> Result<Metadata, Error> {
        let node = self.resolve(path)?;
        Ok(Metadata {
            len: if node.raw.is_dir() { 0 } else { node.raw.size() as u64 },
            is_dir: node.raw.is_dir(),
            read_only: node.raw.attr() & ATTR_READ_ONLY != 0,
            modified_unix: node.raw.write_time().to_unix_secs(),
        })
    }

    pub fn exists(&mut self, path: &str) -> Result<bool, Error> {
        match self.resolve(path) {
            Ok(_) => Ok(true),
            Err(Error::NotFound) | Err(Error::NotADirectory) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Every entry of one directory, refusing above `limit`.
    ///
    /// Refuses rather than truncating: a listing short of the truth is worse
    /// than no listing, because a caller checking that a name is absent gets a
    /// confident wrong answer.
    pub fn read_dir(&mut self, path: &str, limit: usize) -> Result<Vec<DirEntry>, Error> {
        let dir_start = self.resolve_dir(path)?;
        let mut out = Vec::new();
        let mut scan = DirScan::new(dir_start);
        while let Some(loc) = scan.next(self)? {
            if out.len() == limit {
                return Err(Error::LimitExceeded);
            }
            out.push(DirEntry {
                name: scan.name_string(&loc),
                len: if loc.raw.is_dir() { 0 } else { loc.raw.size() as u64 },
                is_dir: loc.raw.is_dir(),
                modified_unix: loc.raw.write_time().to_unix_secs(),
            });
        }
        Ok(out)
    }

    /// Every file in the volume, as a path relative to the root, paired with
    /// its size.
    ///
    /// Files only: directories appear only as a prefix on the paths inside
    /// them, which is the shape ToyOS's VFS `list` expects. An **empty
    /// directory is therefore invisible here** — an adapter that needs to see
    /// one has to ask [`Fat32::read_dir`].
    ///
    /// Iterative, with a visited set of directory clusters and a depth bound,
    /// because the tree is on-disk data and a crafted volume can make it a
    /// graph. `limit` bounds files and directories alike; either exceeding it
    /// abandons the whole listing.
    pub fn walk(&mut self, limit: usize) -> Result<Vec<(String, u64)>, Error> {
        let mut out = Vec::new();
        let mut visited = BTreeSet::new();
        let mut queue: Vec<(u32, String, usize)> = Vec::new();
        queue.push((self.geom.root_cluster, String::new(), 0));
        visited.insert(self.geom.root_cluster);

        while let Some((cluster, prefix, depth)) = queue.pop() {
            let mut scan = DirScan::new(cluster);
            while let Some(loc) = scan.next(self)? {
                let name = scan.name_string(&loc);
                let mut path = String::with_capacity(prefix.len() + name.len() + 1);
                path.push_str(&prefix);
                path.push_str(&name);

                if loc.raw.is_dir() {
                    if depth + 1 > MAX_WALK_DEPTH {
                        return Err(Error::LimitExceeded);
                    }
                    if visited.len() >= limit {
                        return Err(Error::LimitExceeded);
                    }
                    let child = loc.raw.first_cluster();
                    if !self.geom.valid_cluster(child) {
                        return Err(Error::CorruptDirectory);
                    }
                    if visited.insert(child) {
                        path.push('/');
                        queue.push((child, path, depth + 1));
                    }
                } else {
                    if out.len() >= limit {
                        return Err(Error::LimitExceeded);
                    }
                    out.push((path, loc.raw.size() as u64));
                }
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------- file I/O

    /// Open a file. Directories are refused — nothing here reads a directory
    /// as a byte stream, and letting a caller try would hand it 32-byte
    /// entries it has no way to interpret.
    pub fn open(&mut self, path: &str) -> Result<File, Error> {
        let node = self.resolve(path)?;
        if node.raw.is_dir() {
            return Err(Error::IsADirectory);
        }
        let loc = node.loc.ok_or(Error::IsADirectory)?;
        Ok(File {
            loc,
            first_cluster: node.first_cluster,
            entry_cluster: node.first_cluster,
            size: node.raw.size(),
            hint: (0, 0),
        })
    }

    /// The cluster holding chain index `index`, walking from the handle's last
    /// known position when that is not behind it.
    fn cluster_at(&mut self, f: &mut File, index: u32) -> Result<Option<u32>, Error> {
        if f.first_cluster == 0 {
            return Ok(None);
        }
        let (hint_index, hint_cluster) = f.hint;
        let (from, steps) = if hint_cluster != 0 && hint_index <= index {
            (hint_cluster, (index - hint_index) as u64)
        } else {
            (f.first_cluster, index as u64)
        };
        let found = self.advance(from, steps)?;
        if let Some(c) = found {
            f.hint = (index, c);
        }
        Ok(found)
    }

    /// How many clusters past `index` are physically contiguous with it, up to
    /// `want`. Turns a sequential read of a defragmented file into one device
    /// call per run instead of one per cluster.
    fn contiguous_run(&mut self, start: u32, want: u64) -> Result<(u64, u32), Error> {
        let mut run = 1u64;
        let mut last = start;
        while run < want {
            match self.next_cluster(last)? {
                Some(next) if next == last + 1 => {
                    last = next;
                    run += 1;
                }
                _ => break,
            }
        }
        Ok((run, last))
    }

    pub fn read(&mut self, f: &mut File, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        if offset >= f.size as u64 {
            return Ok(0);
        }
        let n = buf.len().min((f.size as u64 - offset) as usize);
        let bpc = self.geom.bytes_per_cluster() as u64;
        let mut done = 0usize;

        while done < n {
            let pos = offset + done as u64;
            let index = (pos / bpc) as u32;
            let within = pos % bpc;
            let cluster = self.cluster_at(f, index)?.ok_or(Error::CorruptChain)?;
            let want = (within + (n - done) as u64).div_ceil(bpc);
            let (run, last) = self.contiguous_run(cluster, want)?;
            f.hint = (index + run as u32 - 1, last);

            let chunk = ((run * bpc - within) as usize).min(n - done);
            let dst = buf.get_mut(done..done + chunk).ok_or(Error::Io)?;
            let at = self.geom.cluster_offset(cluster) + within;
            self.dev.read_at(at, dst)?;
            done += chunk;
        }
        Ok(n)
    }

    /// Grow the chain so it covers `need` bytes.
    ///
    /// Walks from the handle's hint, so an append that ends where the last one
    /// stopped costs no chain traversal at all. A chain already longer than
    /// `need` is adopted rather than trimmed — another writer's file may carry
    /// slack, and shrinking it is not what a write asked for.
    fn ensure_capacity(&mut self, f: &mut File, need: u64) -> Result<(), Error> {
        if need == 0 {
            return Ok(());
        }
        let bpc = self.geom.bytes_per_cluster() as u64;
        let want = need.div_ceil(bpc);

        if f.first_cluster == 0 {
            let c = self.alloc_cluster()?;
            f.first_cluster = c;
            f.hint = (0, c);
        }
        let (mut index, mut cluster) = match f.hint {
            (i, c) if c != 0 => (i, c),
            _ => (0, f.first_cluster),
        };
        while (index as u64) + 1 < want {
            cluster = match self.next_cluster(cluster)? {
                Some(next) => next,
                None => self.append_cluster(cluster)?,
            };
            index += 1;
        }
        f.hint = (index, cluster);
        Ok(())
    }

    /// Write bytes into space that is already allocated.
    fn write_allocated(&mut self, f: &mut File, offset: u64, data: &[u8]) -> Result<(), Error> {
        let bpc = self.geom.bytes_per_cluster() as u64;
        let mut done = 0usize;
        while done < data.len() {
            let pos = offset + done as u64;
            let index = (pos / bpc) as u32;
            let within = pos % bpc;
            let cluster = self.cluster_at(f, index)?.ok_or(Error::CorruptChain)?;
            let want = (within + (data.len() - done) as u64).div_ceil(bpc);
            let (run, last) = self.contiguous_run(cluster, want)?;
            f.hint = (index + run as u32 - 1, last);

            let chunk = ((run * bpc - within) as usize).min(data.len() - done);
            let src = data.get(done..done + chunk).ok_or(Error::Io)?;
            let at = self.geom.cluster_offset(cluster) + within;
            self.invalidate_sector();
            self.dev.write_at(at, src)?;
            done += chunk;
        }
        Ok(())
    }

    /// FAT has no holes, so a write past the end has to fill what it skipped.
    /// Newly allocated clusters hold whatever the last file to own them left
    /// behind, and handing that to a reader is a cross-file data leak.
    fn zero_range(&mut self, f: &mut File, from: u64, to: u64) -> Result<(), Error> {
        let mut at = from;
        while at < to {
            let chunk = ((to - at) as usize).min(ZEROS.len());
            self.write_allocated(f, at, &ZEROS[..chunk])?;
            at += chunk as u64;
        }
        Ok(())
    }

    /// Write `data` at `offset`, allocating and zero-filling as needed.
    ///
    /// All or nothing. A write that fails part way — running out of clusters
    /// is the one that happens — gives back everything it took, so the handle
    /// still describes the file it described before. Without that rollback a
    /// failed write leaves a chain longer than the size that will be recorded
    /// for it, which is what `fsck_msdos` calls "too many clusters allocated"
    /// and is a real inconsistency, not a cosmetic one.
    ///
    /// The alternative — reporting how many bytes did fit — was rejected: a
    /// caller that must handle a short write correctly is a caller that will
    /// not, and the retry it would need is the same retry it can do with a
    /// smaller slice.
    ///
    /// Only the handle is updated; the directory entry still holds the old
    /// size until [`Fat32::flush_meta`]. That split is what lets a caller
    /// write a file in pages and pay for one metadata update.
    pub fn write(&mut self, f: &mut File, offset: u64, data: &[u8]) -> Result<(), Error> {
        let end = offset.checked_add(data.len() as u64).ok_or(Error::TooLarge)?;
        if end > MAX_FILE_SIZE {
            return Err(Error::TooLarge);
        }
        if data.is_empty() {
            return Ok(());
        }
        let old_size = f.size as u64;
        match self.write_inner(f, offset, data, end, old_size) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.rollback_to(f, old_size);
                Err(e)
            }
        }
    }

    fn write_inner(
        &mut self,
        f: &mut File,
        offset: u64,
        data: &[u8],
        end: u64,
        old_size: u64,
    ) -> Result<(), Error> {
        self.ensure_capacity(f, end)?;
        if offset > old_size {
            self.zero_range(f, old_size, offset)?;
        }
        self.write_allocated(f, offset, data)?;
        if end > old_size {
            f.size = end as u32;
        }
        Ok(())
    }

    /// Set a file's length, allocating and zeroing on growth and releasing
    /// clusters on shrink.
    pub fn set_len(&mut self, f: &mut File, len: u64) -> Result<(), Error> {
        if len > MAX_FILE_SIZE {
            return Err(Error::TooLarge);
        }
        let old = f.size as u64;
        if len > old {
            if let Err(e) = self.ensure_capacity(f, len).and_then(|()| self.zero_range(f, old, len)) {
                self.rollback_to(f, old);
                return Err(e);
            }
            f.size = len as u32;
            return Ok(());
        }
        if len < old {
            self.shrink_chain(f, len)?;
        }
        f.size = len as u32;
        Ok(())
    }

    /// Release every cluster past what `len` bytes need.
    fn shrink_chain(&mut self, f: &mut File, len: u64) -> Result<(), Error> {
        let bpc = self.geom.bytes_per_cluster() as u64;
        let keep = len.div_ceil(bpc);
        f.hint = (0, 0);
        if f.first_cluster == 0 {
            return Ok(());
        }
        if keep == 0 {
            self.free_chain(f.first_cluster)?;
            f.first_cluster = 0;
            return Ok(());
        }
        let last = self.advance(f.first_cluster, keep - 1)?.ok_or(Error::CorruptChain)?;
        self.truncate_chain(last)
    }

    /// Undo a failed growth. Best effort by necessity: the failure being
    /// undone is usually a device that has stopped answering, and there is
    /// nothing better to return than the error that started it.
    fn rollback_to(&mut self, f: &mut File, size: u64) {
        let _ = self.shrink_chain(f, size);
        f.size = size as u32;
    }

    /// Record a handle's size, first cluster and modification time in its
    /// directory entry.
    ///
    /// Refuses if the entry no longer holds the cluster this handle last saw
    /// written there — see [`File`].
    pub fn flush_meta(&mut self, f: &mut File, time: FatTime) -> Result<(), Error> {
        let offset = self.entry_offset(f.loc)?;
        let mut raw = self.read_entry_at(offset)?;
        if raw.is_free() || raw.is_lfn() || raw.first_cluster() != f.entry_cluster {
            return Err(Error::NotFound);
        }
        raw.set_first_cluster(f.first_cluster);
        raw.set_size(f.size);
        raw.set_write_time(time);
        self.write_entry_at(offset, &raw)?;
        f.entry_cluster = f.first_cluster;
        Ok(())
    }

    fn entry_offset(&mut self, loc: Loc) -> Result<u64, Error> {
        let mut cursor = crate::dir::EntryCursor::new(loc.dir_start);
        cursor.offset_of(self, loc.index)?.ok_or(Error::NotFound)
    }

    /// The device byte ranges holding a file's data, coalesced.
    ///
    /// For a demand-paging backing that wants to read a file without going
    /// back through this crate. Bounded by `max`: a fragmented file has one
    /// extent per cluster, and how many of those a caller can hold is the
    /// caller's business, not the volume's.
    pub fn extents(&mut self, path: &str, max: usize) -> Result<Vec<Extent>, Error> {
        let node = self.resolve(path)?;
        if node.raw.is_dir() {
            return Err(Error::IsADirectory);
        }
        let size = node.raw.size() as u64;
        let mut out = Vec::new();
        if size == 0 {
            return Ok(out);
        }
        let bpc = self.geom.bytes_per_cluster() as u64;
        let mut covered = 0u64;
        let mut cluster = node.first_cluster;
        while covered < size {
            let want = (size - covered).div_ceil(bpc);
            let (run, last) = self.contiguous_run(cluster, want)?;
            let len = (run * bpc).min(size - covered);
            if out.len() == max {
                return Err(Error::LimitExceeded);
            }
            out.push(Extent { offset: self.geom.cluster_offset(cluster), len });
            covered += len;
            if covered >= size {
                break;
            }
            cluster = self.next_cluster(last)?.ok_or(Error::CorruptChain)?;
        }
        Ok(out)
    }

    // ------------------------------------------------------------ namespace

    fn parent_of(&mut self, path: &str) -> Result<(u32, String), Error> {
        let (parent, name) = split_parent(path);
        if name.is_empty() {
            return Err(Error::InvalidName);
        }
        let dir = self.resolve_dir(parent)?;
        Ok((dir, String::from(name)))
    }

    /// Create an empty file. Fails if anything of that name already exists —
    /// a create that silently opened an existing file would let a caller
    /// believe it owns bytes another writer put there.
    pub fn create(&mut self, path: &str, time: FatTime) -> Result<File, Error> {
        let (dir, name) = self.parent_of(path)?;
        if self.find_in_dir(dir, &name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let mut template = RawEntry::zeroed();
        template.set_attr(ATTR_ARCHIVE);
        template.set_create_time(time);
        template.set_write_time(time);
        let index = self.insert_entry(dir, &name, &template)?;
        Ok(File {
            loc: Loc { dir_start: dir, first_index: index, index },
            first_cluster: 0,
            entry_cluster: 0,
            size: 0,
            hint: (0, 0),
        })
    }

    pub fn create_dir(&mut self, path: &str, time: FatTime) -> Result<(), Error> {
        let (dir, name) = self.parent_of(path)?;
        if self.find_in_dir(dir, &name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        name::validate_component(&name)?;

        // The cluster is prepared before the entry that names it exists, so a
        // failure here leaks a cluster rather than leaving a directory entry
        // pointing at uninitialised bytes that would read as entries.
        let cluster = self.alloc_zeroed_cluster()?;
        self.init_dot_entries(cluster, dir, time)?;

        let mut template = RawEntry::zeroed();
        template.set_attr(ATTR_DIRECTORY);
        template.set_first_cluster(cluster);
        template.set_create_time(time);
        template.set_write_time(time);
        match self.insert_entry(dir, &name, &template) {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = self.free_chain(cluster);
                Err(e)
            }
        }
    }

    /// Create every missing directory along a path.
    pub fn create_dir_all(&mut self, path: &str, time: FatTime) -> Result<(), Error> {
        let mut so_far = String::new();
        for comp in components(path) {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(comp);
            match self.metadata(&so_far) {
                Ok(m) if m.is_dir => continue,
                Ok(_) => return Err(Error::NotADirectory),
                Err(Error::NotFound) => self.create_dir(&so_far, time)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> Result<(), Error> {
        let node = self.resolve(path)?;
        if node.raw.is_dir() {
            return Err(Error::IsADirectory);
        }
        let loc = node.loc.ok_or(Error::IsADirectory)?;
        if node.first_cluster != 0 {
            self.free_chain(node.first_cluster)?;
        }
        self.erase_entries(loc.dir_start, loc.first_index, loc.index)
    }

    pub fn remove_dir(&mut self, path: &str) -> Result<(), Error> {
        let node = self.resolve(path)?;
        if !node.raw.is_dir() {
            return Err(Error::NotADirectory);
        }
        let loc = node.loc.ok_or(Error::IsADirectory)?;
        if !self.dir_is_empty(node.first_cluster)? {
            return Err(Error::DirectoryNotEmpty);
        }
        self.free_chain(node.first_cluster)?;
        self.erase_entries(loc.dir_start, loc.first_index, loc.index)
    }

    /// Move a file or directory.
    ///
    /// Refuses when the destination exists. FAT gives no way to make the
    /// replacement atomic, and a rename that deletes the destination first has
    /// a window in which neither name resolves — which is worse than an error
    /// the caller can act on.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), Error> {
        let node = self.resolve(from)?;
        let loc = node.loc.ok_or(Error::IsADirectory)?;
        let (new_dir, new_name) = self.parent_of(to)?;
        if self.find_in_dir(new_dir, &new_name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        // Moving a directory into itself would detach the subtree: the entry
        // naming it would live inside the tree it names.
        if node.raw.is_dir() && self.is_ancestor(node.first_cluster, new_dir)? {
            return Err(Error::InvalidName);
        }

        self.insert_entry(new_dir, &new_name, &node.raw)?;
        self.erase_entries(loc.dir_start, loc.first_index, loc.index)?;
        if node.raw.is_dir() && new_dir != loc.dir_start {
            self.set_dot_dot(node.first_cluster, new_dir)?;
        }
        Ok(())
    }

    /// Whether `dir` is `ancestor` or lives beneath it.
    fn is_ancestor(&mut self, ancestor: u32, dir: u32) -> Result<bool, Error> {
        let mut queue = vec![ancestor];
        let mut visited = BTreeSet::new();
        visited.insert(ancestor);
        let mut budget = MAX_DIR_ENTRIES;
        while let Some(cluster) = queue.pop() {
            if cluster == dir {
                return Ok(true);
            }
            let mut scan = DirScan::new(cluster);
            while let Some(loc) = scan.next(self)? {
                budget = budget.checked_sub(1).ok_or(Error::LimitExceeded)?;
                if !loc.raw.is_dir() {
                    continue;
                }
                let child = loc.raw.first_cluster();
                if self.geom.valid_cluster(child) && visited.insert(child) {
                    queue.push(child);
                }
            }
        }
        Ok(false)
    }

    // ----------------------------------------------------------- durability

    /// Make every write durable and record the free-cluster hints.
    ///
    /// FSInfo is written before the flush so the flush covers it.
    pub fn sync(&mut self) -> Result<(), Error> {
        if self.fsinfo.dirty {
            let Fat32 { dev, geom, fsinfo, scratch, scratch_at } = self;
            *scratch_at = None;
            fsinfo.write(dev, geom, scratch)?;
            fsinfo.dirty = false;
        }
        self.dev.flush()?;
        Ok(())
    }

    /// Free space, in bytes.
    ///
    /// Uses the FSInfo hint when the volume had one and counts the FAT when it
    /// did not. The count is cached in the hint so the scan happens once.
    pub fn free_bytes(&mut self) -> Result<u64, Error> {
        let free = match self.fsinfo.free_count {
            Some(n) => n,
            None => {
                let n = self.count_free()?;
                self.fsinfo.free_count = Some(n);
                self.fsinfo.dirty = true;
                n
            }
        };
        Ok(free as u64 * self.geom.bytes_per_cluster() as u64)
    }

    pub fn total_bytes(&self) -> u64 {
        self.geom.cluster_count as u64 * self.geom.bytes_per_cluster() as u64
    }
}
