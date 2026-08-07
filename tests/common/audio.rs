//! Wav capture parsing and glitch analysis for the audio integration tests.
//!
//! The QEMU wav audiodev records a continuous timeline of what the
//! virtio-sound device played. Underruns show up as stretches of digital
//! silence inside an otherwise active signal; clicks show up as
//! sample-to-sample jumps no band-limited signal could produce.
//!
//! The capture timeline is NOT wall clock. QEMU's wav backend writes only
//! while the guest voice is enabled, so the file freezes across every
//! suspended stretch (audio spec §5.8) and splices the next resume directly
//! onto the last stopped sample. Verified empirically: 25s of wall clock with
//! the stream stopped adds zero PCM bytes. Consequence: `analyze` reports an
//! underrun for ANY two signal regions in one capture, at ANY wall-clock gap
//! between them — the spliced silence (drain tail + resume prime) always
//! exceeds `MIN_GAP_SECS`, and `NEAR_SECS` is a proximity window into
//! adjacent *samples*, not wall time, so it can never exonerate the gap. A
//! test that plays two tones in one boot will always go red against a
//! zero-gap baseline; keep one signal region per capture.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::qemu::{self, BootOptions, QemuInstance};

/// Largest magnitude a *silent* mix can reach on the wire, in LSB.
///
/// Derivation from soundd's dither generator (`userland/soundd/src/main.rs`):
/// `Xorshift32::next()` returns `state / 2^32 - 0.5`, i.e. a uniform draw in
/// `[-0.5, +0.5]` LSB; TPDF dither sums two independent draws, so
/// `|dither| <= 1.0` LSB exactly. A period no client covered leaves the f32
/// mix bus at exactly `0.0`, so the sample written to the DMA buffer is
/// `round(dither)` — one of `{-1, 0, +1}`. Hence `|s| <= 1` *is* digital
/// silence, and the bound is tight: `P(|s| = 1) = 0.25`.
///
/// Testing `s == 0` instead would be a detector that only works against a
/// truncating quantizer, which is a spec §5.4 defect, not a property to rely
/// on: with a correct quantizer 75% of silent samples are 0, so the longest
/// run of exact zeros in 4M silent samples measures 47 — well under the
/// `MIN_GAP_SECS` floor of 88. Such a detector reports "no dropouts" forever.
///
/// The band is far too narrow to swallow the 440 Hz test tone: at amplitude
/// 16000 the tone slews ~1000 LSB per sample through its zero crossing, so at
/// most one sample per crossing lands inside it.
const SILENCE_MAX: i32 = 1;
/// Silent runs shorter than this are ignored (the test tone dips through the
/// silence band for a single sample at each zero crossing).
const MIN_GAP_SECS: f64 = 0.002;
/// A silent run only counts as an underrun if there is signal within this
/// window on BOTH sides — i.e. it interrupts active playback.
const NEAR_SECS: f64 = 0.25;
/// Amplitude above which a sample counts as signal rather than noise floor.
const SIGNAL_THRESHOLD: i32 = 500;
/// A single-sample jump larger than this is a click: the 440Hz test tone has
/// a max per-sample delta of ~1.1k at 44.1kHz, and any sane audio is
/// band-limited far below this.
const CLICK_DELTA: i32 = 8000;
/// Device period: 512 period_bytes at 44.1kHz stereo 16-bit = 128 frames
/// = 2.902ms (`toyos_abi::virtio_sound::PERIOD_BYTES`, and the HDA stub's is the
/// same number for the same reason). Underruns are period-quantized — the device
/// plays silence one period at a time — so gap lengths are reported in whole
/// periods.
pub const PERIOD_SECS: f64 = 128.0 / 44100.0;

pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    /// Channel 0 only — soundd mixes identical data to all channels.
    pub mono: Vec<i32>,
}

pub struct SilentRun {
    pub start: usize,
    pub len: usize,
}

pub struct Click {
    pub index: usize,
    pub from: i32,
    pub to: i32,
}

pub struct Analysis {
    /// Mid-signal silent runs >= MIN_GAP_SECS: underruns.
    pub underruns: Vec<SilentRun>,
    /// Hard discontinuities not at the edges of counted zero runs.
    pub clicks: Vec<Click>,
    /// Samples with amplitude above SIGNAL_THRESHOLD.
    pub active_samples: usize,
    pub peak: i32,
    /// Fraction of non-zero samples in the capture's longest silent stretch —
    /// the detector's own precondition, measured. TPDF dither into a
    /// round-to-nearest quantizer puts 25% of silent samples at ±1; a
    /// truncating quantizer puts 0% there and collapses this detector's band
    /// back onto `s == 0`, at which point the gate passes while measuring
    /// nothing. `None` when the capture has no silent stretch to judge.
    pub dither_ratio: Option<f64>,
}

/// Floor on `Analysis::dither_ratio`. The expected value is 0.25; anything
/// this far below it means the dither is gone, not that it got unlucky
/// (over the ~6000-sample stretches these captures contain, the sampling
/// error on 0.25 is under 0.01).
pub const MIN_DITHER_RATIO: f64 = 0.10;

