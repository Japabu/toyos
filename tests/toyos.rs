mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;

use common::qemu::{self, BootOptions, QemuInstance, TestResult};
use common::{audio, compile, screen, stats};

struct TestDef {
    name: String,
    qemu_name: String,
    timeout: Duration,
    check: fn(&TestResult) -> bool,
}

// Rust helper binaries that are spawned by tests, not tests themselves.
const RUST_SKIP: &[&str] = &["segfault_child", "test_panic_child"];

// Audio glitch tests. Each runs in its own QEMU boot per SMP config and
// asserts on the wav the virtio-sound device captured, so they are excluded
// from the shared multi-test boot.
const AUDIO_TESTS: &[&str] = &["audio_tone", "audio_tone_load"];

// Scheduler-core gate A covers both SMP configs: smp=1 is the audio spec's
// first-class single-CPU case, smp=8 the full-SMP case.
const AUDIO_SMP: &[u32] = &[1, 8];

// On-screen panic console. Each boots its own QEMU with a UEFI GOP display
// and reads a decoded screendump, so they cannot share the multi-test boot —
// the guest they test is halted by the time they assert. `screen_decoder`
// needs no guest at all; it proves the decoder against a bitmap it rendered
// itself, before anything points it at a real screen.
const SCREEN_TESTS: &[&str] = &["screen_decoder", "screen_early_panic"];

// C tests that can't compile yet (missing toyos-cc features or unsupported platform APIs).
// Tests that compile successfully are discovered automatically — only list failures here.
const C_SKIP: &[&str] = &[
    "03_struct",              // needs _Generic
    "18_include",             // needs system headers we don't provide
    "31_args",                // needs argc/argv
    "32_led",                 // needs system APIs
    "33_ternary_op",          // needs _Generic
    "40_stdio",               // needs FILE* APIs
    "46_grep",                // needs argc/argv + FILE*
    "60_errors_and_warnings", // meta-test for compiler errors
    "73_arm64",               // wrong architecture
    "101_cleanup",            // needs __attribute__((cleanup))
    "102_alignas",            // needs _Alignas
    "103_implicit_memmove",   // needs __builtin_memmove
    "104_inline",             // needs weak symbols in linker
    "106_versym",             // needs pthread
    "107_stack_safe",         // needs alloca
    "108_constructor",        // needs __attribute__((constructor))
    "109_float_struct_calling", // needs struct-in-register calling convention
    "112_backtrace",          // needs tcc_backtrace
    "113_btdll",              // needs tcc_backtrace
    "114_bound_signal",       // needs sigaction
    "115_bound_setjmp",       // needs setjmp
    "116_bound_setjmp2",      // needs setjmp
    "117_builtins",           // needs __builtin_memmove
    "120_alias",              // needs asm aliases
    "122_vla_reuse",          // VLA codegen bug
    "123_vla_bug",            // VLA codegen bug
    "124_atomic_counter",     // needs stdatomic.h (calls process::exit, not catchable)
    "125_atomic_misc",        // needs stdatomic.h (calls process::exit, not catchable)
    "126_bound_global",       // needs bounds checking
    "127_asm_goto",           // needs inline asm
    "128_run_atexit",         // needs atexit
    "132_bound_test",         // needs bounds checking
    "136_atomic_gcc_style",   // needs stdatomic.h (calls process::exit, not catchable)
];

/// Discover C tests by scanning tests/testcases/tinycc/*.c.
/// Skips companion files (contain '+') and tests in C_SKIP.
fn discover_c_tests() -> Vec<String> {
    let dir = compile::testcases_dir();
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let name = e.ok()?.file_name().to_str()?.to_string();
            let stem = name.strip_suffix(".c")?;
            if stem.contains('+') {
                return None;
            }
            if C_SKIP.contains(&stem) {
                return None;
            }
            Some(stem.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Discover Rust test binaries from build output.
/// Skips shared libraries, helper binaries, and audio tests (dedicated boot).
fn discover_rust_tests(bins: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut names: Vec<String> = bins
        .iter()
        .filter_map(|(name, _)| {
            if name.ends_with(".so") {
                return None;
            }
            if RUST_SKIP.contains(&name.as_str()) || AUDIO_TESTS.contains(&name.as_str()) {
                return None;
            }
            Some(name.clone())
        })
        .collect();
    names.sort();
    names
}

fn compile_c_tests(names: &[String]) -> Vec<(String, Vec<u8>)> {
    // Suppress panic messages during compilation — we handle failures via catch_unwind.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut bins = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        match std::panic::catch_unwind(|| {
            let (obj, extras) = compile::compile_c(name);
            compile::link_toyos(&obj, &extras, name)
        }) {
            Ok(linked) => bins.push((name.clone(), linked)),
            Err(_) => skipped.push(name.as_str()),
        }
    }

    std::panic::set_hook(prev_hook);

    if !skipped.is_empty() {
        eprintln!(
            "[toyos] {} C tests skipped (compilation failed): {}",
            skipped.len(),
            skipped.join(", ")
        );
    }

    bins
}

fn check_c_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_c_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL c::{test_name}: {err}");
        return false;
    }

    match result.exit_code {
        Some(0) => {
            let expect_file = compile::testcases_dir().join(format!("{test_name}.expect"));
            if expect_file.exists() {
                let expected = fs::read_to_string(&expect_file).unwrap();
                if result.stdout.trim_end() != expected.trim_end() {
                    eprintln!("FAIL c::{test_name}: output mismatch");
                    eprintln!("--- expected ---\n{}", expected.trim_end());
                    eprintln!("--- actual ---\n{}", result.stdout.trim_end());
                    return false;
                }
            }
            true
        }
        Some(code) => {
            eprintln!("FAIL c::{test_name}: exit code {code}\nstdout: {}", result.stdout);
            false
        }
        None => {
            eprintln!("FAIL c::{test_name}: no exit code");
            false
        }
    }
}

