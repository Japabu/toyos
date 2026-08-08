//! The directory tree: every entry in it, the long names in front of them, and
//! the cluster chains they name.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::boot::{u16_at, u32_at, Geometry};
use crate::fat::Owners;
use crate::{Complaint, Report, MAX_DEPTH};

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = 0x0F;
const ATTR_LONG_NAME_MASK: u8 = 0x3F;
const LAST_LONG_ENTRY: u8 = 0x40;
/// A long name is at most 255 characters and an entry carries 13 of them, so no
/// run of them is longer than this and no ordinal is higher (fatgen103 §7).
const MAX_LONG_ENTRIES: u8 = 20;

/// The `DIR_NTRes` bits something defines.
///
/// fatgen103 §6 reserves the whole byte "for use by Windows NT" and tells a
/// formatter to write 0. What Windows NT put there is deployed everywhere and
/// is not a defect: 0x08 says the 8.3 base is really lowercase and 0x10 says
/// the extension is. macOS writes them — the volumes this crate's own suite is
/// built on carry 0x08 and 0x18 — so a checker that reads the sentence and not
/// the world reds on every one of them. Any *other* bit is a byte nothing has
/// ever defined, which is what this complains about.
const NT_RESERVED_DEFINED: u8 = 0x18;

const DOT: [u8; 11] = *b".          ";
const DOT_DOT: [u8; 11] = *b"..         ";
/// What the checker reports in place of an entry that is not there at all,
/// which is what a directory's end-of-list mark looks like from outside.
const ABSENT: [u8; 11] = [0; 11];

/// fatgen103 §7.2: the short name's eleven bytes, rotated right and summed.
fn short_checksum(name: &[u8; 11]) -> u8 {
    let mut sum: u8 = 0;
    for &c in name {
        sum = (sum >> 1) | (sum << 7);
        sum = sum.wrapping_add(c);
    }
    sum
}

fn trimmed(field: &[u8]) -> &[u8] {
    let mut end = field.len();
    while end > 0 && field[end - 1] == b' ' {
        end -= 1;
    }
    &field[..end]
}

/// `KERNEL~1.ELF` from the eleven bytes that hold it.
fn short_text(name: &[u8; 11]) -> String {
    let mut out: String = trimmed(&name[..8]).iter().map(|&b| b as char).collect();
    let ext = trimmed(&name[8..]);
    if !ext.is_empty() {
        out.push('.');
        out.extend(ext.iter().map(|&b| b as char));
    }
    out
}

/// One long-name entry's thirteen UCS-2 units, in the three runs the format
/// splits them across.
fn long_units(entry: &[u8]) -> [u16; 13] {
    let mut out = [0u16; 13];
    for (i, at) in [1usize, 3, 5, 7, 9].into_iter().enumerate() {
        out[i] = u16_at(entry, at);
    }
    for (i, at) in [14usize, 16, 18, 20, 22, 24].into_iter().enumerate() {
        out[5 + i] = u16_at(entry, at);
    }
    for (i, at) in [28usize, 30].into_iter().enumerate() {
        out[11 + i] = u16_at(entry, at);
    }
    out
}

struct LongEntry {
    index: u32,
    ordinal: u8,
    checksum: u8,
    units: [u16; 13],
}

struct ShortEntry {
    index: u32,
    name: [u8; 11],
    attr: u8,
    nt_reserved: u8,
    first_cluster: u32,
    size: u32,
}

impl ShortEntry {
    fn is_label(&self) -> bool {
        self.attr & ATTR_LONG_NAME_MASK != ATTR_LONG_NAME && self.attr & ATTR_VOLUME_ID != 0
    }

    fn is_directory(&self) -> bool {
        !self.is_label() && self.attr & ATTR_DIRECTORY != 0
    }

    fn is_dot(&self) -> bool {
        self.name == DOT || self.name == DOT_DOT
    }
}

/// The directory being read, as everything asked about one of its entries needs
/// to see it.
struct Dir<'a> {
    path: &'a str,
    is_root: bool,
    cluster: u32,
    /// The cluster `..` must name: the parent's, and 0 where the parent is the
    /// root directory.
    parent: u32,
}

struct Ctx<'a> {
    vol: &'a [u8],
    geo: &'a Geometry,
    table: &'a [u32],
    owners: Owners,
    labels_in_root: u32,
}

/// Every directory reachable from the root, and the clusters they hold.
pub(crate) fn walk(vol: &[u8], geo: &Geometry, table: &[u32], r: &mut Report) -> Owners {
    let mut ctx = Ctx { vol, geo, table, owners: Owners::new(table.len()), labels_in_root: 0 };
    let root = Dir { path: "/", is_root: true, cluster: geo.root_cluster, parent: 0 };
    visit(&mut ctx, &root, 0, r);
    if ctx.labels_in_root > 1 {
        r.say(Complaint::ExtraVolumeLabel { count: ctx.labels_in_root });
    }
    ctx.owners
}

