//! The file allocation table: its two reserved entries, its copies, the chains
//! that run through it, and the clusters nothing reaches.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::boot::{u32_at, Geometry};
use crate::{Complaint, Report};

/// A FAT32 entry is 28 bits; the top four are reserved and a checker that reads
/// them compares against whatever the formatter left there.
const MASK: u32 = 0x0FFF_FFFF;
/// At or above this an entry ends a chain (fatgen103 §4).
const EOC: u32 = 0x0FFF_FFF8;
const BAD: u32 = 0x0FFF_FFF7;
/// `FAT[1]`'s two state bits (fatgen103 §4): set is clean, clear is the
/// complaint.
const CLEAN_SHUTDOWN: u32 = 0x0800_0000;
const NO_HARD_ERROR: u32 = 0x0400_0000;

/// The active FAT's entries for clusters `0..=cluster_count + 1`, masked to the
/// 28 bits the format defines.
pub(crate) fn read(vol: &[u8], geo: &Geometry, r: &mut Report) -> Option<Vec<u32>> {
    let entries = geo.cluster_count as usize + 2;
    let base = usize::try_from(geo.fat_offset(geo.active_fat)).ok()?;
    let Some(bytes) = vol.get(base..base.checked_add(entries * 4)?) else {
        r.say(Complaint::VolumeShorterThanDeclared {
            declared_bytes: geo.fat_offset(geo.active_fat) + (entries as u64) * 4,
            actual_bytes: vol.len() as u64,
        });
        return None;
    };
    Some(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) & MASK).collect())
}

/// `FAT[0]` and `FAT[1]`, which name the media and carry the volume's two state
/// flags rather than belonging to any file.
pub(crate) fn head(table: &[u32], geo: &Geometry, r: &mut Report) {
    let want = 0x0FFF_FF00 | u32::from(geo.media);
    if table[0] != want {
        r.say(Complaint::Fat0 { got: table[0], want });
    }
    let one = table[1];
    if one | CLEAN_SHUTDOWN | NO_HARD_ERROR != MASK {
        r.say(Complaint::Fat1 { got: one });
    }
    if one & CLEAN_SHUTDOWN == 0 {
        r.say(Complaint::VolumeDirty);
    }
    if one & NO_HARD_ERROR == 0 {
        r.say(Complaint::VolumeHardError);
    }
}

/// Every FAT copy against FAT 0.
///
/// The check `fsck_msdos` does not do. A mount reads the active copy only, so a
/// driver that updates FAT 0 and leaves FAT 1 behind passes every read-back
/// test ever written and leaves a volume that changes the moment anything
/// consults the mirror — which is what a repair tool does first.
pub(crate) fn mirrors(vol: &[u8], geo: &Geometry, r: &mut Report) {
    if !geo.mirrored || geo.num_fats < 2 {
        return;
    }
    let len = match usize::try_from(geo.fat_bytes()) {
        Ok(n) => n,
        Err(_) => return,
    };
    let Ok(first) = usize::try_from(geo.fat_offset(0)) else { return };
    let Some(want) = vol.get(first..first + len) else { return };

    for copy in 1..geo.num_fats {
        let Ok(at) = usize::try_from(geo.fat_offset(copy)) else { continue };
        let Some(got) = vol.get(at..at + len) else { continue };
        let Some(byte) = got.iter().zip(want).position(|(a, b)| a != b) else { continue };
        let entry = byte / 4;
        r.say(Complaint::FatMirror {
            fat: copy,
            entry: entry as u32,
            got: u32_at(got, entry * 4),
            want: u32_at(want, entry * 4),
        });
    }
}

/// Which chain holds each cluster, so a cluster claimed twice is named by both.
pub(crate) struct Owners {
    owner: Vec<u32>,
    names: Vec<String>,
}

impl Owners {
    pub(crate) fn new(clusters: usize) -> Owners {
        Owners { owner: vec![0; clusters], names: Vec::new() }
    }

    /// Walk the chain from `first`, claiming every cluster for `path`, and
    /// return what it holds.
    ///
    /// The claim is what detects both a cycle and a cross-link, and tells them
    /// apart: a chain that reaches a cluster it already owns has looped, and one
    /// that reaches a cluster another chain owns has been cross-linked. Either
    /// ends the walk, so a corrupt FAT costs one pass over the volume and not
    /// one per cluster.
    pub(crate) fn claim(
        &mut self,
        path: &str,
        first: u32,
        table: &[u32],
        r: &mut Report,
    ) -> Vec<u32> {
        let mut held = Vec::new();
        if first == 0 {
            return held;
        }
        self.names.push(path.to_string());
        let id = self.names.len() as u32;
        let clusters = (table.len() - 2) as u32;

        let mut at = first;
        loop {
            let slot = at as usize;
            if slot >= self.owner.len() {
                return held;
            }
            if self.owner[slot] != 0 {
                if self.owner[slot] == id {
                    r.say(Complaint::ChainCycle {
                        path: path.to_string(),
                        at: held.last().copied().unwrap_or(first),
                        back_to: at,
                    });
                } else {
                    r.say(Complaint::CrossLinked {
                        path: path.to_string(),
                        at,
                        held_by: self.names[self.owner[slot] as usize - 1].clone(),
                    });
                }
                return held;
            }
            self.owner[slot] = id;
            held.push(at);

            let next = table[slot];
            if next >= EOC {
                return held;
            }
            if next == BAD {
                r.say(Complaint::ChainBadCluster { path: path.to_string(), at });
                return held;
            }
            if next < 2 || next > clusters + 1 {
                r.say(Complaint::ChainOutOfRange { path: path.to_string(), at, next, clusters });
                return held;
            }
            at = next;
        }
    }
}

/// Clusters the FAT marks allocated that no directory entry reaches.
///
/// Reported as the chains they are rather than one complaint each: a failed
/// allocation leaks a run, and a run is what a repair tool would reclaim. A
/// cluster marked bad is allocated and unreachable by design and is not lost.
pub(crate) fn lost(table: &[u32], owners: &Owners, r: &mut Report) {
    let len = table.len();
    let mut orphan = vec![false; len];
    for c in 2..len {
        orphan[c] = table[c] != 0 && table[c] != BAD && owners.owner[c] == 0;
    }
    let mut pointed = vec![false; len];
    for c in 2..len {
        if !orphan[c] {
            continue;
        }
        let next = table[c] as usize;
        if next < len && orphan[next] {
            pointed[next] = true;
        }
    }

    let mut seen = vec![false; len];
    let walk = |from: usize, seen: &mut Vec<bool>, r: &mut Report| {
        let mut at = from;
        let mut clusters = 0u32;
        while at < len && orphan[at] && !seen[at] {
            seen[at] = true;
            clusters += 1;
            at = table[at] as usize;
        }
        r.say(Complaint::LostChain { first: from as u32, clusters });
    };

    for c in 2..len {
        if orphan[c] && !pointed[c] && !seen[c] {
            walk(c, &mut seen, r);
        }
    }
    // What is left is orphaned *and* in a ring, so it has no head to start at.
    for c in 2..len {
        if orphan[c] && !seen[c] {
            walk(c, &mut seen, r);
        }
    }
}
