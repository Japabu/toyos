use crate::device::BlockAccess;
use crate::error::Error;
use crate::fs::Fat32;

/// What this crate writes to terminate a chain. Any value at or above
/// `0x0FFF_FFF8` reads as end-of-chain; there is no reason to prefer one, so
/// this is the one that is obviously not a cluster number.
const END_OF_CHAIN: u32 = 0x0FFF_FFFF;

/// A FAT32 entry is 28 bits. The top four belong to whoever formatted the
/// volume and are preserved across every write, which is why `set_fat_entry`
/// reads before it writes.
const ENTRY_MASK: u32 = 0x0FFF_FFFF;

impl<D: BlockAccess> Fat32<D> {
    /// One FAT entry, masked to its 28 significant bits.
    ///
    /// Reads the *active* FAT — the mirrors are written but never read, since
    /// a driver that reads one FAT and writes another has two answers for the
    /// same question and no way to say which is right.
    pub(crate) fn fat_entry(&mut self, cluster: u32) -> Result<u32, Error> {
        if !self.geom.valid_cluster(cluster) {
            return Err(Error::CorruptChain);
        }
        let fat = self.geom.active_fat.unwrap_or(0);
        let mut raw = [0u8; 4];
        self.dev.read_at(self.geom.fat_entry_offset(fat, cluster), &mut raw)?;
        Ok(u32::from_le_bytes(raw) & ENTRY_MASK)
    }