fn child_path(parent: &str, name: &str) -> String {
    let mut path = String::from(parent);
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
    path
}

fn visit(ctx: &mut Ctx, dir: &Dir, depth: u32, r: &mut Report) {
    if depth > MAX_DEPTH {
        r.say(Complaint::TooDeep { path: dir.path.to_string() });
        return;
    }
    let clusters = ctx.owners.claim(dir.path, dir.cluster, ctx.table, r);

    let mut longs: Vec<LongEntry> = Vec::new();
    let mut shorts: Vec<[u8; 11]> = Vec::new();
    let mut children: Vec<(String, u32)> = Vec::new();
    let mut opening = [ABSENT; 2];
    let mut index = 0u32;
    let bytes_per_cluster = ctx.geo.bytes_per_cluster() as usize;
    let mut buf = vec![0u8; bytes_per_cluster];

    'chain: for cluster in clusters {
        let Ok(at) = usize::try_from(ctx.geo.cluster_offset(cluster)) else { break };
        let Some(src) = ctx.vol.get(at..at + bytes_per_cluster) else { break };
        buf.copy_from_slice(src);

        for entry in buf.chunks_exact(32) {
            if entry[0] == 0x00 {
                break 'chain;
            }
            if let Some(slot) = opening.get_mut(index as usize) {
                slot.copy_from_slice(&entry[..11]);
            }
            if entry[0] == 0xE5 {
                orphan(&mut longs, dir.path, r);
                index += 1;
                continue;
            }
            if entry[11] & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME {
                take_long(&mut longs, entry, index, dir.path, r);
                index += 1;
                continue;
            }

            let mut name = [0u8; 11];
            name.copy_from_slice(&entry[..11]);
            // 0x05 stands in for a first byte of 0xE5, which would otherwise
            // mark the entry free (fatgen103 §6).
            if name[0] == 0x05 {
                name[0] = 0xE5;
            }
            let short = ShortEntry {
                index,
                name,
                attr: entry[11],
                nt_reserved: entry[12],
                first_cluster: (u32::from(u16_at(entry, 20)) << 16) | u32::from(u16_at(entry, 26)),
                size: u32_at(entry, 28),
            };
            let long = check_long_run(&longs, &short, dir.path, r);
            longs.clear();
            check_short(ctx, dir, &short, long, &mut children, r);
            if !short.is_label() {
                shorts.push(short.name);
            }
            index += 1;
        }
    }
    orphan(&mut longs, dir.path, r);
    duplicates(&shorts, dir.path, r);
    if !dir.is_root {
        for (slot, want) in [(0usize, DOT), (1, DOT_DOT)] {
            if opening[slot] != want {
                r.say(Complaint::DotEntry {
                    path: dir.path.to_string(),
                    entry: slot as u32,
                    want: if slot == 0 { "\".\"" } else { "\"..\"" },
                    got: opening[slot],
                });
            }
        }
    }

    for (path, cluster) in children {
        let child = Dir {
            path: &path,
            is_root: false,
            cluster,
            parent: if dir.is_root { 0 } else { dir.cluster },
        };
        visit(ctx, &child, depth + 1, r);
    }
}

/// A long-name run that nothing followed.
fn orphan(longs: &mut Vec<LongEntry>, path: &str, r: &mut Report) {
    if let Some(head) = longs.first() {
        r.say(Complaint::OrphanLongName { path: path.to_string(), entry: head.index });
    }
    longs.clear();
}

/// Accumulate one long-name entry, checking what it says about its own place in
/// the run.
///
/// The format writes a run backwards: the entry holding the *last* thirteen
/// characters comes first and carries `LAST_LONG_ENTRY`, and the ordinals count
/// down to 1 immediately before the short entry they name.
fn take_long(longs: &mut Vec<LongEntry>, entry: &[u8], index: u32, path: &str, r: &mut Report) {
    let ord = entry[0];
    let last = ord & LAST_LONG_ENTRY != 0;
    let ordinal = ord & !LAST_LONG_ENTRY;

    if last && !longs.is_empty() {
        orphan(longs, path, r);
    }
    if longs.is_empty() {
        if !last {
            r.say(Complaint::LongNameLastFlag { path: path.to_string(), entry: index, got: ord });
        }
        if !(1..=MAX_LONG_ENTRIES).contains(&ordinal) {
            r.say(Complaint::LongNameRunLength {
                path: path.to_string(),
                entry: index,
                got: ordinal,
            });
        }
    } else {
        let want = longs[longs.len() - 1].ordinal.saturating_sub(1);
        if ordinal != want {
            r.say(Complaint::LongNameOrdinal {
                path: path.to_string(),
                entry: index,
                got: ordinal,
                want,
            });
        }
    }

    if entry[12] != 0 {
        r.say(Complaint::LongNameType { path: path.to_string(), entry: index, got: entry[12] });
    }
    let cluster = u16_at(entry, 26);
    if cluster != 0 {
        r.say(Complaint::LongNameCluster { path: path.to_string(), entry: index, got: cluster });
    }

    longs.push(LongEntry { index, ordinal, checksum: entry[13], units: long_units(entry) });
}

