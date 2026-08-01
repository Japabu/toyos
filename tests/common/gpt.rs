//! The boot partition, end to end: firmware, the ABI, and a real block driver.
//!
//! The parser's own reasoning is host-tested inside `toyos-gpt/`, over crafted
//! tables and every hostile field, and none of that needs a guest. What only a
//! guest can answer is whether the *identity* survives the trip — OVMF's
//! device path, the bootloader, `KernelArgs`, the kernel's NVMe driver — and
//! whether the parser finds that identity on a table it did not author.
//!
//! Ground truth is the disk image, read on the host by the `gpt` crate, which
//! is a different implementation from the one under test. The guest's own
//! account of the partition it booted from is exactly what is in question, so
//! it cannot also be the reference.
//!
//! The table on the NVMe disk is built to be adversarial in the two ways that
//! matter. Its *first* entry is an ESP by type — so a matcher keying on the
//! type GUID, or taking the first partition, or taking the first ESP, gets a
//! different span than the one asserted. And the second boot moves the
//! matching entry by eight blocks, so a kernel that skips the firmware-versus-
//! table agreement check accepts a partition that is not where firmware said
//! it was.

use std::collections::BTreeMap;
use std::path::Path;

use gpt::disk::LogicalBlockSize;
use gpt::partition::Partition;
use gpt::partition_types;

use super::qemu::{self, BootOptions, QemuInstance};

/// What the host knows about the boot image's ESP, before any guest runs.
struct Esp {
    guid: String,
    first_lba: u64,
    last_lba: u64,
}

impl Esp {
    fn blocks(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

pub fn boot_partition_identity(
    _test_config: &Path,
    _c_bins: &[(String, Vec<u8>)],
    _rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let repo = super::compile::repo_root();
    let config = repo.join("tests/metalcase/system.toml");
    let dir = std::env::temp_dir().join(format!("toyos-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create the test directory: {e}"))?;

    // Built here rather than by `boot_with_options`, because the crafted NVMe
    // table below has to carry this image's partition GUID and the image does
    // not exist until it is built. `create_gpt_disk` draws a fresh random GUID
    // every time, so there is no second build that would agree with this one.
    let quiet = !qemu::VERBOSE.load(std::sync::atomic::Ordering::Relaxed);
    let bytes = toyos_build::build::build_test_image(&repo, &config, &[], quiet, &[]);
    let boot_image = dir.join("gpt-boot.img");
    std::fs::write(&boot_image, &bytes).map_err(|e| format!("write the boot image: {e}"))?;

    let esp = read_esp(&boot_image)?;
    eprintln!(
        "  [gpt] the image the build produced: ESP {} at LBA {}+{}",
        esp.guid,
        esp.first_lba,
        esp.blocks()
    );

    // Positive: the matching entry is third, behind an ESP-typed decoy, and
    // sits exactly where firmware says it does.
    let agreeing = dir.join("gpt-nvme-agree.img");
    craft_decoy_disk(&agreeing, &esp, 0)?;
    let log = boot(&config, &boot_image, &agreeing)?;

    let firmware = format!(
        "gpt: firmware booted us from partition {} at LBA {}+{}",
        esp.guid,
        esp.first_lba,
        esp.blocks()
    );
    if !log.contains(&firmware) {
        return Err(format!(
            "the kernel did not report the partition the image actually has.\nwanted: {firmware}\n{}",
            gpt_lines(&log)
        ));
    }

    // Entry 2 of 3 is the whole assertion: the decoy at entry 0 is an ESP too,
    // so an index or a type would both have answered 0.
    let carries = format!(
        "gpt: device 1 carries the boot partition at LBA {}+{} (512-byte blocks), entry 2 of 3",
        esp.first_lba,
        esp.blocks()
    );
    if !log.contains(&carries) {
        return Err(format!(
            "the kernel did not find the boot partition where the table put it.\nwanted: \
             {carries}\n{}",
            gpt_lines(&log)
        ));
    }

    // A GPT disk is not a ToyOS volume and carries no designation stamp, so
    // the interlock that keeps the kernel off other people's disks has to
    // still refuse it — a boot partition on a disk is not consent to format
    // the disk.
    if !log.contains("this disk is not ours") {
        return Err(format!(
            "finding our boot partition on a disk made the kernel treat the disk as \
             ours:\n{}",
            gpt_lines(&log)
        ));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("the boot did not complete:\n{log}"));
    }

    // Negative: the same GUID, eight blocks to the left of where firmware saw
    // it. Two accounts of one partition that disagree means this is not the
    // disk firmware read, and the next thing anyone does with a boot volume is
    // write to it.
    let disagreeing = dir.join("gpt-nvme-shifted.img");
    craft_decoy_disk(&disagreeing, &esp, 8)?;
    let log = boot(&config, &boot_image, &disagreeing)?;

    let refused = format!(
        "gpt: device 1 puts {} at LBA {}+{} but firmware said {}+{}",
        esp.guid,
        esp.first_lba + 8,
        esp.blocks() - 8,
        esp.first_lba,
        esp.blocks()
    );
    if !log.contains(&refused) {
        return Err(format!(
            "the kernel accepted a partition that is not where firmware said it was.\nwanted: \
             {refused}\n{}",
            gpt_lines(&log)
        ));
    }
    if log.contains("carries the boot partition") {
        return Err(format!(
            "the kernel claimed a boot volume it had just refused:\n{}",
            gpt_lines(&log)
        ));
    }
    if !log.contains("Boot: complete") {
        return Err(format!("a refused partition cost the boot:\n{log}"));
    }

    let _ = std::fs::remove_file(&boot_image);
    let _ = std::fs::remove_file(&agreeing);
    let _ = std::fs::remove_file(&disagreeing);
    eprintln!("  [gpt] matched behind an ESP-typed decoy, and refused an eight-block shift");
    Ok(())
}

fn boot(
    config: &Path,
    boot_image: &Path,
    nvme_image: &Path,
) -> Result<String, String> {
    let mut qemu = QemuInstance::boot_with_options(
        config.parent().expect("system.toml has a directory"),
        &[],
        &[],
        BootOptions {
            profile: qemu::Profile::Metal,
            boot_image: Some(boot_image.to_path_buf()),
            nvme_image: Some(nvme_image.to_path_buf()),
            ..Default::default()
        },
    );
    let mut log = qemu.boot_log().to_string();
    log.push_str(&qemu.drain_serial(std::time::Duration::from_millis(500)));
    for bad in ["!!! PANIC !!!", "panicked at"] {
        if log.contains(bad) {
            return Err(format!("{bad:?} during the boot:\n{log}"));
        }
    }
    Ok(log)
}

/// Every `gpt:` line the guest printed, for a failure message.
fn gpt_lines(log: &str) -> String {
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("gpt:")).collect();
    if lines.is_empty() {
        return format!("the guest printed no gpt: line at all\n{log}");
    }
    format!("what it said:\n{}", lines.join("\n"))
}

