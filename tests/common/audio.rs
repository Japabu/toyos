//! Wav capture parsing and glitch analysis for the audio integration tests.
//!
//! The QEMU wav audiodev records a continuous timeline of what the
//! virtio-sound device played. Underruns show up as stretches of exact
//! digital silence inside an otherwise active signal; clicks show up as
//! sample-to-sample jumps no band-limited signal could produce.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Zero runs shorter than this are ignored (a mixed-to-zero sample pair can
/// occur legitimately at low amplitudes).
const MIN_GAP_SECS: f64 = 0.002;
/// A zero run only counts as an underrun if there is signal within this
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

pub struct ZeroRun {
    pub start: usize,
    pub len: usize,
}

pub struct Click {
    pub index: usize,
    pub from: i32,
    pub to: i32,
}

pub struct Analysis {
    /// Mid-signal zero runs >= MIN_GAP_SECS: underruns.
    pub underruns: Vec<ZeroRun>,
    /// Hard discontinuities not at the edges of counted zero runs.
    pub clicks: Vec<Click>,
    /// Samples with amplitude above SIGNAL_THRESHOLD.
    pub active_samples: usize,
    pub peak: i32,
}

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

    let sig_runs = zero_runs(mono, min_gap);

    let has_signal = |range: &[i32]| range.iter().any(|&s| s.abs() > SIGNAL_THRESHOLD);
    let underruns = sig_runs
        .iter()
        .filter(|run| {
            let left = &mono[run.start.saturating_sub(near)..run.start];
            let end = run.start + run.len;
            let right = &mono[end..(end + near).min(mono.len())];
            !left.is_empty() && !right.is_empty() && has_signal(left) && has_signal(right)
        })
        .map(|run| ZeroRun {
            start: run.start,
            len: run.len,
        })
        .collect();

    // Jumps at the edges of counted zero runs are the underruns themselves,
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

    Analysis {
        underruns,
        clicks,
        active_samples: mono.iter().filter(|s| s.abs() > SIGNAL_THRESHOLD).count(),
        peak: mono.iter().map(|s| s.abs()).max().unwrap_or(0),
    }
}

/// Underrun histogram keyed by gap length in device periods (rounded,
/// min 1): `gaps[n]` = number of mid-signal zero runs of ~n×2.902ms. This is
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

fn zero_runs(mono: &[i32], min_len: usize) -> Vec<ZeroRun> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &s) in mono.iter().enumerate() {
        match (s == 0, start) {
            (true, None) => start = Some(i),
            (false, Some(s0)) => {
                if i - s0 >= min_len {
                    runs.push(ZeroRun {
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
            runs.push(ZeroRun {
                start: s0,
                len: mono.len() - s0,
            });
        }
    }
    runs
}
