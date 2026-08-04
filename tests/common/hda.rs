//! Stage H0 of `specs/hda-driver-plan.md`, in the harness.
//!
//! What this can certify and what it cannot is the honest half. The probe's
//! *branches* are certifiable here — every arm of every one of H0's four
//! questions runs on [`Profile::MetalHda`], because that machine is built to
//! present both answers to each. Which arm the T14 runs is the T14's to say,
//! and QEMU's codec model is a handful of widgets where a real one has tens.
//! So a green here means "the probe asks correctly and reports what it found";
//! it means nothing at all about what `00:1f.3` will answer.

use std::path::Path;
use std::time::Duration;

use crate::common::qemu::{self, BootOptions, Profile, QemuInstance};
use crate::common::serial::Serial;

const FEATURE: &[&str] = &["hda-probe"];

/// The three controllers [`Profile::MetalHda`] stages, by the bus address the
/// probe names each one. Read from here rather than restated per assertion:
/// the addresses are the profile's shape and a test that spelled them again
/// would go green on a profile that had moved them.
const MULTIFUNCTION: &str = "00:10.0";
const MULTIFUNCTION_SIBLING: &str = "00:10.1";
const SINGLETON: &str = "00:11.0";

/// QEMU's `intel-hda` BAR0, measured with QMP `query-pci` on this host
/// (QEMU 11.0.3): one memory BAR at index 0, 16,384 bytes, not 64-bit, not
/// prefetchable. The probe sizes it off the device; this is the host side of
/// the same fact, which is what makes the assertion ground truth rather than
/// the guest agreeing with itself.
const BAR0_BYTES: &str = "size=0x4000";

pub fn hda_probe(
    _test_config: &Path,
    _c_bins: &[(String, Vec<u8>)],
    _rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let log = probe_boot(FEATURE)?;

    // (a) handoff, both arms of the scope rule on one machine.
    let scopes = lines(log.text(), "hda: (a) scope members=");
    let multifunction = section(&log, MULTIFUNCTION)?;
    let singleton = section(&log, SINGLETON)?;
    if scopes.len() != 3 {
        return Err(format!(
            "three class-0403 functions on this machine, {} scope verdicts: {scopes:?}",
            scopes.len()
        ));
    }
    must_say(&multifunction, "hda: (a) scope members=2", MULTIFUNCTION)?;
    must_say(&multifunction, "not a singleton", MULTIFUNCTION)?;
    must_say(&singleton, "hda: (a) scope members=1", SINGLETON)?;
    must_say(&singleton, "a singleton", SINGLETON)?;
    // The sibling has to be named, not counted: §7.3's refusal is what it
    // refuses *with*, and gate N inherits this answer through the I219 sitting
    // beside the T14's HDA.
    must_say(&multifunction, &format!("hda: scope sibling {MULTIFUNCTION_SIBLING}"), MULTIFUNCTION)?;

    // Root-complex-integrated, which is where q35 puts a device with no
    // upstream port and where Tiger Lake puts `00:1f.3`.
    must_say(&multifunction, "hda: scope upstream-bridge none", MULTIFUNCTION)?;

    // QEMU publishes no RMRR at all (`iommu-spec.md` §8), so this arm is the
    // *absence* and the T14 is the first machine that can produce the other.
    must_say(&multifunction, "hda: (a) rmrr none", MULTIFUNCTION)?;
    must_say(&multifunction, "hda: (a) unit=", MULTIFUNCTION)?;

    // `intel-hda` offers `msi=<OnOffAuto>` and no MSI-X option, measured with
    // `-device intel-hda,help` on this host. So the eligible-by-MSI arm is the
    // one this machine runs, and it is the arm `hda-driver-plan.md` §4.2 argues
    // is the better of the two.
    must_say(&multifunction, "hda: (a) msi vectors=", MULTIFUNCTION)?;
    must_say(&multifunction, "msix=none", MULTIFUNCTION)?;

    must_say(&multifunction, BAR0_BYTES, MULTIFUNCTION)?;
    must_say(&multifunction, "64bit=n prefetch=n movable=y", MULTIFUNCTION)?;

    // `ECAP.SC` on the unit, which §4.4 item 4 spends to avoid a config-space
    // write path. QEMU's `snoop-control` is off unless asked for and no profile
    // asks, so `n` is the answer here and the T14's is unread.
    let unit = log
        .must_say("iommu: unit0 ")
        .map_err(|e| format!("{e}\nthe unit line is where ECAP.SC is decoded"))?;
    if !unit.contains(" sc=") {
        return Err(format!("the unit line does not decode ECAP.SC: {unit:?}"));
    }

    // (b) Is a codec on the link at all. Both answers, on one boot.
    let sibling = section(&log, MULTIFUNCTION_SIBLING)?;
    must_say(&sibling, "hda: (b) statests=0x0000", MULTIFUNCTION_SIBLING)?;
    must_say(&sibling, "NO CODEC ON THE LEGACY LINK", MULTIFUNCTION_SIBLING)?;
    must_say(&sibling, "hda: (d) codecs=0", MULTIFUNCTION_SIBLING)?;
    must_say(&multifunction, "a codec answers on the legacy link", MULTIFUNCTION)?;

    // (d) More than one codec, which is the trap a driver taking the first
    // match falls into: `cad=0` and `cad=1` are two codecs on one controller,
    // exactly as an Iris Xe's display audio sits beside the analogue codec.
    must_say(&multifunction, "hda: (d) codecs=2", MULTIFUNCTION)?;
    must_say(&multifunction, "first match can bind display audio", MULTIFUNCTION)?;
    must_say(&singleton, "hda: (d) codecs=1", SINGLETON)?;
    if !multifunction.contains("hda: codec0 vendor=") || !multifunction.contains("hda: codec1 vendor=")
    {
        return Err(format!(
            "both codecs on {MULTIFUNCTION} must be dumped by address, not just counted:\n\
             {multifunction}"
        ));
    }

    // (c) The dump that becomes H1's fixture. What is asserted is that every
    // kind of line the fixture needs is present and carries its raw word —
    // a decoded name with no number beside it is a fixture that has lost the
    // codec's own answer.
    for want in [
        "hda: codec0 fg=",
        "type=audio",
        "hda: codec0 node=",
        "(audio-out)",
        "(pin)",
        "cfgdef=0x",
        "caps=0x",
    ] {
        must_say(&multifunction, want, MULTIFUNCTION)?;
    }
    let pins = lines(&multifunction, "cfgdef=0x");
    if pins.is_empty() {
        return Err(format!("no pin complex reported a configuration default:\n{multifunction}"));
    }
    for pin in &pins {
        if !pin.contains(" device=") || !pin.contains(" conn=") {
            return Err(format!("a configuration default is undecoded: {pin:?}"));
        }
    }
    eprintln!("  [hda] {} pin complexes dumped on {MULTIFUNCTION}", pins.len());

    // The probe finished. On a machine with no serial port a probe that hung
    // is a black screen, so this is the assertion the whole design is for —
    // and it is not implied by the ones above: every one of them could be
    // satisfied by a boot that then wedged on the last controller.
    log.must_say("hda: === H0 probe done ===")?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()?;

    ordinary_boot_is_untouched()
}