/// The ESP of a boot image, as an implementation that is not ours reads it.
fn read_esp(image: &Path) -> Result<Esp, String> {
    let disk = gpt::GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(image)
        .map_err(|e| format!("the built image has no readable GPT: {e}"))?;

    let esps: Vec<&Partition> = disk
        .partitions()
        .values()
        .filter(|p| p.part_type_guid == partition_types::EFI)
        .collect();
    let [esp] = esps.as_slice() else {
        return Err(format!("the built image has {} ESPs, expected one", esps.len()));
    };
    Ok(Esp {
        guid: esp.part_guid.to_string().to_uppercase(),
        first_lba: esp.first_lba,
        last_lba: esp.last_lba,
    })
}

/// A GPT whose third entry is the boot partition and whose first is a decoy
/// ESP, on a sparse disk big enough to hold both.
///
/// `shift` moves the matching entry that many blocks to the right of where
/// firmware saw it, which is how the agreement check is given something to
/// refuse. Zero is the honest table.
fn craft_decoy_disk(path: &Path, esp: &Esp, shift: u64) -> Result<(), String> {
    // Room for the boot partition's span, the two decoys behind it, and the
    // backup table. Sparse, so the host pays for the few blocks written.
    let lbas = esp.last_lba + 4096;
    let file = std::fs::File::create(path).map_err(|e| format!("create the decoy disk: {e}"))?;
    file.set_len(lbas * 512).map_err(|e| format!("size the decoy disk: {e}"))?;
    drop(file);

    let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(u32::try_from(lbas - 1).unwrap_or(0xFFFF_FFFF));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open the decoy disk: {e}"))?;
    mbr.overwrite_lba0(&mut file).map_err(|e| format!("write the protective MBR: {e}"))?;
    drop(file);

    let mut disk = gpt::GptConfig::new()
        .writable(true)
        .initialized(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(path)
        .map_err(|e| format!("open the decoy disk as GPT: {e}"))?;
    disk.update_partitions(BTreeMap::new())
        .map_err(|e| format!("initialise the decoy table: {e}"))?;

    // Written in key order into array slots 0, 1, 2 — so the ESP-typed decoy
    // is what anything selecting by type or by position would land on.
    let after = esp.last_lba + 1;
    let mut parts = BTreeMap::new();
    parts.insert(
        1,
        part(partition_types::EFI, "11111111-2222-3333-4444-555555555555", after, after + 99),
    );
    parts.insert(
        2,
        part(partition_types::LINUX_FS, "66666666-7777-8888-9999-AAAAAAAAAAAA", after + 100, after + 199),
    );
    parts.insert(
        3,
        part(partition_types::EFI, &esp.guid, esp.first_lba + shift, esp.last_lba),
    );
    disk.update_partitions(parts).map_err(|e| format!("write the decoy table: {e}"))?;
    disk.write().map_err(|e| format!("persist the decoy table: {e}"))?;

    // Certify the instrument before trusting a green run: read the disk back
    // with the same outside implementation and check it says what was meant.
    let back = gpt::GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open(path)
        .map_err(|e| format!("the crafted disk does not parse: {e}"))?;
    let seen: Vec<String> = back
        .partitions()
        .values()
        .map(|p| p.part_guid.to_string().to_uppercase())
        .collect();
    if seen.len() != 3 || seen[2] != esp.guid {
        return Err(format!(
            "the crafted disk holds {seen:?}, wanted three entries ending in {}",
            esp.guid
        ));
    }
    Ok(())
}

fn part(ty: partition_types::Type, guid: &str, first_lba: u64, last_lba: u64) -> Partition {
    Partition {
        part_type_guid: ty,
        part_guid: guid.parse().expect("a literal GUID"),
        first_lba,
        last_lba,
        flags: 0,
        name: String::new(),
    }
}
