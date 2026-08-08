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
    // `scoped-by=device-scope` rather than the catch-all, and this one has
    // ground truth beside it: QEMU's DMAR really does publish a `pci-endpoint`
    // scope per function, which the *other* decoder of the same table prints as
    // `iommu: unit0 scope pci-endpoint 00:10.0`. Two independent readers
    // agreeing on one firmware table is what makes the walk checked rather than
    // cited — and it is what would catch a claim that fell through to
    // `include-pci-all` because the path decode was wrong.
    must_say(&multifunction, "hda: (a) unit=0 scoped-by=device-scope", MULTIFUNCTION)?;
    log.must_say(&format!("iommu: unit0 scope pci-endpoint {MULTIFUNCTION}"))?;

    // `intel-hda` offers `msi=<OnOffAuto>` and no MSI-X option, measured with
    // `-device intel-hda,help` on this host. So the eligible-by-MSI arm is the
    // one this machine runs, and it is the arm `hda-driver-plan.md` §4.2 argues
    // is the better of the two.
    must_say(&multifunction, "hda: (a) msi vectors=", MULTIFUNCTION)?;
    must_say(&multifunction, "msix=none", MULTIFUNCTION)?;

    must_say(&multifunction, BAR0_BYTES, MULTIFUNCTION)?;
    must_say(&multifunction, "64bit=n prefetch=n movable=y", MULTIFUNCTION)?;
    // The neighbour scan is the part of (a) that decides whether
    // `userspace-drivers-spec.md` stage 3's relocation is load-bearing, and on
    // this machine firmware packs every small register window into one 2 MiB
    // page — so a zero here means the scan found nothing rather than that
    // nothing is there, and the assertion would be vacuous without it.
    let neighbours = lines(&multifunction, "hda: bar0 shares its 2 MiB page with ");
    if neighbours.is_empty() {
        return Err(format!(
            "nothing shares {MULTIFUNCTION}'s 2 MiB page, on a machine whose BARs are all packed \
             into one — the scan found nothing:\n{multifunction}"
        ));
    }
    must_say(
        &multifunction,
        &format!("2m-page-neighbours={}", neighbours.len()),
        MULTIFUNCTION,
    )?;

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
        "(audio)",
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

    // **QEMU cannot stage a speaker pin.** `hda-output` and `hda-duplex` fix
    // their configuration defaults in the device model — there is no property
    // for it — and both report line-out. So the arm this harness runs is §2.3's
    // *refusal*, and the arm where a traversal has a candidate is the T14's to
    // produce. Asserting the refusal is what keeps that honest: a probe that
    // called a line-out pin a speaker would go green on the sentence below.
    must_say(&multifunction, "hda: (c) pins reporting a speaker default device: 0", MULTIFUNCTION)?;
    must_say(&multifunction, "would refuse this machine", MULTIFUNCTION)?;

    // Every class-0403 function, not the first — the defect `pci.rs` records
    // one layer down and §2.3 records one layer up.
    log.must_say("hda: 3 class 0403 functions on this machine")?;

    // The probe finished. On a machine with no serial port a probe that hung
    // is a black screen, so this is the assertion the whole design is for —
    // and it is not implied by the ones above: every one of them could be
    // satisfied by a boot that then wedged on the last controller.
    log.must_say("hda: === H0 probe done ===")?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()?;

    ordinary_boot_is_untouched()
}

