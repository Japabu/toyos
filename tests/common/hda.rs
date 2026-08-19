//! H4's audio arm, in the harness: soundd driving a real Intel HDA controller
//! itself, read back off the device rather than off the guest's opinion.
//! H0's own feasibility diagnostic was built ahead of this, then deleted once
//! the driver above answered every question it was asked for.

use std::path::Path;
use std::time::Duration;

use crate::common::audio::{await_null_sink, NULL_SINK};
use crate::common::qemu::{BootOptions, Profile, QemuInstance};
use crate::common::serial::Serial;

/// H4's gate: a 440 Hz tone out of an Intel HDA controller soundd drives
/// itself, read back off the device rather than off the guest's opinion.
///
/// `-audiodev wav` is the same ground truth gate A's four recorded configs use,
/// and the machine differs from them in the sound card alone
/// ([`Profile::Hda`]), so the capture is comparable by construction. What is
/// asserted here is **harm** — the tone is present, continuous, and dithered —
/// which is the fast tier's verdict. This is not a gate-A arm: it has no
/// recorded distribution behind it, and the four HDA sections a gate-A arm
/// would need in `tests/audio-baseline.toml` — `audio_tone_hda.smp1`, `.smp8`,
/// `audio_tone_hda_load.smp1`, `.smp8` — are unrecorded.
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
            kernel_params: &["hda-allowlist-selftest"],
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
        return Err(err.to_string());
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
    // The refusal is a kernel boot line and is in the capture already; soundd's
    // answer to it is a userland line that races the ready marker, so it is
    // waited for on the guest's clock rather than on a span of the host's.
    let mut text = qemu.boot_log().to_string();
    let stalled = await_null_sink(&mut qemu, &mut text).err();
    let log = Serial::named("boot console", text);

    log.must_say("hda: 00:")?;
    log.must_say("has a live link (statests=")?;
    log.must_say("controllers answer on this machine")?;
    log.must_say("refused by name, no HDA audio")?;
    log.must_not_say("bound, statests=")?;
    // The machine still boots and still has a sink: absence of hardware is a
    // routing state, and a refusal must not be a machine that will not run.
    log.must_say(NULL_SINK)
        .map_err(|why| match stalled {
            Some(stall) => format!("{stall}\n{why}"),
            None => why,
        })?;
    log.must_say("Boot: complete")?;
    log.must_be_clean()
}