/// Parse a 16-bit PCM RIFF wav. The QEMU wav backend leaves the RIFF/data
/// size fields at 0 until clean shutdown, so sizes are advisory: a zero data
/// size means "read to EOF".
pub fn parse_wav(path: &Path) -> Result<Wav, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{}: not a RIFF/WAVE file", path.display()));
    }

    let mut channels: Option<u16> = None;
    let mut sample_rate: Option<u32> = None;
    let mut data: Option<&[u8]> = None;

    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        match id {
            b"fmt " => {
                let fmt = bytes
                    .get(body_start..body_start + 16)
                    .ok_or("truncated fmt chunk")?;
                let audio_format = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
                let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
                if audio_format != 1 || bits != 16 {
                    return Err(format!(
                        "unsupported wav format: audio_format={audio_format} bits={bits}"
                    ));
                }
                channels = Some(u16::from_le_bytes(fmt[2..4].try_into().unwrap()));
                sample_rate = Some(u32::from_le_bytes(fmt[4..8].try_into().unwrap()));
                pos = body_start + size;
            }
            b"data" => {
                let end = if size == 0 || body_start + size > bytes.len() {
                    bytes.len()
                } else {
                    body_start + size
                };
                data = Some(&bytes[body_start..end]);
                pos = end;
            }
            other => {
                return Err(format!(
                    "unexpected wav chunk {:?} — QEMU writes only fmt+data",
                    String::from_utf8_lossy(other)
                ));
            }
        }
    }

    let channels = channels.ok_or("wav has no fmt chunk")?;
    let sample_rate = sample_rate.ok_or("wav has no fmt chunk")?;
    let data = data.ok_or("wav has no data chunk")?;
    if channels == 0 || sample_rate == 0 {
        return Err(format!("degenerate wav format: {channels}ch {sample_rate}Hz"));
    }

    let frame_bytes = channels as usize * 2;
    let mono = data
        .chunks_exact(frame_bytes)
        .map(|frame| i16::from_le_bytes(frame[0..2].try_into().unwrap()) as i32)
        .collect();

    Ok(Wav {
        sample_rate,
        channels,
        mono,
    })
}

pub fn analyze(wav: &Wav) -> Analysis {
    let mono = &wav.mono;
    let rate = wav.sample_rate as f64;
    let min_gap = (MIN_GAP_SECS * rate) as usize;
    let near = (NEAR_SECS * rate) as usize;

    let sig_runs = silent_runs(mono, min_gap);

    let has_signal = |range: &[i32]| range.iter().any(|&s| s.abs() > SIGNAL_THRESHOLD);
    let underruns = sig_runs
        .iter()
        .filter(|run| {
            let left = &mono[run.start.saturating_sub(near)..run.start];
            let end = run.start + run.len;
            let right = &mono[end..(end + near).min(mono.len())];
            !left.is_empty() && !right.is_empty() && has_signal(left) && has_signal(right)
        })
        .map(|run| SilentRun {
            start: run.start,
            len: run.len,
        })
        .collect();

    // Jumps at the edges of counted silent runs are the underruns themselves,
    // not separate clicks.
    let mut run_edges = std::collections::HashSet::new();
    for run in &sig_runs {
        if run.start > 0 {
            run_edges.insert(run.start - 1);
        }
        run_edges.insert(run.start + run.len - 1);
        run_edges.insert(run.start + run.len);
    }
    let clicks = mono
        .windows(2)
        .enumerate()
        .filter(|(i, w)| {
            (w[1] - w[0]).abs() > CLICK_DELTA
                && !run_edges.contains(i)
                && !run_edges.contains(&(i + 1))
        })
        .map(|(i, w)| Click {
            index: i,
            from: w[0],
            to: w[1],
        })
        .collect();

    // The capture's leading and trailing silence are the longest silent runs
    // and are not underruns, so this measures the quantizer, not the glitch.
    let dither_ratio = sig_runs.iter().max_by_key(|r| r.len).map(|run| {
        let span = &mono[run.start..run.start + run.len];
        span.iter().filter(|&&s| s != 0).count() as f64 / span.len() as f64
    });

    Analysis {
        underruns,
        clicks,
        active_samples: mono.iter().filter(|s| s.abs() > SIGNAL_THRESHOLD).count(),
        peak: mono.iter().map(|s| s.abs()).max().unwrap_or(0),
        dither_ratio,
    }
}

/// The test tone's frequency, which the phase check below has to know and
/// `tests/toyos-rust-tests/src/tone.rs` states.
const TONE_HZ: f64 = 440.0;