/// The run against the short entry it names, and the name it spells.
fn check_long_run(
    longs: &[LongEntry],
    short: &ShortEntry,
    path: &str,
    r: &mut Report,
) -> Option<String> {
    let last = longs.last()?;
    let want = short_checksum(&short.name);
    for long in longs {
        if long.checksum != want {
            r.say(Complaint::LongNameChecksum {
                path: path.to_string(),
                entry: long.index,
                got: long.checksum,
                want,
            });
        }
    }
    if last.ordinal != 1 {
        r.say(Complaint::LongNameOrdinal {
            path: path.to_string(),
            entry: last.index,
            got: last.ordinal,
            want: 1,
        });
    }

    let mut units: Vec<u16> = Vec::new();
    for long in longs.iter().rev() {
        for &u in &long.units {
            if u == 0x0000 || u == 0xFFFF {
                break;
            }
            units.push(u);
        }
    }
    if units.is_empty() {
        return None;
    }
    Some(char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect())
}

fn check_short(
    ctx: &mut Ctx,
    dir: &Dir,
    short: &ShortEntry,
    long: Option<String>,
    children: &mut Vec<(String, u32)>,
    r: &mut Report,
) {
    if short.nt_reserved & !NT_RESERVED_DEFINED != 0 {
        r.say(Complaint::ReservedEntryByte {
            path: dir.path.to_string(),
            entry: short.index,
            got: short.nt_reserved,
        });
    }

    if short.is_label() {
        if dir.is_root {
            ctx.labels_in_root += 1;
        } else {
            r.say(Complaint::VolumeLabelInSubdirectory {
                path: dir.path.to_string(),
                entry: short.index,
            });
        }
        return;
    }

    if short.is_dot() {
        if dir.is_root {
            r.say(Complaint::DotInRoot { got: short.name });
            return;
        }
        let (want, complaint): (u32, fn(String, u32, u32) -> Complaint) = if short.name == DOT {
            (dir.cluster, |path, got, want| Complaint::DotCluster { path, got, want })
        } else {
            (dir.parent, |path, got, want| Complaint::DotDotCluster { path, got, want })
        };
        if short.first_cluster != want {
            r.say(complaint(dir.path.to_string(), short.first_cluster, want));
        }
        return;
    }

    let name = child_path(dir.path, &long.unwrap_or_else(|| short_text(&short.name)));
    if short.first_cluster != 0 && !ctx.geo.holds(short.first_cluster) {
        r.say(Complaint::FirstCluster {
            path: name,
            entry: short.index,
            got: short.first_cluster,
            clusters: ctx.geo.cluster_count,
        });
        return;
    }

    if short.is_directory() {
        if short.size != 0 {
            r.say(Complaint::DirectorySize { path: name.clone(), size: u64::from(short.size) });
        }
        if short.first_cluster == 0 {
            r.say(Complaint::DirectoryHasNoCluster { path: name });
            return;
        }
        children.push((name, short.first_cluster));
        return;
    }

    // A file's chain is claimed here rather than beside the directory's,
    // because the size it has to match is in this entry and nowhere else.
    let held = ctx.owners.claim(&name, short.first_cluster, ctx.table, r).len() as u64;
    let size = u64::from(short.size);
    let needed = size.div_ceil(ctx.geo.bytes_per_cluster());
    if held < needed {
        r.say(Complaint::ChainTooShort { path: name, size, held, needed });
    } else if held > needed {
        r.say(Complaint::ChainTooLong { path: name, size, held, needed });
    }
}

/// Two entries in one directory with the same 8.3 name.
///
/// The other check `fsck_msdos` does not do. Neither it nor a mount looks at
/// short names, because both use the long ones — so a writer that stops
/// uniquifying them leaves a directory whose entries a short-name reader, which
/// is every FAT driver's fallback, cannot tell apart.
fn duplicates(shorts: &[[u8; 11]], path: &str, r: &mut Report) {
    let mut sorted: Vec<[u8; 11]> = shorts.to_vec();
    sorted.sort_unstable();
    let mut said = ABSENT;
    for pair in sorted.windows(2) {
        if pair[0] == pair[1] && pair[0] != said {
            said = pair[0];
            r.say(Complaint::DuplicateShortName { path: path.to_string(), name: pair[0] });
        }
    }
}