/// The other half of the feature's promise: **the probe is the feature and
/// nothing else.**
///
/// Same machine, same three controllers, a kernel without the flag. Not implied
/// by anything above — a probe wired into `drivers::init` rather than behind it
/// would satisfy every assertion in this file.
///
/// It can no longer be "the log says nothing about HDA": H4's driver brings
/// every class-0403 function up on every boot, which is what it is for. What
/// the flag owns is the four verdict blocks and the widget dump, and those are
/// what must be absent.
fn ordinary_boot_is_untouched() -> Result<(), String> {
    let log = probe_boot(&[])?;
    for absent in ["=== H0 probe", "hda: (a)", "hda: (b)", "hda: (c)", "hda: (d)", "hda: codec"] {
        log.must_not_say(absent)?;
    }
    // And the driver did run, so the assertion above is about the flag rather
    // than about a boot that never reached the controllers.
    log.must_say("hda: 00:10.0")?;
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

/// H4's gate: a 440 Hz tone out of an Intel HDA controller soundd drives
/// itself, read back off the device rather than off the guest's opinion.
///
/// `-audiodev wav` is the same ground truth gate A's four recorded configs use,
/// and the machine differs from them in the sound card alone
/// ([`Profile::Hda`]), so the capture is comparable by construction. What is
/// asserted here is **harm** — the tone is present, continuous, and dithered —
/// which is the fast tier's verdict. This is not a gate-A arm: it has no
/// recorded distribution behind it, and §5.3's four baseline sections are
/// unrecorded (`specs/hda-driver-plan.md` §6.6).
pub fn hda_tone(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: Profile::Hda,
            kernel_features: &["hda-allowlist-selftest"],
            ..Default::default()
        },
    );
    // soundd claims and configures the controller the instant it starts, which
    // is after the ready marker and before any test command — a window neither
    // `boot_log` nor `run_test`'s own capture covers.
    let mut log = Serial::boot(&qemu);
    log.push(&qemu.drain_serial(Duration::from_millis(500)));

    let result = qemu.run_test("test_rs_audio_tone", Duration::from_secs(30));
    if let Some(err) = &result.error {
        return Err(err.clone());
    }
    if result.exit_code != Some(0) {
        return Err(format!("the tone did not play: {:?}\n{}", result.exit_code, result.stdout));
    }
    log.push(&result.serial);
    log.push(&qemu.drain_serial(Duration::from_millis(500)));
    let serial = log.text().to_string();
    log.must_say("hda: 00:")?;
    log.must_say("bound, statests=")?;
    log.must_say("soundd: hda codec0 vendor=1af4")?;
    log.must_say("-> pin 0x03 (line-out)")?;
    log.must_say("soundd: hda path configured in")?;
    if serial.contains("presenting a null sink") {
        return Err(format!("soundd fell back to the null sink:\n{serial}"));
    }

    // The allow-list, every arm, on the one caller that can reach it.
    for want in [
        "hda: selftest write ICW written",
        "hda: selftest write SDnFMT written",
        "hda: selftest write SDnCTL written",
        "hda: selftest write SDnCTL-tag written",
        "hda: selftest write SDnBDPL refused",
        "hda: selftest write SDnBDPU refused",
        "hda: selftest write SDnCBL refused",
        "hda: selftest write SDnLVI refused",
        "hda: selftest write SDnSTS refused",
        "hda: selftest write SDnCTL-srst refused",
        "hda: selftest write SDnCTL-wide refused",
        "hda: selftest write INTCTL refused",
        "hda: selftest write GCTL refused",
        "hda: selftest read ICS read",
        "hda: selftest read IRR read",
        "hda: selftest read SDnLPIB refused",
        "hda: selftest read STATESTS refused",
    ] {
        log.must_say(want)?;
    }

    let wav = crate::common::audio::parse_wav(qemu.audio_wav_path())?;
    let analysis = crate::common::audio::analyze(&wav);
    if analysis.peak < 8000 {
        return Err(format!(
            "the capture peaks at {} — the tone plays at 16000 and nothing reached the device",
            analysis.peak
        ));
    }
    let gaps = crate::common::audio::gap_histogram(&analysis, wav.sample_rate);
    let dropouts: u32 = gaps.values().sum();
    let breaks = crate::common::audio::phase_breaks(&wav);
    let pitch = crate::common::audio::dominant_hz(&wav);
    eprintln!(
        "  [hda] {} frames at {} Hz {} ch, peak {} active {:.2}s dither {:.1}% pitch {:.1}Hz \
         gaps {} phase-breaks {}",
        wav.mono.len(),
        wav.sample_rate,
        wav.channels,
        analysis.peak,
        analysis.active_samples as f64 / wav.sample_rate as f64,
        analysis.dither_ratio.unwrap_or(0.0) * 100.0,
        pitch.unwrap_or(0.0),
        crate::common::audio::format_histogram(&gaps),
        breaks.len(),
    );
    if dropouts > 0 {
        return Err(format!(
            "{dropouts} mid-tone silences in the capture: {}",
            crate::common::audio::format_histogram(&gaps)
        ));
    }
    // The rate the engine plays at is soundd's decision on this machine and
    // nothing else here can see it: a stream format naming the wrong base is
    // eight buffers of correct audio a second played 8.8% fast, which every
    // other assertion in this file passes.
    if let Some(complaint) = crate::common::audio::wrong_pitch(&wav) {
        return Err(complaint);
    }
    // The instrument the gap detector cannot be: an engine that replays a
    // period nobody refilled puts the tone back 0.28 of a cycle out, and
    // nothing about that is silent (§5.3 item 5, risk 7). Zero here and zero on
    // all four virtio configs, measured — so the check has a calibration and
    // not just a threshold.
    //
    // `dither_ratio` is deliberately not asserted, and is printed above so the
    // difference is visible rather than hidden. It measures the longest
    // *silent* run, and QEMU's two device models put different silence there:
    // virtio-sound's capture opens before the stream does, so its longest
    // silent run is soundd's own dithered output (24.6% on this host), while
    // `intel-hda`'s wav voice runs only while the stream does and the longest
    // silent run is host padding at the ends of the file. The virtio arm still
    // asserts it, over a stretch that is soundd's.
    if !breaks.is_empty() {
        let where_ = |n: &usize| {
            format!("{n} (period {:.1}, {:?})", *n as f64 / 128.0, &wav.mono[n - 1..=n + 1])
        };
        return Err(format!(
            "the captured tone is not one sine: {} phase breaks at {}",
            breaks.len(),
            breaks.iter().take(8).map(where_).collect::<Vec<_>>().join(", ")
        ));
    }
    log.must_be_clean()
}

