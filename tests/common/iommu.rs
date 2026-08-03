//! Stage I1 of `specs/iommu-spec.md`: what the kernel read off the machine's
//! remapping units.
//!
//! The trap this gate exists to avoid is the one a discovery test falls into
//! by default. A kernel that printed a plausible capability line without
//! reading a register would satisfy any single-machine assertion, and so would
//! a decode reading the wrong bits of the right register. So the assertions
//! are not "the line is there": three machines are booted whose units differ
//! in exactly one advertised capability each, and the gate is that the guest's
//! decode *moves with them*. A constant cannot track a register it never read.
//!
//! Ground truth is split, deliberately. Whether the unit exists at all is
//! invisible to every console line — a kernel that says "no DMAR" on a machine
//! that has one and a harness that forgot the device produce the same log — so
//! presence is checked against the argv, which is the host side of the device.
//! What the unit *says* can only come from the guest, so that half is checked
//! against the console.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::qemu::{self, BootOptions, Profile, QemuInstance};
use super::serial::Serial;

/// The four machines, and what each one moves.
///
/// [`Profile::Metal`] is the reference: the configuration every other profile
/// in the suite runs, so a difference below is a difference the profile made
/// and not one the shape did — all four are metal-sim and differ in the unit
/// alone.
const MACHINES: &[Profile] =
    &[Profile::Metal, Profile::NoIommu, Profile::IommuNarrow, Profile::IommuNoIntremap];

pub fn iommu_discovery(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut decoded: BTreeMap<&str, BTreeMap<String, String>> = BTreeMap::new();

    for &profile in MACHINES {
        let name = profile_name(profile);
        let options = BootOptions { profile, ..Default::default() };
        argv_check(profile, &qemu::profile_argv(&options))?;

        let qemu = QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
        let log = Serial::boot(&qemu);
        // Discovery runs in the storage phase, long before userland; a machine
        // that did not finish booting is a machine whose log says nothing
        // about what came after the unit.
        log.must_be_clean()?;
        log.must_say("Boot: complete")?;

        let Some(unit) = profile.iommu() else {
            // `Absent` is firmware answering the question, and the answer is
            // one a user can act on — so the line names the firmware setting
            // as well as the hardware. What makes this assertion mean
            // something is the pair below it: a kernel that always printed
            // this would fail on every other machine here.
            log.must_say("iommu: no DMAR table")?;
            log.must_say("VT-d is disabled in firmware setup")?;
            log.must_not_say("iommu: unit")?;
            log.must_not_say("iommu: DMAR haw=")?;
            eprintln!("  [iommu] {name}: no DMAR, and no unit described");
            continue;
        };

        // The kernel reports the width one greater than the field holds, so a
        // machine declaring 48 bits of host address is the aw-bits the profile
        // asked for. Both halves of the table's own header are asserted:
        // `INTR_REMAP` is a platform-level flag and `ECAP.IR` below is the
        // unit's, and `specs/iommu-spec.md` §2.2 refuses on them separately.
        log.must_say(&format!("iommu: DMAR haw={}", unit.aw_bits))?;
        log.must_say(&format!(
            "intr_remap={}",
            if unit.intremap { 'y' } else { 'n' }
        ))?;

        let line = log.must_say("iommu: unit0 @")?;
        let fields = unit_fields(line);
        let field = |k: &str| -> Result<String, String> {
            fields
                .get(k)
                .cloned()
                .ok_or_else(|| format!("{name}: the unit line has no {k}= field: {line:?}"))
        };

        // The decode, against what the profile asked QEMU for.
        expect(&field("aw")?, &unit.aw_bits.to_string(), "aw", name, line)?;
        expect(&field("ir")?, if unit.intremap { "y" } else { "n" }, "ir", name, line)?;
        // Not a profile dimension, and asserted because the whole suite rests
        // on it: `caching-mode=on` is what makes QEMU's IOTLB a real cache and
        // the map-side invalidation load-bearing at stage I4 (§5.5), and 2 MiB
        // leaf entries are what this kernel's one page size requires (§5.4).
        expect(&field("cm")?, "y", "cm", name, line)?;
        expect(&field("sps2m")?, "y", "sps2m", name, line)?;

        // Every scope naming a PCI function must name one this machine has.
        // A decode that read the path bytes at the wrong offset would produce
        // requester ids that look like addresses and match no device.
        let scopes = scope_check(&log, name)?;

        eprintln!(
            "  [iommu] {name}: aw={} ir={} cap={} ecap={} — {scopes} PCI scopes matched",
            field("aw")?,
            field("ir")?,
            field("cap")?,
            field("ecap")?
        );
        decoded.insert(name, fields);
    }

    // The negative control, and the reason this test boots four machines
    // instead of one. Each pair below differs in one QEMU knob, so a decode
    // that reports the same value for both is a decode that is not reading the
    // register the knob moves.
    for (a, b, key) in [
        (profile_name(Profile::Metal), profile_name(Profile::IommuNarrow), "aw"),
        (profile_name(Profile::Metal), profile_name(Profile::IommuNoIntremap), "ir"),
    ] {
        let (Some(left), Some(right)) = (decoded.get(a), decoded.get(b)) else {
            return Err(format!("{a} or {b} produced no unit line to compare"));
        };
        let (Some(lv), Some(rv)) = (left.get(key), right.get(key)) else {
            return Err(format!("no {key}= on {a} or {b}"));
        };
        if lv == rv {
            return Err(format!(
                "{a} and {b} both report {key}={lv}, but their units advertise different \
                 capabilities — the kernel is printing a constant, not decoding a register"
            ));
        }
        // And the raw register the field came out of has to have moved too. A
        // decode of the right register reported through the wrong field would
        // pass the line above on one of these pairs by accident.
        let raw = if key == "aw" { "cap" } else { "ecap" };
        let (Some(lr), Some(rr)) = (left.get(raw), right.get(raw)) else {
            return Err(format!("no {raw}= on {a} or {b}"));
        };
        if lr == rr {
            return Err(format!(
                "{a} and {b} report {key}={lv}/{rv} out of the same {raw}={lr} — the value did \
                 not come from that register"
            ));
        }
        eprintln!("  [iommu] {a} vs {b}: {key} {lv} != {rv}, out of {raw} {lr} != {rr}");
    }

    Ok(())
}