fn check_rust_result(result: &TestResult) -> bool {
    let test_name = result.name.strip_prefix("test_rs_").unwrap_or(&result.name);

    if let Some(err) = &result.error {
        eprintln!("FAIL rs::{test_name}: {err}");
        return false;
    }

    match result.exit_code {
        Some(0) => true,
        Some(code) => {
            eprintln!("FAIL rs::{test_name}: exit code {code}\nstdout:\n{}", result.stdout);
            false
        }
        None => {
            eprintln!("FAIL rs::{test_name}: no exit code\nstdout:\n{}", result.stdout);
            false
        }
    }
}

/// Checks both exit code and serial diagnostics for panic recovery.
fn check_panic_recovery(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }

    let checks: &[(&str, &str)] = &[
        ("!!! PANIC !!!", "expected PANIC header"),
        ("SYS_DEBUG", "expected SYS_DEBUG in panic message"),
        ("Syscall: num=92", "expected syscall context in panic report"),
        ("User backtrace:", "expected user backtrace in panic report"),
        ("Registers:", "expected register dump from kernel fault"),
        ("scheduler entered while a lock is held", "expected the §6.4 lock-across-switch tripwire to fire"),
        ("arch/syscall.rs", "expected the tripwire to name the guilty call site, not scheduler.rs"),
        ("SEGFAULT tid=", "expected SEGFAULT header"),
        ("deliberate_null_deref", "expected deliberate_null_deref in segfault backtrace"),
        ("+0x", "expected symbolized backtraces"),
    ];

    let mut ok = true;
    for (needle, msg) in checks {
        if !result.serial.contains(needle) {
            eprintln!("FAIL rs::panic_recovery: {msg}\nserial:\n{}", result.serial);
            ok = false;
        }
    }
    ok
}

/// A zero CPU delta is the signature of a suspended soundd and equally of one
/// wedged with the device running, so the counter the test reads cannot tell
/// them apart on its own. The serial can: in a window where no audio client
/// ever connects, the PCM stream has no business starting.
///
/// This is bounded by what the harness captures — collection begins at
/// ===TEST_START, so a device started before then (a restored boot prime) is
/// invisible here as it is everywhere else; see `audio::check_suspend_structure`.
/// What it does catch is a start inside the window with no client to justify
/// it: soundd's `!streams.is_empty()` fill-loop gate going away, or a resume
/// fired by anything other than a connect.
fn check_audio_idle_suspend(result: &TestResult) -> bool {
    if !check_rust_result(result) {
        return false;
    }
    const STARTED: &str = "virtio-sound: stream 0 started";
    if result.serial.contains(STARTED) {
        eprintln!(
            "FAIL rs::audio_idle_suspend: `{STARTED}` with no client connected — \
             soundd's zero CPU is the device left running, not a suspend\nserial:\n{}",
            result.serial
        );
        return false;
    }
    true
}

/// Select check function by test name convention.
fn check_for(name: &str) -> fn(&TestResult) -> bool {
    match name {
        "panic_recovery" => check_panic_recovery,
        "audio_idle_suspend" => check_audio_idle_suspend,
        _ => check_rust_result,
    }
}

/// Minimum active (non-silent) playback the 3s test tone must produce.
/// Guards against a vacuous pass when nothing plays at all.
const TONE_MIN_ACTIVE_SECS: f64 = 2.5;
/// The tone is generated at amplitude 16000; a far lower peak proves the
/// signal path is broken even if technically "active".
const TONE_MIN_PEAK: i32 = 4000;

/// Recorded per-(test, smp) baselines — the scheduler-core migration's gate A
/// (specs/scheduler-core-spec.md §11). Two independent instruments per config:
/// the wav underrun histogram (`gaps`, keyed by gap length in device periods)
/// and ceilings on soundd's own counters. The wav is a rare-event detector;
/// the counters fire on nearly every run and carry the statistical power. Both
/// must hold. Re-record deliberately, never casually — and justify every
/// number in `tests/audio-baseline.toml` itself.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioBaselineEntry {
    #[serde(default)]
    gaps: BTreeMap<String, u32>,
    max_wake_lat_us: u64,
    drains: u32,
    underruns: u32,
    sample: BaselineSample,
}

/// The recorded clean-tree *sample* for one config, not a summary of it. The
/// thorough tier compares a fresh sample against this one, so it needs the
/// observations themselves — see `tests/common/stats.rs` for why a summary
/// would understate the false-red rate.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineSample {
    /// Runs whose wav was analysed (the counter arrays can be longer: a run
    /// can lose its histogram and still report counters).
    gap_sample: u32,
    /// Of `gap_sample`, how many showed at least one mid-tone dropout.
    gap_runs: u32,
    /// Of the counter runs, how many breached this config's per-run ceilings.
    ceiling_runs: u32,
    max_wake_lat_us: Vec<f64>,
    underruns: Vec<f64>,
    wakes: Vec<f64>,
    /// Recorded for re-baselining the per-run ceiling only. Deliberately not
    /// tested distributionally: it is zero on 50-90% of runs, and the ties
    /// leave a rank test with no power (measured: 0.00-0.21 against a tripling).
    drains: Vec<f64>,
}

type AudioBaseline = BTreeMap<String, BTreeMap<String, AudioBaselineEntry>>;

struct ConfigBaseline<'a> {
    gaps: BTreeMap<u32, u32>,
    counters: audio::CounterLimits,
    sample: &'a BaselineSample,
}