/// Where the captured tone stops being one sine.
///
/// `specs/hda-driver-plan.md` §5.3 item 5 and risk 7: **a cyclic DMA engine
/// replays a period nobody refilled**, and a repeat is audible harm that
/// [`analyze`]'s gap detector cannot see — the samples are not silent and the
/// seam is not a large enough single-sample jump to be a click. The
/// zero-on-complete rule is what stops a repeat happening, and it is a design
/// promise; this is the measurement beside it.
///
/// A sampled sinusoid obeys `x[n+1] = 2·cos(ω)·x[n] − x[n−1]` exactly, so one
/// pass over the capture tests the whole signal for phase continuity with no
/// transform. A replayed 128-frame period is 1.28 cycles of 440 Hz, so the
/// tone re-enters 0.28 of a cycle out and breaks the recurrence by thousands
/// of LSB.
///
/// The tolerance covers what a correct capture does contain: TPDF dither at
/// ±1 LSB and quantization. Only the region between the first and last strong
/// sample is examined.
pub fn phase_breaks(wav: &Wav) -> Vec<usize> {
    const TOLERANCE: f64 = 400.0;
    let k = 2.0 * (2.0 * std::f64::consts::PI * TONE_HZ / wav.sample_rate as f64).cos();
    let mono = &wav.mono;
    let (Some(first), Some(last)) = (
        mono.iter().position(|&s| s.abs() > SIGNAL_THRESHOLD),
        mono.iter().rposition(|&s| s.abs() > SIGNAL_THRESHOLD),
    ) else {
        return Vec::new();
    };
    (first + 1..last)
        .filter(|&n| {
            let predicted = k * mono[n] as f64 - mono[n - 1] as f64;
            (predicted - mono[n + 1] as f64).abs() > TOLERANCE
        })
        .collect()
}

/// Underrun histogram keyed by gap length in device periods (rounded,
/// min 1): `gaps[n]` = number of mid-signal silent runs of ~n×2.902ms. This is
/// the unit the scheduler-core migration gates on (spec §11 gate A): stages
/// 1-6 must not regress the recorded baseline; Stage 7 requires zero.
pub fn gap_histogram(analysis: &Analysis, sample_rate: u32) -> BTreeMap<u32, u32> {
    let mut gaps = BTreeMap::new();
    for run in &analysis.underruns {
        let secs = run.len as f64 / sample_rate as f64;
        let n = (secs / PERIOD_SECS).round().max(1.0) as u32;
        *gaps.entry(n).or_insert(0u32) += 1;
    }
    gaps
}

/// Render a histogram as e.g. `total 3 [1p×2 4p×1]`, or `none`.
pub fn format_histogram(gaps: &BTreeMap<u32, u32>) -> String {
    if gaps.is_empty() {
        return "none".to_string();
    }
    let total: u32 = gaps.values().sum();
    let entries: Vec<String> = gaps.iter().map(|(n, c)| format!("{n}p×{c}")).collect();
    format!("total {total} [{}]", entries.join(" "))
}

/// No-regression gate against a recorded baseline histogram: neither the
/// total gap count nor the longest gap class may exceed the baseline. An
/// empty baseline is the strict zero-gap gate.
pub fn check_gap_regression(
    measured: &BTreeMap<u32, u32>,
    baseline: &BTreeMap<u32, u32>,
) -> Result<(), String> {
    let m_total: u32 = measured.values().sum();
    let b_total: u32 = baseline.values().sum();
    if m_total > b_total {
        return Err(format!(
            "underrun regression: {m_total} gaps vs baseline {b_total}"
        ));
    }
    let m_max = measured.keys().next_back().copied().unwrap_or(0);
    let b_max = baseline.keys().next_back().copied().unwrap_or(0);
    if m_max > b_max {
        return Err(format!(
            "underrun regression: longest gap {m_max} periods vs baseline {b_max}"
        ));
    }
    Ok(())
}

/// The DMA pipeline depth: `TX_INFLIGHT_MAX` = 8 buffers of one device period.
/// This is soundd's entire timing budget — wake later than this and every
/// buffer has already drained, so the device has run out of audio to play.
pub const PIPELINE_DEPTH_US: u64 = (8.0 * PERIOD_SECS * 1e6) as u64;

/// One `soundd: wakes=...` stats line. soundd emits one every 2s, but only
/// while it has clients, so every line describes streaming, not idle.
#[derive(Debug, Clone, Copy)]
pub struct SounddWindow {
    pub wakes: u32,
    pub completions: u32,
    pub submitted: u32,
    pub underruns: u32,
    pub drains: u32,
    pub max_wake_lat_us: u64,
    pub max_batch: u32,
    pub clients: u32,
}

/// Worst/total over every stats window of one run.
#[derive(Debug, Default, Clone, Copy)]
pub struct SounddCounters {
    pub windows: usize,
    /// Worst single-window wake lateness — the sharpest instrument here.
    pub max_wake_lat_us: u64,
    /// Cycles that found the whole DMA pipeline free (§5.9 recovery).
    pub drains: u32,
    /// Periods submitted with no client audio behind them: silence that
    /// actually went on the wire while a client was streaming.
    pub underruns: u32,
    pub submitted: u32,
    pub wakes: u32,
    pub max_batch: u32,
}