/// The `key=value` pairs on a unit line. `@0xfed90000` carries no `=` and is
/// skipped, which is what makes the split total rather than a parse.
fn unit_fields(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn expect(got: &str, want: &str, key: &str, name: &str, line: &str) -> Result<(), String> {
    if got == want {
        return Ok(());
    }
    Err(format!("{name}: {key}={got}, want {key}={want}\n{line}"))
}

/// The requester ids the unit's scopes name are exactly the functions this
/// machine enumerated. Returns how many.
///
/// Set equality rather than "each one exists", and the difference is the whole
/// value of this check. Measured against the raw table on QEMU 11.0.2: the
/// DRHD carries no `INCLUDE_PCI_ALL` flag and instead lists every PCI function
/// as its own scope, so the two sets are the same set. A path read one byte
/// off produces ids that are still *plausible* — `00:1f.3` becomes `00:03.0`,
/// which on this machine is the NVMe controller — and an each-one-exists check
/// stays green on all seven of them. The set catches it, because five of the
/// seven collapse onto `00:00.0` and four real functions go missing.
///
/// A failure here on a future QEMU that switches to `INCLUDE_PCI_ALL` is a
/// real report and not a false one: which functions a unit's scope names is
/// what stage I2 hands context entries to.
fn scope_check(log: &Serial, name: &str) -> Result<usize, String> {
    let mut scoped: Vec<String> = Vec::new();
    for line in log.text().lines() {
        let Some(rest) = line.split("iommu: unit0 scope ").nth(1) else { continue };
        let mut words = rest.split_whitespace();
        let (Some(kind), Some(who)) = (words.next(), words.next()) else {
            return Err(format!("{name}: unreadable scope line: {line:?}"));
        };
        // An I/O APIC sits on a pseudo-bus no PCI walk sees, and a scope whose
        // path runs through a bridge reports no requester id at all — neither
        // is a name this cross-check can look up.
        if kind == "pci-endpoint" || kind == "pci-bridge" {
            scoped.push(who.to_string());
        }
    }

    let unique: BTreeSet<&String> = scoped.iter().collect();
    if unique.len() != scoped.len() {
        return Err(format!(
            "{name}: the unit names {} scopes but only {} distinct requester ids. A unit cannot \
             name the same requester twice, so the path bytes are being read at the wrong \
             offset: {scoped:?}",
            scoped.len(),
            unique.len()
        ));
    }

    let enumerated = enumerated_functions(log);
    let scoped: BTreeSet<String> = scoped.into_iter().collect();
    if scoped != enumerated {
        return Err(format!(
            "{name}: the unit's scope names {scoped:?} and this machine enumerated \
             {enumerated:?}. On QEMU these are the same set — the DRHD lists every function \
             rather than setting INCLUDE_PCI_ALL."
        ));
    }
    if scoped.is_empty() {
        return Err(format!(
            "{name}: neither the unit nor the PCI walk named a single function, so this \
             comparison is between two empty sets"
        ));
    }
    Ok(scoped.len())
}

/// Every function `pci::enumerate` printed. Anchored on the class field that
/// follows the address, so `xHCI: found at PCI 00:02.0` is not one of them.
fn enumerated_functions(log: &Serial) -> BTreeSet<String> {
    log.text()
        .lines()
        .filter_map(|line| {
            let (bdf, tail) = line.split("PCI ").nth(1)?.split_once(' ')?;
            tail.starts_with('[').then(|| bdf.to_string())
        })
        .collect()
}

fn profile_name(profile: Profile) -> &'static str {
    match profile {
        Profile::Metal => "metal",
        Profile::NoIommu => "no-iommu",
        Profile::IommuNarrow => "narrow",
        Profile::IommuNoIntremap => "no-intremap",
        _ => "unexpected",
    }
}