fn load_audio_baseline() -> AudioBaseline {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio-baseline.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Baseline for one (test, smp) config. Every config must be recorded: an
/// ungated config would pass by omission.
fn config_baseline<'a>(baseline: &'a AudioBaseline, name: &str, smp: u32) -> ConfigBaseline<'a> {
    let entry = baseline
        .get(name)
        .and_then(|per_smp| per_smp.get(&format!("smp{smp}")))
        .unwrap_or_else(|| panic!("audio-baseline.toml: no [{name}.smp{smp}] section"));
    ConfigBaseline {
        sample: &entry.sample,
        gaps: entry
            .gaps
            .iter()
            .map(|(k, &count)| {
                let periods: u32 = k.parse().unwrap_or_else(|_| {
                    panic!("audio-baseline.toml: bad gap key {k:?} for {name} smp{smp}")
                });
                (periods, count)
            })
            .collect(),
        counters: audio::CounterLimits {
            max_wake_lat_us: entry.max_wake_lat_us,
            drains: entry.drains,
            underruns: entry.underruns,
        },
    }
}

/// What one audio boot measured. Both tiers are computed from this; they
/// differ only in how many they collect and what decision they take on the
/// collection.
struct AudioRun {
    gaps: BTreeMap<u32, u32>,
    counters: audio::SounddCounters,
    /// The instrument itself is untrustworthy on this run (no tone, no dither,
    /// clicks). Never a rare-event judgement — always fatal, in both tiers.
    broken: Vec<String>,
    /// soundd counters past this config's per-run ceilings. Fatal in the fast
    /// tier; a counted rate in the thorough tier.
    breaches: Vec<String>,
}

impl AudioRun {
    fn dropped_audio(&self) -> bool {
        !self.gaps.is_empty()
    }
}

/// Boot a fresh QEMU with the given CPU count, run one in-guest audio test,
/// and measure it: soundd's in-guest counters (wake lateness, pipeline drains,
/// periods of silence submitted) and the captured wav (mid-signal silence, hard
/// sample-to-sample discontinuities, and the dither the detector needs to see
/// anything at all).
///
/// `Err` means the run produced no measurement — a boot failure, a timeout, an
/// unreadable capture. That is never a rare-event judgement call; it is fatal
/// in both tiers.
fn measure_audio_run(
    name: &str,
    smp: u32,
    baseline: &ConfigBaseline,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
    // Distinguishes this boot from the others of the same config in the log
    // and in the kept capture's filename; empty for a plain single boot.
    tag: &str,
) -> Result<AudioRun, String> {
    let label = if tag.is_empty() {
        String::new()
    } else {
        format!("{tag}: ")
    };
    // Bounds every duration soundd can report: its whole life is inside this
    // process's. See `audio::check_physical`.
    let run_start = std::time::Instant::now();
    let mut qemu = QemuInstance::boot_with_options(
        test_config,
        c_bins,
        rust_bins,
        BootOptions {
            smp,
            ..Default::default()
        },
    );

    let result = qemu.run_test(&format!("test_rs_{name}"), Duration::from_secs(30));
    if let Some(err) = &result.error {
        return Err(err.clone());
    }
    match result.exit_code {
        Some(0) => {}
        Some(code) => return Err(format!("exit code {code}\nstdout:\n{}", result.stdout)),
        None => return Err(format!("no exit code\nstdout:\n{}", result.stdout)),
    }

    // The wav timeline advances in real time; give the tone tail and its
    // trailing silence context time to reach the file before reading it. The
    // same wait collects soundd's final stats flush, which races the client's
    // exit and so can arrive after ===TEST_END===.
    let serial = result.serial + &qemu.drain_serial(Duration::from_millis(500));

    let wav = audio::parse_wav(qemu.audio_wav_path())?;
    let analysis = audio::analyze(&wav);
    let rate = wav.sample_rate as f64;
    let secs = |samples: usize| samples as f64 / rate;

    // Always printed, so every run leaves comparable numbers in the log.
    let gaps = audio::gap_histogram(&analysis, wav.sample_rate);
    let counters = audio::parse_soundd_counters(&serial)?;
    eprintln!(
        "        {label}{name} smp={smp} gaps: {} (baseline {}) peak {} active {:.2}s dither {:.1}%",
        audio::format_histogram(&gaps),
        audio::format_histogram(&baseline.gaps),
        analysis.peak,
        secs(analysis.active_samples),
        analysis.dither_ratio.unwrap_or(0.0) * 100.0,
    );
    eprintln!(
        "        {label}{name} smp={smp} soundd: wake_lat {}us ({:.2} pipelines, limit {}us) \
         drains {}/{} underruns {}/{} submitted {} wakes {} batch {} windows {}",
        counters.max_wake_lat_us,
        counters.max_wake_lat_us as f64 / audio::PIPELINE_DEPTH_US as f64,
        baseline.counters.max_wake_lat_us,
        counters.drains,
        baseline.counters.drains,
        counters.underruns,
        baseline.counters.underruns,
        counters.submitted,
        counters.wakes,
        counters.max_batch,
        counters.windows,
    );

    let mut breaches = Vec::new();
    if let Err(regression) = audio::check_counters(&counters, &baseline.counters) {
        breaches.push(regression);
    }

    // A counter past a physical bound is the instrument failing, so it belongs
    // here with the other instrument checks rather than among the ceilings: it
    // must fail loudly in both tiers, and it must never be ranked against the
    // recorded sample or printed into the next baseline.
    let mut problems = audio::check_physical(&counters, run_start.elapsed().as_secs_f64());
    if secs(analysis.active_samples) < TONE_MIN_ACTIVE_SECS {
        problems.push(format!(
            "tone missing: only {:.2}s of active signal (expected >= {TONE_MIN_ACTIVE_SECS}s)",
            secs(analysis.active_samples)
        ));
    }
    if analysis.peak < TONE_MIN_PEAK {
        problems.push(format!(
            "tone too quiet: peak {} (expected >= {TONE_MIN_PEAK})",
            analysis.peak
        ));
    }
    // Without this the gate can go green while measuring nothing: the underrun
    // detector's silence band is derived from soundd applying TPDF dither into
    // a rounding quantizer (spec §5.4). Lose the dither and silence becomes
    // exact zero everywhere, the band collapses, and dropouts stop being
    // visible — the exact failure this instrument was rebuilt to remove.
    match analysis.dither_ratio {
        Some(ratio) if ratio < audio::MIN_DITHER_RATIO => problems.push(format!(
            "dither missing: only {:.1}% of silent samples are non-zero (expected ~25%, \
             floor {:.0}%) — soundd is not dithering, so the underrun detector is blind",
            ratio * 100.0,
            audio::MIN_DITHER_RATIO * 100.0
        )),
        Some(_) => {}
        None => problems.push("no silent stretch in capture to verify dither against".to_string()),
    }
    if audio::check_gap_regression(&gaps, &baseline.gaps).is_err() {
        let mut msg = format!(
            "{} mid-signal underruns (silence >= 2ms inside the tone):",
            analysis.underruns.len()
        );
        for run in analysis.underruns.iter().take(20) {
            msg.push_str(&format!(
                "\n      at {:8.3}s len {:6.2}ms",
                secs(run.start),
                secs(run.len) * 1000.0
            ));
        }
        if analysis.underruns.len() > 20 {
            msg.push_str(&format!("\n      ... and {} more", analysis.underruns.len() - 20));
        }
        eprintln!("        {label}{name} smp={smp} {msg}");
    }
    if !analysis.clicks.is_empty() {
        let mut msg = format!("{} hard discontinuities (|delta| > 8000):", analysis.clicks.len());
        for click in analysis.clicks.iter().take(10) {
            msg.push_str(&format!(
                "\n      at {:8.3}s  {} -> {}",
                secs(click.index),
                click.from,
                click.to
            ));
        }
        if analysis.clicks.len() > 10 {
            msg.push_str(&format!("\n      ... and {} more", analysis.clicks.len() - 10));
        }
        problems.push(msg);
    }

    // §5.8 suspend structure — categorical per-run assertions, so they belong
    // with the instrument checks: fatal in both tiers, never a counted rate.
    problems.extend(audio::check_suspend_structure(&serial));

    // Keep every capture that shows something, so a dropout can be listened to
    // even when the tier's rule says one occurrence is not yet a verdict.
    if !problems.is_empty() || !breaches.is_empty() || !gaps.is_empty() {
        let suffix = if tag.is_empty() {
            String::new()
        } else {
            format!("-{tag}")
        };
        let kept = qemu
            .audio_wav_path()
            .with_file_name(format!("audio-{name}-smp{smp}{suffix}.wav"));
        match fs::rename(qemu.audio_wav_path(), &kept) {
            Ok(()) => eprintln!("        {label}{name} smp={smp} wav kept at {}", kept.display()),
            Err(e) => eprintln!(
                "        {label}{name} smp={smp} could not keep {}: {e}",
                kept.display()
            ),
        }
    }

    Ok(AudioRun {
        gaps,
        counters,
        broken: problems,
        breaches,
    })
}