    /// Set one FAT entry in every live FAT.
    pub(crate) fn set_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), Error> {
        if !self.geom.valid_cluster(cluster) {
            return Err(Error::CorruptChain);
        }
        self.invalidate_sector();
        for fat in self.geom.fat_mirrors() {
            let offset = self.geom.fat_entry_offset(fat, cluster);
            let mut raw = [0u8; 4];
            self.dev.read_at(offset, &mut raw)?;
            let reserved = u32::from_le_bytes(raw) & !ENTRY_MASK;
            self.dev.write_at(offset, &(reserved | (value & ENTRY_MASK)).to_le_bytes())?;
        }
        Ok(())
    }

    /// The next cluster in a chain, or `None` at the end.
    ///
    /// Everything that is not a link to a valid cluster and not an
    /// end-of-chain marker is [`Error::CorruptChain`]: a free entry (0), the
    /// bad-cluster marker, the reserved cluster numbers 0 and 1, and anything
    /// past the volume's last cluster. The single range check covers all of
    /// them because [`Geometry::parse`] already refused a volume whose cluster
    /// numbers could reach the reserved markers.
    ///
    /// A cluster linking to itself is rejected here because the check is free.
    /// A longer cycle is not detected — see [`Self::advance`] for why the
    /// bound rather than the detection is what makes that safe.
    pub(crate) fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, Error> {
        let v = self.fat_entry(cluster)?;
        if v >= 0x0FFF_FFF8 {
            return Ok(None);
        }
        if !self.geom.valid_cluster(v) || v == cluster {
            return Err(Error::CorruptChain);
        }
        Ok(Some(v))
    }

    /// Advance `steps` links from `cluster`.
    ///
    /// `Ok(None)` means the chain ended first, which for a file read is simply
    /// end-of-file.
    ///
    /// There is no general cycle detection here, and the bound is what stands
    /// in for it. Every caller derives `steps` from something the chain cannot
    /// influence — a file's size field, a directory's entry bound, the
    /// volume's cluster count — so a cycle costs a bounded number of FAT reads
    /// and then either ends the operation or trips a range check. Nothing
    /// loops and nothing allocates.
    ///
    /// The residual is deliberate and worth stating: a cyclic chain under a
    /// *file* returns that file's own earlier bytes again rather than an
    /// error. Detecting it would need either a tortoise-and-hare, which
    /// doubles the FAT reads on every sequential access, or a full walk from
    /// the head at open time, which is incompatible with the position hint
    /// that makes sequential access O(1) in the first place. The cases where a
    /// cycle would do damage rather than confuse do detect it:
    /// [`Self::free_chain`] (a cluster freed twice), [`Self::chain_len`] and
    /// [`Self::chain_last`] (a directory that never ends).
    pub(crate) fn advance(&mut self, cluster: u32, steps: u64) -> Result<Option<u32>, Error> {
        let mut c = cluster;
        for _ in 0..steps {
            match self.next_cluster(c)? {
                Some(next) => c = next,
                None => return Ok(None),
            }
        }
        Ok(Some(c))
    }

    /// Length of a chain in clusters, refusing at `limit`.
    ///
    /// The refusal is what bounds a directory walk: a cyclic directory chain
    /// is indistinguishable from a very long one until you have counted past
    /// what the structure can legally be.
    pub(crate) fn chain_len(&mut self, start: u32, limit: u64) -> Result<u64, Error> {
        let mut c = start;
        let mut n = 1u64;
        loop {
            match self.next_cluster(c)? {
                Some(next) => {
                    n += 1;
                    if n > limit {
                        return Err(Error::CorruptChain);
                    }
                    c = next;
                }
                None => return Ok(n),
            }
        }
    }

    /// The last cluster of a chain, walking at most `limit` links.
    pub(crate) fn chain_last(&mut self, start: u32, limit: u64) -> Result<u32, Error> {
        let mut c = start;
        for _ in 0..limit {
            match self.next_cluster(c)? {
                Some(next) => c = next,
                None => return Ok(c),
            }
        }
        Err(Error::CorruptChain)
    }

    /// Claim one free cluster and mark it end-of-chain.
    ///
    /// Scans the FAT a sector at a time from the FSInfo hint, wrapping once.
    /// The hint is only a starting point: a hostile FSInfo can make the scan
    /// begin in the wrong place, which costs a wrap and nothing else.
    pub(crate) fn alloc_cluster(&mut self) -> Result<u32, Error> {
        let bps = self.geom.bytes_per_sector as u64;
        let entries_per_sector = bps / 4;
        let last_sector = self.geom.max_cluster() as u64 * 4 / bps;
        let hint = match self.fsinfo.next_free {
            Some(c) if self.geom.valid_cluster(c) => c,
            _ => 2,
        };
        let first_sector = hint as u64 * 4 / bps;
        let fat = self.geom.active_fat.unwrap_or(0);
        let max_cluster = self.geom.max_cluster() as u64;
        self.invalidate_sector();

        for k in 0..=last_sector {
            let sector = (first_sector + k) % (last_sector + 1);
            let base = sector * entries_per_sector;
            let offset = self.geom.fat_entry_offset(fat, 0) + sector * bps;
            let buf = self
                .scratch
                .get_mut(..bps as usize)
                .ok_or(Error::CorruptChain)?;
            self.dev.read_at(offset, buf)?;

            for e in 0..entries_per_sector {
                let cluster = base + e;
                if cluster < 2 || cluster > max_cluster {
                    continue;
                }
                let at = (e * 4) as usize;
                let Some(bytes) = self.scratch.get(at..at + 4) else { continue };
                let Ok(bytes) = <[u8; 4]>::try_from(bytes) else { continue };
                if u32::from_le_bytes(bytes) & ENTRY_MASK != 0 {
                    continue;
                }
                let cluster = cluster as u32;
                self.set_fat_entry(cluster, END_OF_CHAIN)?;
                self.fsinfo.next_free = Some(cluster.saturating_add(1));
                self.fsinfo.free_count = self.fsinfo.free_count.map(|n| n.saturating_sub(1));
                self.fsinfo.dirty = true;
                return Ok(cluster);
            }
        }
        Err(Error::NoSpace)
    }

    /// Allocate a cluster and link it onto the end of an existing chain.
    ///
    /// The link is written after the new cluster is claimed and terminated, so
    /// a failure between the two leaks a cluster rather than producing a chain
    /// that runs into free space. Leaked clusters are recoverable by `fsck`; a
    /// chain pointing at a free cluster is a filesystem two files can share.
    pub(crate) fn append_cluster(&mut self, last: u32) -> Result<u32, Error> {
        let new = self.alloc_cluster()?;
        self.set_fat_entry(last, new)?;
        Ok(new)
    }

    /// Free every cluster of a chain.
    ///
    /// Bounded by the volume's cluster count, which no legal chain can exceed.
    /// Freeing is also self-limiting against a cycle: a revisited cluster
    /// reads as free, and a free entry mid-chain is [`Error::CorruptChain`].
    pub(crate) fn free_chain(&mut self, start: u32) -> Result<(), Error> {
        let mut c = start;
        for _ in 0..self.geom.cluster_count as u64 {
            let next = self.next_cluster(c)?;
            self.set_fat_entry(c, 0)?;
            self.fsinfo.free_count = self.fsinfo.free_count.map(|n| n.saturating_add(1));
            if self.fsinfo.next_free.is_none_or(|n| c < n) {
                self.fsinfo.next_free = Some(c);
            }
            self.fsinfo.dirty = true;
            match next {
                Some(n) => c = n,
                None => return Ok(()),
            }
        }
        Err(Error::CorruptChain)
    }

    /// Drop everything after `cluster`, leaving it as the new end of chain.
    pub(crate) fn truncate_chain(&mut self, cluster: u32) -> Result<(), Error> {
        let tail = self.next_cluster(cluster)?;
        self.set_fat_entry(cluster, END_OF_CHAIN)?;
        match tail {
            Some(t) => self.free_chain(t),
            None => Ok(()),
        }
    }

    /// Allocate a cluster with its contents zeroed, for a new directory.
    ///
    /// Data clusters are not zeroed on allocation — a file write covers what
    /// it allocates — but a directory's free slots are recognised by being
    /// zero, so a new directory cluster full of stale bytes would read as
    /// entries.
    pub(crate) fn alloc_zeroed_cluster(&mut self) -> Result<u32, Error> {
        let cluster = self.alloc_cluster()?;
        self.zero_cluster(cluster)?;
        Ok(cluster)
    }

    pub(crate) fn zero_cluster(&mut self, cluster: u32) -> Result<(), Error> {
        if !self.geom.valid_cluster(cluster) {
            return Err(Error::CorruptChain);
        }
        let bps = self.geom.bytes_per_sector as usize;
        let base = self.geom.cluster_offset(cluster);
        self.invalidate_sector();
        let buf = self.scratch.get_mut(..bps).ok_or(Error::CorruptChain)?;
        buf.fill(0);
        for s in 0..self.geom.sectors_per_cluster as u64 {
            self.dev.write_at(base + s * bps as u64, &self.scratch[..bps])?;
        }
        Ok(())
    }

    /// Count free clusters by scanning the FAT.
    ///
    /// Used when FSInfo has no answer, and to produce one worth writing back.
    pub(crate) fn count_free(&mut self) -> Result<u32, Error> {
        let bps = self.geom.bytes_per_sector as u64;
        let entries_per_sector = bps / 4;
        let last_sector = self.geom.max_cluster() as u64 * 4 / bps;
        let fat = self.geom.active_fat.unwrap_or(0);
        let max_cluster = self.geom.max_cluster() as u64;
        let mut free = 0u32;
        self.invalidate_sector();

        for sector in 0..=last_sector {
            let offset = self.geom.fat_entry_offset(fat, 0) + sector * bps;
            let buf = self.scratch.get_mut(..bps as usize).ok_or(Error::CorruptChain)?;
            self.dev.read_at(offset, buf)?;
            let base = sector * entries_per_sector;
            for e in 0..entries_per_sector {
                let cluster = base + e;
                if cluster < 2 || cluster > max_cluster {
                    continue;
                }
                let at = (e * 4) as usize;
                let Some(bytes) = self.scratch.get(at..at + 4) else { continue };
                let Ok(bytes) = <[u8; 4]>::try_from(bytes) else { continue };
                if u32::from_le_bytes(bytes) & ENTRY_MASK == 0 {
                    free += 1;
                }
            }
        }
        Ok(free)
    }
}
