//! Wav capture parsing and glitch analysis for the audio integration tests.
//!
//! The QEMU wav audiodev records a continuous timeline of what the
//! virtio-sound device played. Underruns show up as stretches of digital
//! silence inside an otherwise active signal; clicks show up as
//! sample-to-sample jumps no band-limited signal could produce.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

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
/// = 2.902ms (kernel/src/drivers/virtio_sound.rs PERIOD_BYTES). Underruns
/// are period-quantized — the device plays silence one period at a time —
/// so gap lengths are reported in whole periods.
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

// ---------------------------------------------------------------------------
// soundd's own counters
// ---------------------------------------------------------------------------

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

/// Pull soundd's stats lines out of a serial capture. Fields are located by
/// name rather than by position, and an unreadable line is an error rather
/// than a skip: a silently dropped window would under-count `drains` and
/// `underruns`, which is a gate that passes because it failed to look.
pub fn parse_soundd_counters(serial: &str) -> Result<SounddCounters, String> {
    let field = |line: &str, key: &str| -> Option<u64> {
        let rest = line.split(&format!("{key}=")).nth(1)?;
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    };
    let mut out = SounddCounters::default();
    for line in strip_kernel_logging(serial).lines() {
        if !line.contains("soundd: wakes=") {
            continue;
        }
        let get = |key: &str| {
            field(line, key)
                .ok_or_else(|| format!("unreadable soundd stats line (no {key}=): {line}"))
        };
        let w = SounddWindow {
            wakes: get("wakes")? as u32,
            completions: get("completions")? as u32,
            submitted: get("submitted")? as u32,
            underruns: get("underruns")? as u32,
            drains: get("drains")? as u32,
            max_wake_lat_us: get("max_wake_lat_us")?,
            max_batch: get("max_batch")? as u32,
            clients: get("clients")? as u32,
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

/// Gate on soundd's in-guest accounting. Unlike the wav histogram — a
/// rare-event detector that samples ~1000 periods once per run — these
/// counters are non-zero on nearly every run, so they can actually resolve a
/// change in the failure rate.
pub fn check_counters(
    counters: &SounddCounters,
    limits: &CounterLimits,
) -> Result<(), String> {
    if counters.windows == 0 {
        return Err(
            "soundd printed no stats window with clients — the tone never reached the mixer"
                .to_string(),
        );
    }
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
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!("soundd counter regression: {}", problems.join("; ")))
    }
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