/// Fast tier — one boot per config, run on every `cargo test`.
///
/// Certifies: this build's soundd counters sit inside their recorded per-run
/// ceilings, the instrument is alive, and the capture does not *reproducibly*
/// drop audio. It cannot certify a dropout *rate*; one run is one Bernoulli
/// trial against a per-config rate measured at 0-7%, which discriminates
/// nothing. That is what `--audio-gate` is for.
///
/// The dropout check keeps the strict zero-gap bar and adds a confirmation: a
/// run that gaps is re-booted once, and only a second gap fails. No limit is
/// widened by this. Without the confirmation the per-config dropout rate alone
/// reds one invocation in eight on a clean tree, and a gate developers see
/// every day cannot cry wolf that often. The first gap is still printed and the
/// capture still kept.
fn run_audio_test(
    name: &str,
    smp: u32,
    baseline: &ConfigBaseline,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let run = measure_audio_run(name, smp, baseline, test_config, c_bins, rust_bins, "")?;

    let problems = [run.broken.as_slice(), run.breaches.as_slice()].concat();
    if !problems.is_empty() {
        return Err(problems.join("\n    "));
    }
    if !run.dropped_audio() {
        return Ok(());
    }

    eprintln!(
        "        {name} smp={smp} DROPOUT {} — rare on this tree ({} of {} recorded runs); \
         re-booting once to confirm",
        audio::format_histogram(&run.gaps),
        baseline.sample.gap_runs,
        baseline.sample.gap_sample,
    );
    let again = measure_audio_run(name, smp, baseline, test_config, c_bins, rust_bins, "confirm")?;
    let problems = [again.broken.as_slice(), again.breaches.as_slice()].concat();
    if !problems.is_empty() {
        return Err(problems.join("\n    "));
    }
    if again.dropped_audio() {
        return Err(format!(
            "audio dropped out on two consecutive boots: {} then {}",
            audio::format_histogram(&run.gaps),
            audio::format_histogram(&again.gaps),
        ));
    }
    eprintln!("        {name} smp={smp} not reproduced on the confirming boot");
    Ok(())
}

// Thorough tier: `cargo test --test toyos-build -- --audio-gate N`

/// One config's fresh sample, accumulated over the N iterations.
#[derive(Default)]
struct GateSamples {
    max_wake_lat_us: Vec<f64>,
    underruns: Vec<f64>,
    wakes: Vec<f64>,
    drains: Vec<f64>,
    gap_runs: u32,
    ceiling_runs: u32,
}

/// A rejected statistic, ready to print.
struct Verdict {
    config: String,
    statistic: String,
    detail: String,
}

fn mwu_verdict(
    config: &str,
    statistic: &str,
    base: &[f64],
    fresh: &[f64],
    worse_is_lower: bool,
) -> Option<Verdict> {
    let z = stats::mann_whitney_z(base, fresh);
    let z = if worse_is_lower { -z } else { z };
    let med = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    (z > stats::Z_CRIT).then(|| Verdict {
        config: config.to_string(),
        statistic: statistic.to_string(),
        detail: format!(
            "median {:.0} -> {:.0} (Mann-Whitney z={z:.2} > {:.2})",
            med(base),
            med(fresh),
            stats::Z_CRIT
        ),
    })
}