/// Kernel logging shares the virtio-console with userspace and is not
/// line-atomic, so a kernel message lands wherever it lands — including
/// mid-word inside soundd's stats line, which pushes that line's tail onto
/// the next serial line. A kernel message always runs from `[kernel ` to the
/// end of its line, so deleting exactly that span splices the interrupted
/// line back together and leaves standalone kernel lines simply removed.
fn strip_kernel_logging(serial: &str) -> String {
    let mut out = String::with_capacity(serial.len());
    let mut rest = serial;
    while let Some(start) = rest.find("[kernel ") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find('\n') {
            Some(nl) => &rest[start + nl + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

const STATS_MARKER: &str = "soundd: wakes=";
/// soundd's stats fields, in the order it prints them.
const STATS_KEYS: [&str; 8] = [
    "wakes",
    "completions",
    "submitted",
    "underruns",
    "drains",
    "max_wake_lat_us",
    "max_batch",
    "clients",
];

/// Read `key=<digits>` at or after `from`, tolerating a foreign line spliced
/// in between. Any writer sharing the console can land in the middle of the
/// value — the kernel is stripped beforehand, but the tone client's own
/// `println!` does it too — and such a write always ends at a newline, so
/// when the value is interrupted it resumes on the following line.
fn stats_field(window: &str, key: &str, from: usize) -> Option<(u64, usize)> {
    let pat = format!("{key}=");
    let mut at = from + window[from..].find(&pat)? + pat.len();
    loop {
        let digits: String = window[at..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return Some((digits.parse().ok()?, at));
        }
        at += window[at..].find('\n')? + 1;
    }
}

/// Pull soundd's stats windows out of a serial capture. An unreadable window
/// is an error rather than a skip: silently dropping one would under-count
/// `drains` and `underruns`, which is a gate passing because it failed to
/// look.
pub fn parse_soundd_counters(serial: &str) -> Result<SounddCounters, String> {
    let text = strip_kernel_logging(serial);
    // A window's fields can be split across lines, so it extends to the next
    // window marker rather than to the next newline.
    let starts: Vec<usize> = text.match_indices(STATS_MARKER).map(|(i, _)| i).collect();
    let mut out = SounddCounters::default();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(text.len());
        let window = &text[start..end];
        let mut vals = [0u64; STATS_KEYS.len()];
        let mut cursor = 0;
        for (i, key) in STATS_KEYS.iter().enumerate() {
            let (v, at) = stats_field(window, key, cursor).ok_or_else(|| {
                format!("unreadable soundd stats window (no {key}=<digits>): {window:?}")
            })?;
            vals[i] = v;
            cursor = at;
        }
        let w = SounddWindow {
            wakes: vals[0] as u32,
            completions: vals[1] as u32,
            submitted: vals[2] as u32,
            underruns: vals[3] as u32,
            drains: vals[4] as u32,
            max_wake_lat_us: vals[5],
            max_batch: vals[6] as u32,
            clients: vals[7] as u32,
        };
        out.windows += 1;
        out.max_wake_lat_us = out.max_wake_lat_us.max(w.max_wake_lat_us);
        out.max_batch = out.max_batch.max(w.max_batch);
        out.drains += w.drains;
        out.underruns += w.underruns;
        out.submitted += w.submitted;
        out.wakes += w.wakes;
    }
    Ok(out)
}

/// Per-config ceilings on soundd's counters. Every number is justified in
/// `tests/audio-baseline.toml`; there are no defaults, because an unjustified
/// threshold is the same problem as an unmeasured baseline.
#[derive(Debug, Clone, Copy)]
pub struct CounterLimits {
    pub max_wake_lat_us: u64,
    pub drains: u32,
    pub underruns: u32,
}

/// Which of this config's per-run ceilings this run sits outside, one message
/// each. Unlike the wav histogram — a rare-event detector that samples ~1000
/// periods once per run — these counters are non-zero on nearly every run, so
/// the *rate* at which they breach can resolve a change the histogram cannot.
///
/// A breach is not by itself audible: a pipeline that drained and recovered put
/// no silence on the wire. So this decides nothing on its own — the thorough
/// tier counts breaches and compares the rate, and the fast tier prints them
/// and judges harm.
pub fn check_counters(counters: &SounddCounters, limits: &CounterLimits) -> Vec<String> {
    let mut problems = Vec::new();
    if counters.max_wake_lat_us > limits.max_wake_lat_us {
        problems.push(format!(
            "wake lateness {}us > limit {}us ({:.1} vs {:.1} pipeline depths)",
            counters.max_wake_lat_us,
            limits.max_wake_lat_us,
            counters.max_wake_lat_us as f64 / PIPELINE_DEPTH_US as f64,
            limits.max_wake_lat_us as f64 / PIPELINE_DEPTH_US as f64,
        ));
    }
    if counters.drains > limits.drains {
        problems.push(format!(
            "pipeline drains {} > limit {}",
            counters.drains, limits.drains
        ));
    }
    if counters.underruns > limits.underruns {
        problems.push(format!(
            "client underruns {} > limit {} ({} periods submitted total)",
            counters.underruns, limits.underruns, counters.submitted
        ));
    }
    problems
}

/// Structural §5.8 suspend assertions, per-run and yes/no: the device stream
/// must start only for a client and must be stopped — with soundd suspended
/// and silent — once the last client is gone. TCG-immune, so a violation is
/// categorical, never a rare event to be averaged.
///
/// Positions are byte offsets in the RAW serial. That is sound because every
/// pattern below lands as one atomic chunk on the shared console: each pattern
/// sits inside a single format piece of its `eprintln!`, so one `write` syscall
/// carries it. Writers interleave BETWEEN chunks — whole foreign lines can land
/// inside a soundd line — but never inside these patterns, and chunk order is
/// emission order for soundd's mix thread, which emits every marker here. All
/// four are soundd's own since H3 moved the driver into it; the two stream
/// markers used to come from the kernel, from inside the submit syscall, and
/// the order they appear in is unchanged.
///
/// What this does NOT see is boot. The harness starts collecting at
/// ===TEST_START and never joins the reader thread's full log
/// (`tests/common/qemu.rs`), so every line soundd and the driver emit before
/// the first test are gone. A restored boot prime — the exact code deleted in
/// 465bc22 — would open the voice, play 8 periods, drain and suspend entirely
/// inside that discarded prefix, and the window would then show the identical
/// `connected → started → removed → stopped → suspended` sequence and pass
/// every assertion below. The §5.8 boot state is certified by nothing today,
/// in any test: `audio_idle_suspend` asserts no `stream 0 started` in its own
/// window, which catches a device started with no client attached (the
/// fill-loop gate at `soundd/src/main.rs` going away, or a spurious resume)
/// but not one started before the capture opened. Catching that needs the
/// boot capture the harness currently throws away.
pub fn check_suspend_structure(serial: &str) -> Vec<String> {
    const STARTED: &str = "virtio-sound: stream 0 started";
    const STOPPED: &str = "virtio-sound: stream 0 stopped";
    const CONNECTED: &str = " connected (id=";
    const SUSPENDED: &str = "soundd: suspended";

    let mut problems = Vec::new();

    let Some(first_connect) = serial.find(CONNECTED) else {
        problems.push("suspend structure: no client connect in capture".to_string());
        return problems;
    };
    match serial.find(STARTED) {
        None => problems.push(
            "suspend structure: the stream never started inside the test window — \
             either the device was already running at boot (the §5.8 boot state is \
             SUSPENDED) or the resume path is broken"
                .to_string(),
        ),
        Some(at) if at < first_connect => problems.push(
            "suspend structure: stream started before the first client connect".to_string(),
        ),
        Some(_) => {}
    }

    let Some(last_removed) = last_client_removed(serial) else {
        problems.push("suspend structure: no client removal in capture".to_string());
        return problems;
    };
    if !serial[last_removed..].contains(SUSPENDED) {
        problems.push(
            "suspend structure: no `soundd: suspended` after the last client removal"
                .to_string(),
        );
    }
    if !serial[last_removed..].contains(STOPPED) {
        problems.push(
            "suspend structure: no `virtio-sound: stream 0 stopped` after the last \
             client removal — the device is still running with no clients"
                .to_string(),
        );
    }
    problems
}

/// Offset of the last `soundd: client {id} removed`, the anchor the two
/// after-the-last-client assertions above are relative to.
///
/// ` removed` alone is an eight-character substring that any future line in
/// any component could carry; landing after soundd's markers it would move the
/// anchor past them and red all four configs at once, with a message accusing
/// soundd of the bug it does not have. Requiring the `soundd: client ` prefix
/// makes the anchor soundd's by construction rather than by a tree-wide
/// absence of other emitters.
///
/// The two halves are matched separately because they are separate console
/// writes: `eprintln!("soundd: client {} removed", id)` emits three format
/// pieces, so a whole foreign line can land between the prefix and the suffix
/// (see the module doc on interleaving). A ` removed` qualifies when some
/// `soundd: client ` precedes it with no other ` removed` in between — true
/// for soundd's own, false for a foreign line printed after it.
fn last_client_removed(serial: &str) -> Option<usize> {
    const CLIENT: &str = "soundd: client ";
    const REMOVED: &str = " removed";
    serial
        .match_indices(REMOVED)
        .filter(|(at, _)| {
            let before = &serial[..*at];
            before.rfind(CLIENT).is_some_and(|c| !before[c..].contains(REMOVED))
        })
        .map(|(at, _)| at)
        .last()
}

/// Bounds derived from the device's clock, not from any recorded run: values
/// on the wrong side of one did not happen, whatever the counter says.
///
/// `check_counters` asks whether a run got *worse*; this asks whether it
/// happened *at all*. A violation is reported as a **broken instrument**, never
/// as a regression: fatal in both tiers, and in the thorough tier it aborts the
/// run before the value can enter the sample or the re-baselining output. That
/// separation is what the thorough tier cannot provide for itself — it applies
/// no per-run ceiling, its Mann-Whitney test is rank-based so one absurd value
/// moves no median, and it prints its own sample as the next baseline.
///
/// The reference is the wall-clock life of the QEMU process, timed by the
/// harness. It is *outside* the guest, so no guest-side defect can inflate it
/// in step with the counter it bounds; the wav capture cannot serve, because
/// its timeline is the stream soundd submitted, so a stall that submits nothing
/// does not lengthen it. And it needs no recorded number, so there is nothing
/// to tune when a run goes red. The one assumption is that the guest and host
/// clocks agree to within a large factor — they are the same TSC up to
/// calibration error, and a calibration wrong by a percent would break the DLL
/// long before it reached these margins.
///
/// These bounds sit far above every per-run ceiling in
/// `tests/audio-baseline.toml` (2.07-8.01 pipeline depths), and they have to: a
/// ceiling admits values that are bad but real, so a bound firing anywhere near
/// one would be answering the regression question again. "A few pipeline
/// depths" is a health threshold, not a physical limit.
pub fn check_physical(counters: &SounddCounters, run_secs: f64) -> Vec<String> {
    let mut faults = Vec::new();

    // Lateness is the distance between two instants on the guest clock, both
    // inside the life of the soundd process, which is inside the life of the
    // QEMU process. A larger value does not fit, whatever it would mean.
    if counters.max_wake_lat_us as f64 > run_secs * 1e6 {
        faults.push(format!(
            "wake lateness {}us ({:.1} pipeline depths) exceeds the whole {run_secs:.2}s run \
             it was measured inside — the instrument is broken, not the scheduler",
            counters.max_wake_lat_us,
            counters.max_wake_lat_us as f64 / PIPELINE_DEPTH_US as f64,
        ));
    }

    // The device is a fixed-rate DAC: it retires exactly one period every
    // PERIOD_SECS and frees the DMA slot soundd then refills, so it cannot
    // have taken more periods in the run than the run had room for, plus the
    // pipeline still in flight at the end.
    let room = (run_secs / PERIOD_SECS) as u32 + 8;
    if counters.submitted > room {
        faults.push(format!(
            "{} periods submitted, but a {run_secs:.2}s run holds at most {room} \
             — the instrument is broken",
            counters.submitted
        ));
    }

    // Definitional: soundd counts an underrun on a subset of the periods it
    // counts as submitted, in the same branch. Violating it means the counter
    // or the parser is wrong.
    if counters.underruns > counters.submitted {
        faults.push(format!(
            "{} underruns out of {} periods submitted — underruns are a subset of \
             submitted, so one of the two counters is wrong",
            counters.underruns, counters.submitted
        ));
    }

    faults
}

fn silent_runs(mono: &[i32], min_len: usize) -> Vec<SilentRun> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &s) in mono.iter().enumerate() {
        match (s.abs() <= SILENCE_MAX, start) {
            (true, None) => start = Some(i),
            (false, Some(s0)) => {
                if i - s0 >= min_len {
                    runs.push(SilentRun {
                        start: s0,
                        len: i - s0,
                    });
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s0) = start {
        if mono.len() - s0 >= min_len {
            runs.push(SilentRun {
                start: s0,
                len: mono.len() - s0,
            });
        }
    }
    runs
}

/// Gate: on a machine with **no audio hardware** (`Profile::Metal`, the T14's
/// shape and the one the `tone` panic reproduced on), an audio-producing client
/// runs to completion — exit 0, no panic — and does so at the real audio rate,
/// neither instantly nor stalled.
///
/// This is the whole point of the null sink: hardware absence is a routing
/// state. Before it, soundd exited on a device-less machine and released its
/// service name, so cpal's `build_output_stream` failed `NotFound` and `tone`
/// panicked in `.expect("failed to build audio stream")`. That pre-null tree is
/// the negative control — reverting soundd's null-sink path reds this test with
/// the tone panic.
///
/// Three host-side assertions, and there is no wav here (this machine has no
/// device to capture from), so ground truth is the client's exit, the host wall
/// clock around it, and soundd's own counters:
///
/// 1. **No crash.** The client exits 0.
/// 2. **Real rate.** A 3 s tone takes ~3 s of wall clock. Instant discard would
///    finish in a fraction of a second (the client fills its ring and races to
///    the end); a stalled sink would time the run out. soundd's `submitted`
///    counter — periods it drained — is the in-guest cross-check: ~3 s / 2.9 ms
///    ≈ 1034.
/// 3. **Not silent about being silenced.** soundd reports the discarded stream
///    in its stats windows (clients ≥ 1), the same accounting a real sink emits
///    and what #106's status tool will read.
pub fn null_sink_real_rate(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            profile: qemu::Profile::Metal,
            ..Default::default()
        },
    );

    // soundd must present the null sink rather than exit. It starts before the
    // first test, so the line is in the boot log; drain a little more in case it
    // races the ready marker.
    const NULL_LINE: &str = "soundd: no audio device, presenting a null sink";
    let mut early = qemu.boot_log().to_string();
    if !early.contains(NULL_LINE) {
        early.push_str(&qemu.drain_serial(Duration::from_millis(500)));
    }
    if !early.contains(NULL_LINE) {
        return Err(format!(
            "soundd did not present a null sink on a device-less machine:\n{early}"
        ));
    }

    // The tone is DURATION_SECS = 3.0 s of samples (tests/toyos-rust-tests/
    // src/tone.rs). At the real audio rate it cannot finish before then.
    let start = Instant::now();
    let result = qemu.run_test("test_rs_audio_tone", Duration::from_secs(30));
    let elapsed = start.elapsed().as_secs_f64();

    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    match result.exit_code {
        Some(0) => {}
        Some(code) => {
            return Err(format!(
                "tone exited {code} on a device-less machine (a no-device machine must \
                 still play to completion):\n{}",
                result.stdout
            ))
        }
        None => return Err(format!("tone produced no exit code:\n{}", result.stdout)),
    }

    // Real rate, host-measured. The 3 s tone plus its ~0.2 s drain tail and a
    // little spawn overhead lands near 3.3 s; the bounds catch the two failure
    // shapes the null sink exists to avoid — instant discard below, a sink
    // draining slower than the audio rate above.
    const MIN_SECS: f64 = 2.5;
    const MAX_SECS: f64 = 8.0;
    if !(MIN_SECS..=MAX_SECS).contains(&elapsed) {
        return Err(format!(
            "null sink drained a 3 s tone in {elapsed:.2} s (expected {MIN_SECS}..={MAX_SECS} s): \
             a client that writes N seconds of audio must take ~N seconds\nstdout:\n{}",
            result.stdout
        ));
    }

    // soundd's own accounting of the discarded stream: a real-rate cross-check
    // and the proof it is not silent about the silencing. The final window
    // races the client's exit, so collect a little more serial first.
    let serial = result.serial.clone() + &qemu.drain_serial(Duration::from_millis(500));
    let counters = parse_soundd_counters(&serial)?;
    if counters.windows == 0 {
        return Err(format!(
            "soundd reported no stats window with a client — the tone never reached the null sink:\n{serial}"
        ));
    }
    // ~3 s / 2.902 ms ≈ 1034 periods, plus the disconnect ramp. Wide enough to
    // absorb the window boundaries, tight enough that instant discard (a handful
    // of periods) or a half-rate drain (~520) both fail.
    const MIN_SUBMITTED: u32 = 700;
    const MAX_SUBMITTED: u32 = 1500;
    if !(MIN_SUBMITTED..=MAX_SUBMITTED).contains(&counters.submitted) {
        return Err(format!(
            "null sink submitted {} periods for a 3 s tone (expected {MIN_SUBMITTED}..={MAX_SUBMITTED}): \
             the drain rate is not the audio rate\nstdout:\n{}",
            counters.submitted, result.stdout
        ));
    }

    eprintln!(
        "  [metal-sim] null sink drained a 3 s tone in {elapsed:.2} s, {} periods, \
         {} stats window(s) — real rate, no device",
        counters.submitted, counters.windows
    );
    Ok(())
}

/// Gate: doom's sound producer outruns its audio callback and the game lives.
///
/// The T14 report this exists for: about five seconds into playing doom the
/// machine froze, and the first thing to go was doom itself —
/// `sound command ring overflow: audio callback stalled` inside
/// `I_UpdateSoundParams`, an `extern "C"` frame with no unwind path, so the
/// panic became `abort`. The kernel then panicked retiring the task and the
/// compositor died granting shared memory to a pid that was no longer there.
/// This is the first domino.
///
/// Nothing in the flood is timed. The actuator parks the audio callback with
/// `cpal::Stream::pause` and requires the callback's own period counter to
/// stand still across the burst, so "the producer outran the consumer" is a
/// fact about the two of them rather than about how busy this host was — a
/// distinction the harness owes the suspended-laptop case, where a wall-clock
/// burst is satisfied by stopping both sides at once.
///
/// Four assertions, three of them in-guest facts the host reads back and one
/// on the wire:
///
/// 1. **The game lives.** `/bin/doom --sound-stress` exits 0. On the tree this
///    replaced the same burst aborts on its 65th command.
/// 2. **The burst was real.** `stalled_burst` commands were issued with the
///    callback's period count unchanged — 4096 of them, 64x the retired ring.
/// 3. **The callback converged, and did so at the audio rate.** The sound the
///    last command started plays to completion, and the periods that took
///    match its length: a mixer that lost the command never finishes, and one
///    that restarted it takes longer.
/// 4. **The last command is what reached the device.** Every superseded update
///    in the burst carries `QUIET_VOLUME`, which mixes to 251 LSB — under
///    `SIGNAL_THRESHOLD`, so it is not signal. The final one carries full
///    volume, 16000 * 127/255 = 7968. A capture with signal in it is a capture
///    in which the last write won.
pub fn doom_sound_flood(rust_bins: &[(String, Vec<u8>)]) -> Result<(), String> {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/doomcase");
    let mut qemu = QemuInstance::boot_with_options(&config, &[], rust_bins, BootOptions::default());

    let result = qemu.run_test("test_rs_doom_sound_flood", Duration::from_secs(60));
    if let Some(err) = &result.error {
        return Err(format!("{err}\n{}", result.stdout));
    }
    if result.exit_code != Some(0) {
        return Err(format!(
            "doom did not survive its own sound producer (exit {:?}):\n{}",
            result.exit_code, result.stdout
        ));
    }

    let counters = parse_stress_line(&result.stdout)?;

    // The retired ring held 64 commands and asserted on the 65th.
    const RETIRED_RING_CAP: u64 = 64;
    if counters.stalled_burst <= RETIRED_RING_CAP {
        return Err(format!(
            "the flood was {} commands against a callback that had stopped, which the \
             retired 64-entry ring would have swallowed — the actuator proved nothing",
            counters.stalled_burst
        ));
    }

    // A period is 128 frames, so a sound of N frames occupies ceil(N/128) of
    // them. The ceiling is loose on purpose: it is a liveness bound on the game
    // thread noticing the sound ended, and the verdict is the floor — a mixer
    // that restarted or skipped the sound cannot land on its exact length.
    check_playback("tone", counters.tone_periods, counters.tone_frames)?;
    check_playback("probe", counters.probe_periods, counters.probe_frames)?;

    // Let the tail of the capture reach the file before reading it.
    let _ = qemu.drain_serial(Duration::from_millis(500));
    let wav = parse_wav(qemu.audio_wav_path())?;
    let analysis = analyze(&wav);

    // 16000 * 127/255 = 7968 at full volume against 251 for every superseded
    // update, so the band excludes the second outcome by a factor of eight and
    // is wide enough that soundd's mix path is not being measured here.
    const MIN_PEAK: i32 = 4000;
    const MAX_PEAK: i32 = 12000;
    if !(MIN_PEAK..=MAX_PEAK).contains(&analysis.peak) {
        return Err(format!(
            "the device played a peak of {} (expected {MIN_PEAK}..={MAX_PEAK}): the volume \
             the last command named is not the volume that reached the wire",
            analysis.peak
        ));
    }
    // The tone is TONE_FRAMES long and 96% of a sine at this amplitude clears
    // SIGNAL_THRESHOLD; a third of it is a floor no partial application meets.
    let min_active = counters.tone_frames as usize / 3;
    if analysis.active_samples < min_active {
        return Err(format!(
            "only {} samples of signal reached the device for a {}-frame tone (expected \
             at least {min_active})",
            analysis.active_samples, counters.tone_frames
        ));
    }

    eprintln!(
        "  [doomcase] {} commands issued with the callback parked, tone converged in {} \
         periods for {} frames, {} concurrent commands, {} samples of signal at peak {}",
        counters.stalled_burst,
        counters.tone_periods,
        counters.tone_frames,
        counters.concurrent_cmds,
        analysis.active_samples,
        analysis.peak,
    );
    Ok(())
}

struct StressCounters {
    stalled_burst: u64,
    tone_periods: u64,
    tone_frames: u64,
    concurrent_cmds: u64,
    probe_periods: u64,
    probe_frames: u64,
}

fn parse_stress_line(stdout: &str) -> Result<StressCounters, String> {
    let line = stdout
        .lines()
        .find(|l| l.contains("[sound-stress] stalled_burst="))
        .ok_or_else(|| format!("doom printed no [sound-stress] line:\n{stdout}"))?;
    let field = |name: &str| -> Result<u64, String> {
        let prefix = format!("{name}=");
        line.split_whitespace()
            .find_map(|tok| tok.strip_prefix(&prefix)?.parse().ok())
            .ok_or_else(|| format!("no {name} in {line:?}"))
    };
    Ok(StressCounters {
        stalled_burst: field("stalled_burst")?,
        tone_periods: field("tone_periods")?,
        tone_frames: field("tone_frames")?,
        concurrent_cmds: field("concurrent_cmds")?,
        probe_periods: field("probe_periods")?,
        probe_frames: field("probe_frames")?,
    })
}

fn check_playback(what: &str, periods: u64, frames: u64) -> Result<(), String> {
    const PERIOD_FRAMES: u64 = 128;
    let exact = frames.div_ceil(PERIOD_FRAMES);
    if !(exact..=exact * 4).contains(&periods) {
        return Err(format!(
            "the {what} took {periods} periods to play {frames} frames (expected \
             {exact}..={}): the mixer did not apply the last command as written",
            exact * 4
        ));
    }
    Ok(())
}

/// The shipped audio client must finish and exit on a machine with no device.
///
/// `metal_sim_null_audio` already asserts a client drains at the real rate, and
/// it passed on every boot while the T14 hung — because it runs this crate's
/// own tone, which reaches soundd through the SDK, and the program a user runs
/// reaches it through `cpal`. Same sink, same period grid, different client.
///
/// Two clients in series, because the T14 log shows the *second* connect never
/// being applied: the control thread accepts and prints `opening stream`, and
/// no `client N connected` follows it.
pub fn null_sink_shipped_client(
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions { profile: qemu::Profile::Metal, ..Default::default() },
    );

    let result = qemu.run_test("test_rs_null_sink_client_exits", Duration::from_secs(60));
    if let Some(err) = &result.error {
        return Err(format!("{err}\nstdout:\n{}\nserial:\n{}", result.stdout, result.serial));
    }
    match result.exit_code {
        Some(0) => {}
        Some(code) => {
            return Err(format!(
                "the shipped tone exited {code} on a device-less machine:\n{}",
                result.stdout
            ))
        }
        None => return Err(format!("no exit code:\n{}", result.stdout)),
    }
    eprintln!("  [null-sink] {}", result.stdout.trim());
    Ok(())
}