/// The T14's panic, staged: a client that stops producing mid-stream.
///
/// **Ground truth is which of two things soundd did with the periods the client
/// did not cover**, and the two machines must answer differently. HDA's engine
/// is a cyclic ring — it plays buffer `i` again `num_buffers` periods after
/// completing it, whatever soundd put there — so a period held back for a
/// client is played as silence anyway and then completed a second time, which
/// is a completion for a buffer soundd still holds. virtio-sound's queue plays
/// nothing soundd has not submitted, so holding one costs nothing and §5.10's
/// deferral is exactly right there.
///
/// So: the ring arm must report `underruns` (soundd filled the periods and had
/// no client audio for them) and the queue arm must report `deferred` (soundd
/// held them). Asserting both is what stops the two obvious wrong fixes —
/// deleting the deferral, which reds the queue arm, and letting the ring hold a
/// period, which reds the ring arm with the panic this exists for.
///
/// Nothing in the tone clients reaches this state: they keep their rings full,
/// so `hda_tone` measured `deferred=0` on every run. The stall is the actuator,
/// and it has to outlast one lap of the ring — 8 periods, 23.2 ms — or the
/// engine never comes back round to a period soundd is holding.
pub fn hda_client_stall(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let ring = stall_run(test_config, c_bins, rust_bins, "ring", Profile::Hda)?;
    let queue = stall_run(test_config, c_bins, rust_bins, "queue", Profile::Headless)?;

    if ring.underruns == 0 {
        return Err(format!(
            "the ring arm reports no underrun: soundd never filled a period the stalled client \
             had not covered, so this run staged nothing\n{}",
            ring.serial
        ));
    }
    if ring.deferred != 0 {
        return Err(format!(
            "the ring arm deferred {} period(s): the engine replays every one of them and then \
             completes it again, which is the panic this test exists for\n{}",
            ring.deferred, ring.serial
        ));
    }
    if queue.deferred == 0 {
        return Err(format!(
            "the queue arm deferred nothing: §5.10 is what a stalled client is supposed to buy on \
             a device that plays only what it is given\n{}",
            queue.serial
        ));
    }
    eprintln!(
        "  [hda] stalled client: ring filled {} period(s) it had no audio for and held none; \
         queue held {}",
        ring.underruns, queue.deferred
    );
    Ok(())
}