fn rate_verdict(
    config: &str,
    statistic: &str,
    k1: u32,
    n1: u32,
    k0: u32,
    n0: u32,
) -> Option<Verdict> {
    let p = stats::fisher_greater(k1, n1, k0, n0);
    (p <= stats::ALPHA).then(|| Verdict {
        config: config.to_string(),
        statistic: statistic.to_string(),
        detail: format!(
            "{k1} of {n1} vs recorded {k0} of {n0} (Fisher p={p:.2e} <= {:.0e})",
            stats::ALPHA
        ),
    })
}

/// Thorough tier — N iterations of all four configs, gating on *rates* and
/// *distributions* rather than on single outcomes. This is what a
/// scheduler-migration stage transition must pass (spec §11 gate A).
///
/// Certifies, at N=30 and the measured clean-tree distributions:
///   * wake lateness has not shifted by 25% (detected 99.9% of the time) or
///     20% (93%). A 10% shift is missed (4%).
///   * periods of silence on the wire have not risen 25% (94%) or 50% (100%).
///   * soundd is not being woken less often — the signature of completions
///     being batched because it ran late. A 5% drop is caught 99.9% of the
///     time.
///   * the mid-tone dropout *rate* has not risen 10x (100%) or 5x (71%).
///     A doubling is NOT detectable at this N and never will be at any N a
///     human waits for: separating 3% from 7% at this confidence needs ~600
///     runs per config. The counters above are the instrument with power; the
///     dropout rate is the audible symptom, kept because it is the only
///     statistic here that says "someone would have heard it".
///
/// False-red rate on a clean tree: 0.25%, measured over 2000 invocations
/// simulated from the recorded distributions.
fn run_audio_gate(
    iterations: u32,
    audio_baseline: &AudioBaseline,
    audio_to_run: &[&str],
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> bool {
    let configs: Vec<(&str, u32)> = audio_to_run
        .iter()
        .flat_map(|name| AUDIO_SMP.iter().map(move |&smp| (*name, smp)))
        .collect();
    let mut samples: BTreeMap<String, GateSamples> = BTreeMap::new();
    let start = std::time::Instant::now();

    eprintln!(
        "\n[gate A] {iterations} iterations x {} configs, serial. Every per-run outcome \
         becomes a rate; the verdict is on the collection, not on any one run.",
        configs.len()
    );

    for iter in 1..=iterations {
        eprintln!("  --- iteration {iter}/{iterations} ---");
        for &(name, smp) in &configs {
            let key = format!("{name}.smp{smp}");
            let baseline = config_baseline(audio_baseline, name, smp);
            let tag = format!("iter{iter:03}");
            let run = match measure_audio_run(
                name, smp, &baseline, test_config, c_bins, rust_bins, &tag,
            ) {
                Ok(run) => run,
                Err(err) => {
                    eprintln!("\n[gate A] FAILED on iteration {iter}: {key} produced no measurement: {err}");
                    eprintln!("[gate A] A run that does not complete is not a rare event to be \
                               averaged away — every known cause of one has been fixed.");
                    return false;
                }
            };
            if !run.broken.is_empty() {
                eprintln!("\n[gate A] FAILED on iteration {iter}: {key} instrument broken: {}",
                          run.broken.join("; "));
                return false;
            }
            let s = samples.entry(key).or_default();
            s.max_wake_lat_us.push(run.counters.max_wake_lat_us as f64);
            s.underruns.push(run.counters.underruns as f64);
            s.wakes.push(run.counters.wakes as f64);
            s.drains.push(run.counters.drains as f64);
            s.gap_runs += u32::from(run.dropped_audio());
            s.ceiling_runs += u32::from(!run.breaches.is_empty());
        }

        // Fail-side curtailment. Adding runs can only raise a count, so once a
        // count passes the threshold for the *full* N the final verdict is
        // already decided — stopping early costs no confidence.
        if let Some(v) = curtail(&samples, audio_baseline, &configs, iterations) {
            eprintln!("\n[gate A] FAILED after {iter} of {iterations} iterations (the remaining \
                       runs cannot change this):");
            eprintln!("    {} {}: {}", v.config, v.statistic, v.detail);
            return false;
        }
    }

    let mut rejected: Vec<Verdict> = Vec::new();
    let (mut pooled_gap_k, mut pooled_gap_n) = (0, 0);
    let (mut pooled_ceil_k, mut pooled_ceil_n) = (0, 0);
    let (mut base_gap_k, mut base_gap_n) = (0, 0);
    let (mut base_ceil_k, mut base_ceil_n) = (0, 0);

    eprintln!("\n[gate A] {iterations} iterations in {:.0?}. Fresh sample vs recorded sample:\n", start.elapsed());
    for &(name, smp) in &configs {
        let key = format!("{name}.smp{smp}");
        let base = config_baseline(audio_baseline, name, smp).sample;
        let s = &samples[&key];

        rejected.extend(mwu_verdict(&key, "wake lateness", &base.max_wake_lat_us, &s.max_wake_lat_us, false));
        rejected.extend(mwu_verdict(&key, "underruns", &base.underruns, &s.underruns, false));
        rejected.extend(mwu_verdict(&key, "wakes", &base.wakes, &s.wakes, true));
        rejected.extend(rate_verdict(&key, "dropout rate", s.gap_runs, iterations, base.gap_runs, base.gap_sample));

        pooled_gap_k += s.gap_runs;
        pooled_gap_n += iterations;
        pooled_ceil_k += s.ceiling_runs;
        pooled_ceil_n += iterations;
        base_gap_k += base.gap_runs;
        base_gap_n += base.gap_sample;
        base_ceil_k += base.ceiling_runs;
        base_ceil_n += base.max_wake_lat_us.len() as u32;

        report_config(&key, base, s, iterations);
    }
    rejected.extend(rate_verdict("pooled", "dropout rate", pooled_gap_k, pooled_gap_n, base_gap_k, base_gap_n));
    rejected.extend(rate_verdict("pooled", "per-run ceiling breaches", pooled_ceil_k, pooled_ceil_n, base_ceil_k, base_ceil_n));

    eprintln!(
        "  pooled dropouts {pooled_gap_k}/{pooled_gap_n} (recorded {base_gap_k}/{base_gap_n}), \
         ceiling breaches {pooled_ceil_k}/{pooled_ceil_n} (recorded {base_ceil_k}/{base_ceil_n})"
    );

    if rejected.is_empty() {
        eprintln!("\n[gate A] PASS — no statistic regressed at alpha={:.0e} per test.", stats::ALPHA);
        true
    } else {
        eprintln!("\n[gate A] FAILED — {} statistic(s) regressed:", rejected.len());
        for v in &rejected {
            eprintln!("    {} {}: {}", v.config, v.statistic, v.detail);
        }
        false
    }
}

/// Whether a count has already passed the threshold it would face at the full
/// iteration count. Only the yes/no statistics curtail: a rank test's outcome
/// is not monotone in the sample, so there is no honest early exit for it.
fn curtail(
    samples: &BTreeMap<String, GateSamples>,
    audio_baseline: &AudioBaseline,
    configs: &[(&str, u32)],
    iterations: u32,
) -> Option<Verdict> {
    let mut pooled_gap = 0;
    let mut pooled_ceil = 0;
    let (mut base_gap_k, mut base_gap_n) = (0, 0);
    let (mut base_ceil_k, mut base_ceil_n) = (0, 0);
    for &(name, smp) in configs {
        let key = format!("{name}.smp{smp}");
        let base = config_baseline(audio_baseline, name, smp).sample;
        let Some(s) = samples.get(&key) else { continue };
        if let Some(v) = rate_verdict(&key, "dropout rate", s.gap_runs, iterations, base.gap_runs, base.gap_sample) {
            return Some(v);
        }
        pooled_gap += s.gap_runs;
        pooled_ceil += s.ceiling_runs;
        base_gap_k += base.gap_runs;
        base_gap_n += base.gap_sample;
        base_ceil_k += base.ceiling_runs;
        base_ceil_n += base.max_wake_lat_us.len() as u32;
    }
    let n = iterations * configs.len() as u32;
    rate_verdict("pooled", "dropout rate", pooled_gap, n, base_gap_k, base_gap_n)
        .or_else(|| rate_verdict("pooled", "per-run ceiling breaches", pooled_ceil, n, base_ceil_k, base_ceil_n))
}

/// Print one config's fresh sample next to the recorded one, in a form that can
/// be pasted straight back into `tests/audio-baseline.toml` when a re-baseline
/// is deliberate. The gate's output *is* the next baseline.
fn report_config(key: &str, base: &BaselineSample, s: &GateSamples, iterations: u32) {
    let stat = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (v[0], v[v.len() / 2], v[v.len() - 1])
    };
    eprintln!("  {key}  (n={iterations}, recorded n={})", base.max_wake_lat_us.len());
    for (label, b, f) in [
        ("wake_lat_us", &base.max_wake_lat_us, &s.max_wake_lat_us),
        ("underruns  ", &base.underruns, &s.underruns),
        ("wakes      ", &base.wakes, &s.wakes),
        ("drains     ", &base.drains, &s.drains),
    ] {
        let (bl, bm, bh) = stat(b);
        let (fl, fm, fh) = stat(f);
        eprintln!(
            "    {label} recorded {bl:.0}/{bm:.0}/{bh:.0}   fresh {fl:.0}/{fm:.0}/{fh:.0}   (min/median/max)"
        );
    }
    eprintln!(
        "    dropouts    recorded {}/{}   fresh {}/{iterations}",
        base.gap_runs, base.gap_sample, s.gap_runs
    );
    let fmt = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let v: Vec<String> = v.iter().map(|x| format!("{x:.0}")).collect();
        format!("[{}]", v.join(", "))
    };
    eprintln!("    toml: max_wake_lat_us = {}", fmt(&s.max_wake_lat_us));
    eprintln!("    toml: underruns = {}", fmt(&s.underruns));
    eprintln!("    toml: wakes = {}", fmt(&s.wakes));
    eprintln!("    toml: drains = {}", fmt(&s.drains));
}