/// Presence, configuration and *position* of the unit in the argv.
///
/// The last one is the vacuity trap `specs/userspace-drivers-spec.md` §7.2
/// names, in its harness-side form: QEMU hands a PCI function the bypassing
/// address space when the function is created before the unit exists, so a
/// `-device intel-iommu` emitted after the devices it is meant to decode is a
/// unit that decodes nothing — and every assertion above it would still pass.
fn argv_check(profile: Profile, argv: &[String]) -> Result<(), String> {
    let name = profile_name(profile);
    let devices: Vec<&str> = argv
        .windows(2)
        .filter(|w| w[0] == "-device")
        .map(|w| w[1].as_str())
        .collect();
    let unit = devices.iter().find(|d| d.starts_with("intel-iommu"));
    let machine = argv
        .windows(2)
        .find(|w| w[0] == "-machine")
        .map(|w| w[1].as_str())
        .ok_or_else(|| format!("{name}: no -machine in the argv"))?;

    match profile.iommu() {
        None => {
            if let Some(d) = unit {
                return Err(format!("{name} declares no unit but QEMU is given {d}"));
            }
            if machine.contains("kernel-irqchip") {
                return Err(format!(
                    "{name} declares no unit but the machine is still split-irqchip: {machine}"
                ));
            }
        }
        Some(want) => {
            let d = *unit.ok_or_else(|| {
                format!("{name} declares a unit and QEMU is given none: {devices:?}")
            })?;
            for field in [
                format!("aw-bits={}", want.aw_bits),
                format!("intremap={}", if want.intremap { "on" } else { "off" }),
                String::from("caching-mode=on"),
            ] {
                if !d.contains(&field) {
                    return Err(format!("{name}: {field} is not in {d}"));
                }
            }
            if !machine.contains("kernel-irqchip=split") {
                return Err(format!(
                    "{name}: interrupt remapping needs the userspace half of the irqchip, and \
                     the machine is {machine}"
                ));
            }
            if devices[0] != d {
                return Err(format!(
                    "{name}: the unit is not the first -device ({} is), so every function ahead \
                     of it gets QEMU's bypassing address space",
                    devices[0]
                ));
            }
        }
    }
    Ok(())
}
