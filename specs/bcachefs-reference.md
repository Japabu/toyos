# bcachefs On-Disk Format Reference

Collected from bcachefs.org Architecture page, bcachefs-docs.readthedocs.io,
and linux/fs/bcachefs/ header files (v6.8).

## Overview

bcachefs is a COW B+ tree filesystem. Three primary on-disk structure types:
superblock, journal, and btree.

## Superblock

Located 4KB from device start (`BCH_SB_SECTOR = 8` × 512-byte sectors).
Redundant copies elsewhere. `bch_sb_layout` at 3584 bytes from device start
records superblock locations.

### struct bch_sb (248+ bytes)
```
bch_csum    csum
le16        version, version_min
uuid        magic               (BCHFS_MAGIC)
uuid        uuid                (filesystem UUID)
uuid        user_uuid
u8[32]      label
le64        offset              (in 512-byte sectors)
le64        seq                 (sequence number, incremented on write)
le16        block_size          (in 512-byte sectors)
u8          dev_idx
u8          nr_devices
le32        u64s                (size of variable-length fields)
le64        time_base_lo
le32        time_base_hi, time_precision
le64        flags[7]            (bitmask fields, see below)
le64        write_time
le64        features[2]
le64        compat[2]
bch_sb_layout layout
bch_sb_field  start[0]          (variable-length sections follow)
```

### Key superblock flags (bitmask in flags[])
- INITIALIZED, CLEAN, ERROR_ACTION
- BTREE_NODE_SIZE (in 512-byte sectors)
- BLOCK_SIZE
- CSUM_TYPE, META_CSUM_TYPE, DATA_CSUM_TYPE
- META_REPLICAS_WANT/REQ, DATA_REPLICAS_WANT/REQ
- COMPRESSION_TYPE, BACKGROUND_COMPRESSION_TYPE
- STR_HASH_TYPE (crc32c, crc64, siphash)
- ENCODED_EXTENT_MAX_BITS
- FOREGROUND/BACKGROUND/METADATA/PROMOTE_TARGET

### Variable-length superblock sections
- BCH_SB_FIELD_journal: bucket locations for journal per device
- BCH_SB_FIELD_members_v1/v2: device list with per-device settings
- BCH_SB_FIELD_crypt: encryption key + KDF settings
- BCH_SB_FIELD_replicas: replica device lists
- BCH_SB_FIELD_clean: journal entries from clean shutdown (btree roots, usage)
- BCH_SB_FIELD_journal_v2: journal bucket entries with start/nr pairs

### struct bch_member (per device)
```
uuid        uuid
le64        nbuckets
le64        last_mount
le64        flags
le64        seq
le16        first_bucket
le16        bucket_size         (in 512-byte sectors)
le32        iops[4]
le64        errors[BCH_MEMBER_ERROR_NR]
```

## B-Tree

### Key Format (bpos - position/search key)
```
u32     snapshot    (for snapshot versioning)
u64     offset      (within-file, in 512-byte sectors for extents)
u64     inode       (file inode number)
```
Note: bpos is ordered: inode first, then offset, then snapshot.

### struct bkey (unpacked, 36 bytes)
```
u8      u64s        (combined key+value size in u64 units)
u8      format      (KEY_FORMAT_LOCAL_BTREE or KEY_FORMAT_CURRENT)
u8      type        (KEY_TYPE_*)
bversion version
u32     size        (extent size, 0 for non-extents)
bpos    p           (position)
```

### Key Packing
Keys are packed using per-node bkey_format that specifies bit-width and
offset for each field. Extents compress from ~40 bytes to ~16 bytes.
Packing is order-preserving and allowed to fail (falls back to unpacked).

### 34 Key Types
- deleted, whiteout, error
- btree_ptr, btree_ptr_v2 (interior node pointers)
- extent (data extents with pointers)
- reservation (preallocated space)
- inode, inode_v2, inode_v3
- dirent (directory entries, hashed)
- xattr
- alloc, alloc_v2, alloc_v3, alloc_v4 (bucket allocation tracking)
- quota, stripe, reflink_p, reflink_v
- inline_data, indirect_inline_data
- subvolume, snapshot, snapshot_tree
- lru, backpointer, bucket_gens
- logged_op_truncate, logged_op_finsert

### B-Tree Node Structure

Nodes are large (default 256KB). Each node contains multiple bsets.

```
struct btree_node:
    bch_csum        csum
    le64            magic       (BSET_MAGIC ^ sb UUID)
    le64            flags       (btree_id, level, seq)
    bpos            min_key, max_key
    bch_extent_ptr  _ptr
    bkey_format     format      (packing format for this node)
    bset            keys        (first bset inline)

struct bset:
    le64    seq
    le64    journal_seq     (newest journal seq in this bset)
    le32    flags
    le16    version
    le16    u64s            (total size of keys)
    bkey_packed start[0]    (sorted variable-length keys)
```

A node has 0+ written-out bsets and 1 dirty bset for new inserts.
Lookups must search all bsets. Auxiliary search trees optimize this:
- Active bset: simple offset table (first key per cacheline)
- Immutable bsets: heap-layout binary tree with compressed discriminator bits