/// Echo what the guest actually put on screen, under `--nocapture` only —
/// it is the measurement these tests are built on, and the audio gate prints
/// its numbers for the same reason.
fn print_screen(name: &str, text: &str) {
    if !qemu::VERBOSE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    eprintln!("        {name} decoded screen:");
    for line in text.lines() {
        eprintln!("        | {line}");
    }
}

/// Run one screen test. `Err` carries the decoded screen, because a failure
/// here is almost always "the text is not what I expected" and the decoded
/// grid is the only readable form of that.
fn run_screen_test(
    name: &str,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    match name {
        "screen_decoder" => {
            screen::self_test();
            Ok(())
        }
        "screen_early_panic" => {
            // The window the console exists for: percpu is not up, mm::init
            // has not run, and on a machine with no UART nothing else can
            // report at all. render() runs before panic_flush, so the marker
            // reaching the UART proves the paint already finished — no sleep.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    display: qemu::Display::Gop,
                    qmp: true,
                    kernel_features: &["test-early-panic"],
                    ready_marker: "!!! EARLY PANIC !!!",
                    ..Default::default()
                },
            );
            let text = qemu.screendump().text();
            print_screen(name, &text);
            for want in ["!!! EARLY PANIC !!!", "test-early-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            Ok(())
        }
        other => Err(format!("unknown screen test {other}")),
    }
}

