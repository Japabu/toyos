//! What a built boot image actually holds, read out of the image itself.
//!
//! `cargo run --example imgstat -- target/bootable.img`
//!
//! Every published size of a boot image comes from here — so the claim "the
//! image is N bytes and M% of it is X" stays a command anyone can re-run rather
//! than a figure that was true once. It parses the GPT, both FAT32 volumes and
//! the initrd's bcachefs with the same three crates the build system writes
//! them with, so it cannot drift from the writer the way a separate parser
//! would.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bcachefs::{Mounted, VecBlockIO};
use toyos_fat32::{BlockAccess, Fat32, IoError};

/// A partition, in memory, byte-addressed. Read-only: this reports.
struct Volume(Vec<u8>);

impl BlockAccess for Volume {
    fn capacity(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let start = usize::try_from(offset).map_err(|_| IoError::Device)?;
        let end = start.checked_add(buf.len()).ok_or(IoError::Device)?;
        if end > self.0.len() {
            return Err(IoError::Device);
        }
        buf.copy_from_slice(&self.0[start..end]);
        Ok(())
    }

    fn write_at(&mut self, _offset: u64, _buf: &[u8]) -> Result<(), IoError> {
        Err(IoError::Device)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

/// GPT geometry, as `src/image.rs` writes it.
const LBA: usize = 512;
const ENTRIES_LBA: usize = 2;
const ENTRY_BYTES: usize = 128;
const MAX_ENTRIES: usize = 8;

fn main() {
    let path = PathBuf::from(
        std::env::args().nth(1).expect("usage: imgstat <image>"),
    );
    let disk = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    println!("image {} = {} bytes", path.display(), disk.len());

    for index in 0..MAX_ENTRIES {
        let at = ENTRIES_LBA * LBA + index * ENTRY_BYTES;
        let entry = &disk[at..at + ENTRY_BYTES];
        if entry[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let first = u64::from_le_bytes(entry[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(entry[40..48].try_into().unwrap());
        let name: String = entry[56..ENTRY_BYTES]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&unit| unit != 0)
            .filter_map(|unit| char::from_u32(u32::from(unit)))
            .collect();
        println!(
            "\n  partition {index}: {name:24} LBA {first}..{last}  {} bytes",
            (last - first + 1) * LBA as u64
        );

        let bytes = disk[first as usize * LBA..(last as usize + 1) * LBA].to_vec();
        let Ok(mut fs) = Fat32::mount(Volume(bytes)) else {
            println!("      not a FAT32 volume");
            continue;
        };
        let Ok(entries) = fs.walk(64) else {
            println!("      unreadable directory tree");
            continue;
        };
        for (file, size) in &entries {
            println!("      {file:40} {size}");
        }

        let initrd = entries
            .iter()
            .find(|(file, _)| file.to_lowercase().ends_with("initrd.img"));
        if let Some((file, size)) = initrd {
            let mut handle = fs.open(file).expect("open the initrd");
            let mut image = vec![0u8; *size as usize];
            fs.read(&mut handle, 0, &mut image).expect("read the initrd");
            report_initrd(image);
        }
    }
}

/// Which of the four things in the initrd an entry is.
///
/// The order matters: `bin/rustc` is the toolchain's, not userland's.
fn group_of(name: &str) -> &'static str {
    if name.starts_with("lib/") {
        "hosted rustc lib/"
    } else if name.starts_with("bin/rustc") {
        "hosted rustc bin/"
    } else if name.starts_with("bin/") {
        "userland bin/"
    } else if name.starts_with("share/") {
        "assets share/"
    } else {
        "other"
    }
}

fn report_initrd(image: Vec<u8>) {
    let volume_bytes = image.len() as u64;
    let fs: Mounted<VecBlockIO, bcachefs::ReadOnly> =
        Mounted::open(VecBlockIO::from_vec(image)).expect("open the initrd's bcachefs");
    let entries = fs.list().expect("list the initrd");

    let mut groups: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let mut content = 0u64;
    for (name, size) in &entries {
        content += size;
        let group = groups.entry(group_of(name)).or_default();
        group.0 += 1;
        group.1 += size;
    }

    println!(
        "\n  initrd: {} entries, {content} bytes of content in a {volume_bytes}-byte volume",
        entries.len()
    );
    for (group, (count, bytes)) in &groups {
        println!(
            "      {group:20} {count:4} entries {bytes:>12} bytes  {:5.2}% of content",
            *bytes as f64 * 100.0 / content as f64
        );
    }
    println!(
        "      {:20} {:>25} bytes  {:5.2}% of the volume",
        "slack + metadata",
        volume_bytes - content,
        (volume_bytes - content) as f64 * 100.0 / volume_bytes as f64
    );

    let mut by_size: Vec<_> = entries.iter().collect();
    by_size.sort_by_key(|e| std::cmp::Reverse(e.1));
    println!("\n  largest entries:");
    for (name, size) in by_size.iter().take(20) {
        println!("      {size:>12}  {name}");
    }
    println!("\n  everything outside lib/:");
    for (name, size) in by_size.iter().filter(|(name, _)| !name.starts_with("lib/")) {
        println!("      {size:>12}  {name}");
    }
}