### COW Semantics
- Most updates are appends to the dirty bset
- Old keys marked KEY_TYPE_deleted, pruned on compaction/rewrite
- Interior nodes updated via COW: new node written, parent pointer updated
- Journal coalesces random updates across nodes

### 19 B-Tree IDs (separate trees)
0:extents, 1:inodes, 2:dirents, 3:xattrs, 4:alloc, 5:quotas,
6:stripes, 7:reflink, 8:subvolumes, 9:snapshots, 10:lru,
11:freespace, 12:need_discard, 13:backpointers, 14:bucket_gens,
15:snapshot_trees, 16:deleted_inodes, 17:logged_ops, 18:rebalance_work

BTREE_MAX_DEPTH = 4

## Extents

Extents are indexed by inode:offset where offset is the END position.
Value is a list of entries:

### Entry Types
- bch_extent_ptr: 44-bit offset (512-byte sectors), 8-bit dev, 8-bit gen
- bch_extent_crc32/64/128: checksum + compressed/uncompressed sizes
- bch_extent_stripe_ptr: RAID stripe reference
- bch_extent_rebalance: compression/target tracking

### struct bch_extent_ptr
```
offset:     48 bits (512-byte sectors on device)
dev:        8 bits  (device index)
gen:        8 bits  (bucket generation, must match for validity)
cached:     1 bit
unwritten:  1 bit
type:       1 bit   (BCH_EXTENT_ENTRY_ptr = 0)
```

## Inodes

Three versions: bch_inode, bch_inode_v2, bch_inode_v3.
Root inode: BCACHEFS_ROOT_INO = 4096.

### bch_inode_v3 fields
- hash_seed, flags, mode (fixed)
- Varint-encoded optional fields: atime, ctime, mtime, otime,
  size, sectors, uid, gid, nlink, generation, dev,
  compression, checksum, replication targets, etc.
- bi_nocow flag

### Inode flags
sync, immutable, append, nodump, noatime, i_size_dirty,
i_sectors_dirty, unlinked, backptr_untrusted

## Directory Entries

### struct bch_dirent
```
bch_val     v
union { le64 d_inum; struct { le32 d_child_subvol, d_parent_subvol; }; }
u8          d_type      (DT_REG, DT_DIR, etc.)
u8          d_name[]    (variable-length, max 512)
```

Key offset = 64-bit hash of filename (truncated SHA1, or siphash).
Collision resolution via linear probing. Deletion uses whiteouts.

## Journal

Purely logical log of btree insertions. COW btrees don't require journals
for consistency, but journaling coalesces random updates.

### struct jset (journal entry set)
```
bch_csum    csum
le64        magic       (JSET_MAGIC)
le64        seq
le32        version, flags
le32        u64s
le16        _read_clock, _write_clock
le64        last_seq    (oldest journal seq still needed)
jset_entry  start[0]
```

### Journal Entry Types
- btree_keys: btree updates (keys + btree_id + level)
- btree_root: btree root pointers (KEY_TYPE_btree_ptr_v2)
- clock: IO timestamps
- usage: inode counts, key versions
- data_usage: per-replica-set usage
- dev_usage: per-device usage

### Recovery
Each bset header tracks newest journal seq of its keys.
On recovery: bsets with seq > available journal are ignored.
Journal entries replayed in order. Replay is idempotent.

## Allocation

Devices divided into fixed-size buckets (typically 128K-2M).
Each bucket has an 8-bit generation number (incremented on reuse).
Pointers are "weak" — stale gen = invalid pointer.

### struct bch_alloc_v4 (current)
```
le64    journal_seq
le32    flags           (need_discard, need_inc_gen, etc.)
u8      gen
u8      oldest_gen
u8      data_type       (free, sb, journal, btree, user, cached, etc.)
u8      stripe_redundancy
le32    dirty_sectors
le32    cached_sectors
le64    stripe
le32    io_time[2]      (read, write)
le32    stripe_sectors
le32    bp_count        (backpointer count)
le64    _fragmentation_lru
```

## Snapshots

### struct bch_snapshot
```
le32    flags       (DELETED, SUBVOL)
le32    parent
le32    children[2]
le32    subvol
le32    tree
le32    depth
le32    skip[3]
le128   btime
```

Snapshots are key-level versioned via the snapshot field in bpos,
not tree-level cloned like btrfs.

## Constants
```
BCH_SB_SECTOR           = 8         (4096 bytes from device start)
BCH_SB_LABEL_SIZE       = 32
BCH_SB_MEMBERS_MAX      = 64
BCH_REPLICAS_MAX        = 4
BCH_BKEY_PTRS_MAX       = 16
BTREE_MAX_DEPTH         = 4
BCH_JOURNAL_BUCKETS_MIN = 8
BCACHEFS_ROOT_INO       = 4096
BCH_NAME_MAX            = 512
```

## Checksum Types
none, crc32c, crc64, chacha20_poly1305_80, chacha20_poly1305_128, xxhash

## Compression Types
none, lz4, gzip, zstd

## Magic Numbers
```
BCHFS_MAGIC     = specific UUID
JSET_MAGIC      = 0x245235c1a3625032
BSET_MAGIC      = 0x90135c78b99e07f5
BCACHEFS_STATFS = 0xca451a4e
```
Actual magic per-fs = JSET_MAGIC ^ sb.uuid (first 8 bytes)