struct StallRun {
    underruns: u32,
    deferred: u32,
    serial: String,
}

/// One boot of the stalling client, and what soundd did with the periods.
///
/// soundd's liveness is the first verdict and it is not implied by the client's
/// exit: the client talks to soundd over IPC and a soundd that died mid-stream
/// leaves it blocked, so the run times out rather than reporting a code. The
/// panic line is checked by name anyway — `must_be_clean` would catch it, but
/// not say which of soundd's assertions it was.
fn stall_run(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    arm: &str,
    profile: Profile,
) -> Result<StallRun, String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions { profile, ..Default::default() },
    );
    let mut log = Serial::boot(&qemu);
    let result = qemu.run_test("test_rs_hda_client_stall", Duration::from_secs(60));
    if let Some(err) = &result.error {
        return Err(format!("the {arm} arm: {err}\n{}\n{}", result.stdout, result.serial));
    }
    log.push(&result.serial);
    log.push(&qemu.drain_serial(Duration::from_millis(500)));
    if result.exit_code != Some(0) {
        return Err(format!(
            "the stalling client exited {:?} on the {arm} arm:\n{}\n{}",
            result.exit_code,
            result.stdout,
            log.text()
        ));
    }
    log.must_not_say("repeated completion for free buffer")?;
    log.must_be_clean()?;

    let serial = log.text().to_string();
    // The client plays twice with a suspend between, so a resume is under test
    // as much as the stall is: on a ring the drain gives its periods up rather
    // than holding them, and what the second prime fills and where in the ring
    // it starts are both what the first stream left behind.
    let resumes = serial.matches("soundd: resumed").count();
    if resumes < 2 {
        return Err(format!(
            "soundd resumed {resumes} time(s) on the {arm} arm — the second stream did not find a \
             suspended daemon, so nothing here tests a resume:\n{serial}"
        ));
    }
    let counters = crate::common::audio::parse_soundd_counters(&serial)?;
    if counters.windows == 0 {
        return Err(format!("soundd reported no stats window on the {arm} arm:\n{serial}"));
    }
    Ok(StallRun { underruns: counters.underruns, deferred: sum_field(&serial, "deferred"), serial })
}

/// Sum one `soundd:` counter across every stats window.
///
/// `parse_soundd_counters` stops at the fields gate A's baseline records, and
/// `deferred` is not one of them — it is an activity signal with no ceiling. It
/// is read here because it is the whole difference between the two arms.
fn sum_field(serial: &str, key: &str) -> u32 {
    let needle = format!(" {key}=");
    serial
        .match_indices(&needle)
        .filter_map(|(at, _)| {
            let rest = &serial[at + needle.len()..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u32>().ok()
        })
        .sum()
}

/// Two controllers, both with a codec that answers.
///
/// The kernel binds neither and names both. A first-match bind would go green
/// on every other test in this file, and it is the defect `pci.rs` records one
/// layer down — so this is the arm that makes the rule tested rather than
/// merely written.
pub fn hda_two_live_refused(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions { profile: Profile::HdaTwoLive, ..Default::default() },
    );
    let mut log = Serial::boot(&qemu);
    log.push(&qemu.drain_serial(Duration::from_secs(1)));

    log.must_say("hda: 00:")?;
    log.must_say("has a live link (statests=")?;
    log.must_say("controllers answer on this machine")?;
    log.must_say("refused by name, no HDA audio")?;
    log.must_not_say("bound, statests=")?;
    // The machine still boots and still has a sink: absence of hardware is a
    // routing state, and a refusal must not be a machine that will not run.
    log.must_say("presenting a null sink")?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()
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