/// The other half of the feature's promise: **nothing in the ordinary boot
/// path takes that controller out of reset.**
///
/// Same machine, same three controllers, a kernel without the feature. Not
/// implied by anything above — a probe wired into `drivers::init` rather than
/// behind the flag would satisfy every assertion in this file.
fn ordinary_boot_is_untouched() -> Result<(), String> {
    let log = probe_boot(&[])?;
    log.must_not_say("hda:")?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()
}

/// One diagnostic boot of the three-controller machine.
///
/// The **diag config**, because that is the image H0 ships in: no test binaries
/// in the initrd and no process that can claim the framebuffer, so what the
/// harness boots here is what gets flashed.
fn probe_boot(features: &'static [&'static str]) -> Result<Serial, String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("diag");
    let options = BootOptions {
        profile: Profile::MetalHda,
        kernel_features: features,
        // No test runner in this image, so the kernel's own last phase line is
        // the marker.
        ready_marker: "Boot: complete",
        ..Default::default()
    };
    crate::metal_sim_argv_check(&qemu::profile_argv(&options))?;
    let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
    let mut log = Serial::boot(&qemu);
    // Past the marker: the probe runs in the peripheral phase, so everything it
    // logs is already in the ring by then — but a controller that wedges after
    // it would show here and nowhere else.
    log.push(&qemu.drain_serial(Duration::from_secs(1)));
    Ok(log)
}

/// Every `hda:` line from one controller's banner up to the next one's.
///
/// The probe walks the controllers in bus order and prints a banner per
/// controller, so this is what separates three interleaved answers. Splitting
/// on the banner rather than filtering by address is deliberate: most of the
/// probe's lines do not repeat the address, and a test that searched the whole
/// log for them would pass on the wrong controller's answer.
fn section(log: &Serial, address: &str) -> Result<String, String> {
    let banner = format!("hda: controller {address} ");
    let mut out = String::new();
    let mut inside = false;
    for line in log.text().lines() {
        if line.contains("hda: controller ") {
            inside = line.contains(&banner);
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.is_empty() {
        return Err(format!("no probe output for the controller at {address}:\n{}", log.text()));
    }
    Ok(out)
}

fn lines(text: &str, needle: &str) -> Vec<String> {
    text.lines().filter(|l| l.contains(needle)).map(str::to_string).collect()
}

fn must_say(section: &str, needle: &str, address: &str) -> Result<(), String> {
    if section.contains(needle) {
        return Ok(());
    }
    Err(format!("the controller at {address} never said {needle:?}:\n{section}"))
}