fn build_test_registry(
    rust_bins: &[(String, Vec<u8>)],
    c_names: &[String],
) -> Vec<TestDef> {
    let mut tests = Vec::new();

    for name in discover_rust_tests(rust_bins) {
        let timeout = match name.as_str() {
            "panic_recovery" => Duration::from_secs(10),
            _ => Duration::from_secs(5),
        };
        tests.push(TestDef {
            qemu_name: format!("test_rs_{name}"),
            check: check_for(&name),
            timeout,
            name,
        });
    }

    for name in c_names {
        tests.push(TestDef {
            qemu_name: format!("test_c_{name}"),
            timeout: Duration::from_secs(10),
            check: check_c_result,
            name: name.clone(),
        });
    }

    tests
}

fn run_debug_mode(c_tests: &[(String, Vec<u8>)], rust_bins: &[(String, Vec<u8>)]) {
    let cmd_path = Path::new("/tmp/toyos-debug-cmd");
    let result_path = Path::new("/tmp/toyos-debug-result");
    let ready_path = Path::new("/tmp/toyos-debug-ready");

    let _ = fs::remove_file(cmd_path);
    let _ = fs::remove_file(result_path);
    let _ = fs::remove_file(ready_path);

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let mut qemu = QemuInstance::boot_with_options(
        &test_config,
        c_tests,
        rust_bins,
        BootOptions {
            gdb_stub: true,
            debug_wait: true,
            ..Default::default()
        },
    );

    let repo = compile::repo_root();
    let kernel_elf = repo.join("kernel/target/x86_64-unknown-none/debug/kernel");

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  QEMU running with GDB stub on localhost:1234               ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║  Kernel ELF: {}", kernel_elf.display());
    eprintln!("║                                                              ║");
    eprintln!("║  Send commands:                                              ║");
    eprintln!("║    echo 'run test_c_49_bracket_evaluation' > {}    ║", cmd_path.display());
    eprintln!("║    echo 'run test_rs_std_alloc' > {}               ║", cmd_path.display());
    eprintln!("║    cat {}                                 ║", result_path.display());
    eprintln!("║    echo 'quit' > {}                                ║", cmd_path.display());
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    fs::write(ready_path, "ready\n").unwrap();

    loop {
        thread::sleep(Duration::from_millis(200));

        let cmd = match fs::read_to_string(cmd_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let _ = fs::remove_file(cmd_path);
        let cmd = cmd.trim();
        if cmd.is_empty() {
            continue;
        }

        if cmd == "quit" || cmd == "q" {
            eprintln!("[debug] Quit requested");
            let _ = fs::write(result_path, "quit\n");
            break;
        }

        if let Some(test_name) = cmd.strip_prefix("run ") {
            let test_name = test_name.trim();
            eprintln!("[debug] Running {test_name}...");
            let result = qemu.run_test(test_name, Duration::from_secs(60));

            let mut output = String::new();
            output.push_str(&format!("test: {}\n", result.name));
            output.push_str(&format!("exit_code: {:?}\n", result.exit_code));
            if let Some(err) = &result.error {
                output.push_str(&format!("error: {err}\n"));
            }
            if !result.stdout.is_empty() {
                output.push_str("--- stdout ---\n");
                output.push_str(&result.stdout);
            }
            eprintln!("[debug] {output}");
            fs::write(result_path, &output).unwrap();
        } else {
            eprintln!("[debug] Sending raw serial: {cmd}");
            writeln!(qemu.stdin_mut(), "{cmd}").expect("Failed to write to QEMU stdin");
            qemu.flush_stdin();
            fs::write(result_path, "sent\n").unwrap();
        }
    }

    let _ = fs::remove_file(ready_path);
    eprintln!("[debug] Shutting down QEMU...");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let debug_mode = args.iter().any(|a| a == "--debug");
    let list_mode = args.iter().any(|a| a == "--list");
    let nocapture = args.iter().any(|a| a == "--nocapture" || a == "--show-output");

    // Thorough tier. A flag rather than an env var or a test name: an env var
    // is invisible in the command line and easy to leave set, and a test name
    // would drag ~17 minutes into every plain `cargo test`.
    let mut audio_gate: Option<u32> = None;
    let mut consumed: Vec<usize> = Vec::new();
    for (i, a) in args.iter().enumerate() {
        let n = if let Some(v) = a.strip_prefix("--audio-gate=") {
            consumed.push(i);
            v
        } else if a == "--audio-gate" {
            consumed.push(i);
            consumed.push(i + 1);
            args.get(i + 1).map(|s| s.as_str()).unwrap_or_else(|| {
                panic!("--audio-gate needs an iteration count, e.g. --audio-gate 30")
            })
        } else {
            continue;
        };
        let n: u32 = n
            .parse()
            .unwrap_or_else(|_| panic!("--audio-gate: {n:?} is not an iteration count"));
        assert!(n >= 2, "--audio-gate needs at least 2 iterations to compare anything");
        audio_gate = Some(n);
    }

    if nocapture || debug_mode {
        common::qemu::VERBOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Filter: first positional arg that isn't a flag
    let filter: Option<&str> = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && !consumed.contains(i))
        .map(|(_, s)| s.as_str());

    let c_names = discover_c_tests();
    eprintln!("[toyos] Compiling {} C tests...", c_names.len());
    let c_bins = compile_c_tests(&c_names);
    let c_compiled: Vec<String> = c_bins.iter().map(|(n, _)| n.clone()).collect();

    let rust_tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/toyos-rust-tests");
    eprintln!("[toyos] Building Rust tests...");
    let rust_bins = qemu::build_toyos_bins(&rust_tests_dir);

    // --list: print test names and exit
    if list_mode {
        let tests = build_test_registry(&rust_bins, &c_compiled);
        for t in &tests {
            println!("{}", t.name);
        }
        for name in AUDIO_TESTS {
            println!("{name}");
        }
        for name in SCREEN_TESTS {
            println!("{name}");
        }
        return;
    }

    if debug_mode {
        run_debug_mode(&c_bins, &rust_bins);
        return;
    }

    if let Some(iterations) = audio_gate {
        let audio_to_run: Vec<&str> = AUDIO_TESTS
            .iter()
            .copied()
            .filter(|n| filter.map_or(true, |f| n.contains(f)))
            .collect();
        assert!(!audio_to_run.is_empty(), "no audio test matches filter {filter:?}");
        let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
        let ok = run_audio_gate(
            iterations,
            &load_audio_baseline(),
            &audio_to_run,
            &test_config,
            &c_bins,
            &rust_bins,
        );
        if !ok {
            std::process::exit(1);
        }
        return;
    }

    let all_tests = build_test_registry(&rust_bins, &c_compiled);
    let tests_to_run: Vec<&TestDef> = match filter {
        Some(f) => all_tests.iter().filter(|t| t.name.contains(f)).collect(),
        None => all_tests.iter().collect(),
    };
    let audio_to_run: Vec<&str> = AUDIO_TESTS
        .iter()
        .copied()
        .filter(|n| filter.map_or(true, |f| n.contains(f)))
        .collect();
    let screen_to_run: Vec<&str> = SCREEN_TESTS
        .iter()
        .copied()
        .filter(|n| filter.map_or(true, |f| n.contains(f)))
        .collect();

    if tests_to_run.is_empty() && audio_to_run.is_empty() && screen_to_run.is_empty() {
        eprintln!("No tests match filter {:?}", filter);
        std::process::exit(1);
    }

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let total =
        tests_to_run.len() + audio_to_run.len() * AUDIO_SMP.len() + screen_to_run.len();
    eprintln!("\nrunning {total} tests\n");
    let mut passed = 0;
    let mut failed = 0;
    let mut failures: Vec<(String, String)> = Vec::new();
    let suite_start = std::time::Instant::now();

    // Shared boot: everything except audio tests runs in one QEMU.
    if !tests_to_run.is_empty() {
        eprintln!(
            "[toyos] Booting QEMU with {} C + {} Rust binaries...",
            c_bins.len(),
            rust_bins.len()
        );
        let mut qemu = QemuInstance::boot(&test_config, &c_bins, &rust_bins);
        let mut last_prefix = "";

        for test in &tests_to_run {
            let prefix = if test.qemu_name.starts_with("test_rs_") {
                "rust"
            } else {
                "c"
            };
            if prefix != last_prefix {
                eprintln!("  --- {prefix} ---");
                last_prefix = prefix;
            }

            let start = std::time::Instant::now();
            let result = qemu.run_test(&test.qemu_name, test.timeout);
            let elapsed = start.elapsed();
            let ok = (test.check)(&result);
            if ok {
                passed += 1;
                eprintln!("  PASS  {}  ({:.0?})", test.name, elapsed);
            } else {
                failed += 1;
                let reason = result
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("exit code {:?}", result.exit_code));
                failures.push((test.name.clone(), reason));
                eprintln!("  FAIL  {}  ({:.0?})", test.name, elapsed);
            }
        }
    }

    // Audio tests: one dedicated boot per (test, smp) config, wav glitch
    // analysis gated on the recorded baseline.
    if !audio_to_run.is_empty() {
        let audio_baseline = load_audio_baseline();
        eprintln!("  --- audio ---");
        for name in &audio_to_run {
            for &smp in AUDIO_SMP {
                let label = format!("{name} (smp={smp})");
                let baseline = config_baseline(&audio_baseline, name, smp);
                let start = std::time::Instant::now();
                let outcome =
                    run_audio_test(name, smp, &baseline, &test_config, &c_bins, &rust_bins);
                let elapsed = start.elapsed();
                match outcome {
                    Ok(()) => {
                        passed += 1;
                        eprintln!("  PASS  {label}  ({:.0?})", elapsed);
                    }
                    Err(reason) => {
                        failed += 1;
                        eprintln!("FAIL rs::{label}: {reason}");
                        eprintln!("  FAIL  {label}  ({:.0?})", elapsed);
                        let summary = reason.lines().next().unwrap_or("audio check failed");
                        failures.push((label, summary.to_string()));
                    }
                }
            }
        }
    }

    // Last, because they are the only tests that toggle a kernel feature and
    // so force a kernel rebuild; running them here confines that to one
    // rebuild per invocation instead of two.
    if !screen_to_run.is_empty() {
        eprintln!("  --- screen ---");
        for name in &screen_to_run {
            let start = std::time::Instant::now();
            let outcome = run_screen_test(name, &test_config, &c_bins, &rust_bins);
            let elapsed = start.elapsed();
            match outcome {
                Ok(()) => {
                    passed += 1;
                    eprintln!("  PASS  {name}  ({:.0?})", elapsed);
                }
                Err(reason) => {
                    failed += 1;
                    eprintln!("FAIL {name}: {reason}");
                    eprintln!("  FAIL  {name}  ({:.0?})", elapsed);
                    let summary = reason.lines().next().unwrap_or("screen check failed");
                    failures.push((name.to_string(), summary.to_string()));
                }
            }
        }
    }

    let suite_elapsed = suite_start.elapsed();

    eprintln!();
    if failures.is_empty() {
        eprintln!(
            "test result: ok. {passed} passed, {total} total ({:.1?})",
            suite_elapsed
        );
    } else {
        eprintln!("failures:");
        for (name, reason) in &failures {
            eprintln!("    {name}: {reason}");
        }
        eprintln!();
        eprintln!(
            "test result: FAILED. {passed} passed, {failed} failed, {total} total ({:.1?})",
            suite_elapsed
        );
        std::process::exit(1);
    }
}
