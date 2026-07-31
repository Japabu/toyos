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
const RUST_SKIP: &[&str] = &[
    "segfault_child",
    "test_panic_child",
    "i8042_keyboard",
    "i8042_mouse",
    "input_events",
];

// Audio glitch tests. Each runs in its own QEMU boot per SMP config and
// asserts on the wav the virtio-sound device captured, so they are excluded
// from the shared multi-test boot.
const AUDIO_TESTS: &[&str] = &["audio_tone", "audio_tone_load"];

// Scheduler-core gate A covers both SMP configs: smp=1 is the audio spec's
// first-class single-CPU case, smp=8 the full-SMP case.
const AUDIO_SMP: &[u32] = &[1, 8];

// Tests that read a decoded screendump, which is exactly the set for which
// the screen is the device under test: the panic console. On a machine with
// no serial port the rendered report is the only diagnostic that exists, so
// asserting on pixels there is asserting on the product. Everything else that
// used to read a screendump now reads the console instead — a screenshot is a
// poor way to ask "did the right process come up", and thresholds over a live
// desktop are how those tests passed vacuously twice.
// `screen_decoder` needs no guest at all; it proves the decoder against a
// bitmap it rendered itself, before anything points it at a real screen.
/// Feature-carrying tests last: each distinct kernel feature set is one more
/// kernel rebuild, and ending on one leaves the plain-kernel tests above it
/// untouched by the thrash.
const SCREEN_TESTS: &[&str] = &[
    "screen_decoder",
    "screen_recoverable_untouched",
    "screen_early_panic",
    "screen_late_panic",
    "screen_panic_muted",
    "screen_fatal_halt",
];

/// Tests whose machine shape *is* the test: metal-sim, where the PS/2
/// keyboard is the only input source and no virtio device exists, or a q35
/// with the i8042 switched off. None of them can share the multi-test boot,
/// so each costs its own. `run_machine_test` dispatches them.
/// Feature-carrying ones last, as SCREEN_TESTS does: each distinct kernel
/// feature set is another kernel rebuild.
const MACHINE_TESTS: &[&str] = &[
    "ioapic_topology",
    "input_merge",
    "metal_sim_compositor",
    "metal_sim_input",
    "i8042_keyboard",
    "i8042_no_spurious_wake",
    "i8042_mouse",
    "i8042_absent",
    "i8042_quarantine",
];

/// The renderer's two text colours, as the screendump reports them.
const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
const ALERT: [u8; 3] = [0xFF, 0x50, 0x50];
/// And the fill a halted machine leaves behind.
const FILL_FATAL: [u8; 3] = [0x60, 0x00, 0x00];

/// The line `SYS_DEBUG` action 3 logs immediately before halting every CPU.
/// Action 3 exists only under the `test-fatal-halt` kernel feature, which
/// screen_fatal_halt is the only caller of. Kept in sync with
/// `kernel/src/arch/syscall.rs` by this comment and by screen_fatal_halt
/// failing loudly if it drifts.
const FATAL_HALT_NONCE: &str = "SYS_DEBUG: fatal halt 4b1d9e2c";

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
    if let Err(msg) = check_tripwire_attribution(&result.serial) {
        eprintln!("FAIL rs::panic_recovery: {msg}\nserial:\n{}", result.serial);
        ok = false;
    }
    ok
}

/// The §6.4 tripwire must fire, and its `panicked at` must name the syscall
/// that held the lock rather than the scheduler that caught it — which is the
/// only thing `#[track_caller]` on `assert_baseline` buys.
///
/// A whole-buffer `contains("arch/syscall.rs")` certifies none of that: the
/// same boot's `test_syscall_panic` panics in that file too, so the needle is
/// already present before the tripwire runs. Scope it instead to the window
/// between this panic's header and its message — `panicked at <location>` is
/// the only thing in there, and the backtrace that names every frame comes
/// after the message, so it cannot supply the answer either.
fn check_tripwire_attribution(serial: &str) -> Result<(), String> {
    const MSG: &str = "scheduler entered while a lock is held";
    const HEADER: &str = "!!! PANIC !!!";
    let msg_at = serial
        .find(MSG)
        .ok_or("expected the §6.4 lock-across-switch tripwire to fire")?;
    let header_at = serial[..msg_at]
        .rfind(HEADER)
        .ok_or("tripwire message with no panic header before it")?;
    let location = &serial[header_at..msg_at];
    if !location.contains("arch/syscall.rs") {
        return Err(format!(
            "expected the tripwire to name the guilty call site, not scheduler.rs; got: {}",
            location.trim()
        ));
    }
    Ok(())
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

/// Assert the two colour decisions `text()` cannot see: the fill, and the
/// alert highlight on a `!!!` line against white everywhere else.
fn check_colors(dump: &screen::Ppm, fill: [u8; 3], alert_line: &str) -> Result<(), String> {
    if dump.fill() != fill {
        return Err(format!("fill is {:?}, want {fill:?}", dump.fill()));
    }
    let rows = dump.rows();
    let Some(cy) = dump.row_index(alert_line) else {
        return Err(format!("{alert_line:?} not on screen"));
    };
    if dump.row_fg(cy) != Some(ALERT) {
        return Err(format!(
            "{alert_line:?} drawn in {:?}, want alert {ALERT:?}",
            dump.row_fg(cy)
        ));
    }
    let Some(plain) = rows.iter().position(|r| !r.is_empty() && !r.contains("!!!")) else {
        return Err("no ordinary row to compare the highlight against".to_string());
    };
    if dump.row_fg(plain) != Some(WHITE) {
        return Err(format!(
            "ordinary row {:?} drawn in {:?}, want white {WHITE:?}",
            rows[plain],
            dump.row_fg(plain)
        ));
    }
    Ok(())
}

/// Assert the renderer wrapped a backtrace line rather than clipping it.
///
/// The stimulus is the panic's own bottom frame: `late_panic::Nest` is a
/// generic nested in itself, so its demangled symbol is wider than any
/// console grid and its head and tail cannot share a display row. Wrap-over-
/// clip exists precisely so the symbol at the *end* of such a line survives,
/// which is why the tail is the thing asserted.
fn check_wrap(dump: &screen::Ppm) -> Result<(), String> {
    let rows = dump.rows();
    let Some(head) = dump.row_index("late_panic::Nest") else {
        return Err(format!(
            "no `late_panic::Nest` frame on screen — no over-wide symbol to wrap\n{}",
            dump.text()
        ));
    };
    if rows[head].contains("on_screen_console_check") {
        return Err(format!(
            "the frame fit one display row ({} columns); wrap is not exercised",
            rows[head].len()
        ));
    }
    if !rows[head..].iter().take(4).any(|r| r.contains("on_screen_console_check")) {
        return Err(format!(
            "the tail of the demangled symbol never reached the screen — clipped?\n{}",
            dump.text()
        ));
    }
    Ok(())
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
        "screen_panic_muted" => {
            // The machine the whole M0/M1 line exists for: metal-sim with the
            // 16550 taken away, so `uart_present()` is false, `panic_flush`
            // returns without draining anywhere, and the rendered screen is
            // the only channel the report can possibly reach. Same kernel
            // feature and same image as `screen_late_panic`, so this costs a
            // boot and no rebuild — and it is the one place the absent-UART
            // branches run at all.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                mute: true,
                kernel_features: &["test-late-panic"],
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            metal_sim_argv_check(&argv)?;
            match argv.iter().position(|a| a == "-serial") {
                Some(i) if argv.get(i + 1).is_some_and(|v| v == "none") => {}
                _ => return Err(format!("the muted profile still has a 16550: {argv:?}")),
            }
            if argv.iter().any(|a| a.contains("stdio")) {
                return Err(format!("the muted profile still has a stdio chardev: {argv:?}"));
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            // Nothing announces the panic here — there is no console for a
            // marker to arrive on — so the screen is polled until it carries
            // the report. 30s covers firmware plus the initrd read off USB.
            let dump = qemu.screendump_until("!!! PANIC !!!", Duration::from_secs(30));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! PANIC !!!", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!(
                        "{want:?} not on screen of a guest with no serial port at all\ndecoded screen:\n{text}"
                    ));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! PANIC !!!")?;
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
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_features: &["test-early-panic"],
                    ready_marker: "!!! EARLY PANIC !!!",
                    ..Default::default()
                },
            );
            let dump = qemu.screendump();
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! EARLY PANIC !!!", "test-early-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! EARLY PANIC !!!")?;
            Ok(())
        }
        "screen_late_panic" => {
            // The ordinary fatal panic, which no userland process can produce:
            // crash_report, capture, panic_flush, halt_all_cpus, render. The
            // flush drains the ring before the paint, so the snapshot capture()
            // took is the only thing left to paint from.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_features: &["test-late-panic"],
                    ready_marker: "!!! PANIC !!!",
                    ..Default::default()
                },
            );
            // Here the marker reaches serial *before* the paint — the drain is
            // what emits it — so unlike the halt paths this one has to look
            // more than once.
            let dump = qemu.screendump_until("!!! PANIC !!!", Duration::from_secs(10));
            let text = dump.text();
            print_screen(name, &text);
            for want in ["!!! PANIC !!!", "test-late-panic: on-screen console check"] {
                if !text.contains(want) {
                    return Err(format!("{want:?} not on screen\ndecoded screen:\n{text}"));
                }
            }
            check_colors(&dump, FILL_FATAL, "!!! PANIC !!!")?;
            check_wrap(&dump)?;
            Ok(())
        }
        "screen_fatal_halt" => {
            // The steady-state fatal path: userland is up, the display is
            // idle, and SYS_DEBUG action 3 runs halt_all_cpus for real.
            // No sleep — render() runs before panic_flush(), so the nonce
            // arriving on the console proves the paint already finished.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    kernel_features: &["test-fatal-halt"],
                    ..Default::default()
                },
            );
            if !qemu.command_until(
                "run test_rs_test_panic_child 3",
                FATAL_HALT_NONCE,
                Duration::from_secs(15),
            ) {
                return Err(format!("{FATAL_HALT_NONCE:?} never reached the console"));
            }
            let dump = qemu.screendump();
            let text = dump.text();
            print_screen(name, &text);
            if !text.contains(FATAL_HALT_NONCE) {
                return Err(format!(
                    "{FATAL_HALT_NONCE:?} reached serial but not the screen\ndecoded screen:\n{text}"
                ));
            }
            if dump.fill() != FILL_FATAL {
                return Err(format!("fatal fill is {:?}, want {FILL_FATAL:?}", dump.fill()));
            }
            Ok(())
        }
        "screen_recoverable_untouched" => {
            // The negative of screen_fatal_halt, and the property that makes
            // the capture/render split worth having: a panic the kernel
            // recovers from must not clobber a live display. Action 0 panics
            // in syscall context, which the handler recovers from, so it
            // never reaches halt_all_cpus and must leave every pixel alone.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Gop,
                    qmp: true,
                    ..Default::default()
                },
            );
            let before = qemu.screendump();
            let result = qemu.run_test("test_rs_test_panic_child", Duration::from_secs(15));
            // The premise, not a formality: a timeout returns exit_code None,
            // which the old `!= Some(0)` check accepted — so a panic that
            // never fired left two identical screendumps and a green test.
            if let Some(err) = &result.error {
                return Err(format!("the recoverable panic never completed: {err}"));
            }
            if result.exit_code == Some(0) {
                return Err("recoverable panic did not kill the child".to_string());
            }
            if !result.serial.contains("SYS_DEBUG: kernel panic triggered by userspace") {
                return Err(format!(
                    "no kernel panic in the child's output\nserial:\n{}",
                    result.serial
                ));
            }
            let after = qemu.screendump();
            if !before.identical_to(&after) {
                return Err("recovering panic changed the screen".to_string());
            }
            // A screen that was blank to begin with would pass the diff for
            // the wrong reason.
            let text = before.text();
            print_screen(name, &text);
            if !text.contains("Boot: complete") {
                return Err(format!("nothing on screen to preserve\ndecoded screen:\n{text}"));
            }
            Ok(())
        }
        other => Err(format!("unknown screen test {other}")),
    }
}

/// Run a test that owns its QEMU, turning a panic into a failed test.
///
/// Every way the harness reports a dead or unreachable guest is a panic —
/// `wait_for_ready`'s boot timeout, `assert_alive`'s exit status, `Qmp`'s
/// connect and read asserts. Uncaught, one of those unwinds out of `main` and
/// the suite exits 101 with no failure list, no remaining tests and no screen:
/// the worst report for the failure class these tests exist to catch.
fn catching(f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|e| {
        Err(e
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "the boot panicked".to_string()))
    })
}

/// Every negative claim `Profile::Metal` makes, read off the argv QEMU is
/// launched with. A claim about which devices do *not* exist is a claim about
/// this list and nothing else — no console line and no screendump can see a
/// device that is present but unused.
fn metal_sim_argv_check(argv: &[String]) -> Result<(), String> {
    if let Some(bad) = argv.iter().find(|a| a.contains("virtio")) {
        return Err(format!("metal-sim passed a virtio device to QEMU: {bad}"));
    }
    // The mechanism, not two names. `xhci::device::scan_ports` binds any
    // boot-protocol HID — keyboard, mouse or tablet — so an enumeration of the
    // two device names that happen to be in the tree today would let a
    // `usb-mouse` added for debugging break the profile's only negative claim
    // while the assertion stayed green. The boot stick is the one USB device
    // this machine has.
    let hid = argv
        .windows(2)
        .filter(|w| w[0] == "-device")
        .map(|w| w[1].as_str())
        .find(|v| v.starts_with("usb-") && !v.starts_with("usb-storage"));
    if let Some(bad) = hid {
        return Err(format!("metal-sim passed a USB device that is not the boot stick: {bad}"));
    }
    // Without this QEMU adds an e1000e with a slirp backend, an ide-cd and an
    // isa-parallel that nothing declared — and the NIC is enough to make netd
    // claim a device on the machine whose whole point is that it has none.
    // None of them appears in argv, so this flag is the only observable form
    // of their absence here; `query-pci` is the direct one.
    if !argv.iter().any(|a| a == "-nodefaults") {
        return Err("metal-sim did not pass -nodefaults; QEMU's default-device pass is back".to_string());
    }
    Ok(())
}

/// Run one machine-shape test. Like `run_screen_test`, each of these owns its
/// QEMU: the machine shape *is* the test.
fn run_machine_test(
    name: &str,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
    match name {
        "metal_sim_compositor" => {
            // M1's permanent config: the whole boot with no virtio device
            // anywhere. What it certifies is which processes survive the
            // T14's device shape — the compositor claims a firmware
            // framebuffer and says what it got, soundd and netd find no
            // device and exit rather than panic. All three are the process's
            // own words. The earlier version read the bottom pixel row
            // instead, which says nothing about soundd or netd and stayed
            // green with their graceful exit reverted.
            let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/metalcase");
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;

            let mut qemu = QemuInstance::boot_with_options(&config, &[], &[], options);
            // init spawns all four programs without waiting, so test-runner's
            // ready marker races the daemons' own lines. Keep draining until
            // every line has been said or the window closes.
            const WANT: [&str; 3] = [
                "compositor: ready",
                "soundd: no audio device on this machine, exiting",
                "netd: no NIC on this machine, exiting",
            ];
            let mut console = qemu.boot_log().to_string();
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < deadline
                && !WANT.iter().all(|w| console.contains(w))
            {
                console.push_str(&qemu.drain_serial(Duration::from_millis(250)));
            }
            for want in WANT {
                if !console.contains(want) {
                    return Err(format!("{want:?} never reached the console:\n{console}"));
                }
            }
            // The compositor reports the mode it was handed, which is the
            // proof it claimed a real firmware framebuffer rather than
            // starting on nothing.
            let Some(mode) = console
                .lines()
                .find_map(|l| l.split("compositor: wallpaper ").nth(1))
            else {
                return Err(format!(
                    "the compositor never said what framebuffer it got:\n{console}"
                ));
            };
            // And nothing panicked on the way. A daemon that dies on its
            // absent device fails the positive check above; this catches the
            // rest of the boot dying instead.
            for bad in ["!!! PANIC !!!", "KERNEL PANIC", "panicked at"] {
                if console.contains(bad) {
                    return Err(format!("{bad:?} on a boot that should be clean:\n{console}"));
                }
            }
            eprintln!("  [metal-sim] compositor up on {}", mode.trim());
            eprintln!("  [metal-sim] soundd and netd both exited on their absent device");
            Ok(())
        }
        "ioapic_topology" => {
            // Everything the I/O APIC driver says happens in Phase 2, long
            // before the virtio-console exists, so the 16550 file is where a
            // host reads it. On the T14 the same lines land on the screen at
            // the next boot checkpoint; this is the QEMU-side equivalent.
            let qemu = QemuInstance::boot(test_config, c_bins, rust_bins);
            // The ready marker only proves the guest booted; the lines under
            // test were written before that, so nothing else to wait for.
            let log = qemu.boot_log().to_string();
            let units: Vec<&str> = log
                .lines()
                .filter_map(|l| l.split("ioapic: id=").nth(1))
                .collect();
            if units.is_empty() {
                return Err(format!("no `ioapic: id=` line in the boot log:\n{log}"));
            }
            // A window the machine does not decode answers 0xFFFFFFFF to
            // everything, which is a *valid-looking* unit: 256 entries, all
            // read back masked, `route` succeeds into nothing. The driver
            // drops such a unit, so its absence from the log is the assertion.
            if let Some(ignored) = log.lines().find(|l| l.contains("ioapic: id=") && l.contains("IGNORED")) {
                return Err(format!("an I/O APIC failed its plausibility gate: {ignored}"));
            }
            let mut covered: Vec<(u32, u32)> = Vec::new();
            for unit in &units {
                // `<id> at <addr> ver=<v> gsi <lo>..<hi> masked <n>/<total>`
                let ver = unit
                    .split_once(" ver=0x")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                    .ok_or_else(|| format!("no version in {unit:?}"))?;
                // Both halves of the entry count come from this register, so a
                // version that is not a chip's makes the count meaningless.
                if ver == 0x00 || ver == 0xFF {
                    return Err(format!("I/O APIC version {ver:#04x} is a floating bus: {unit:?}"));
                }
                let (range, masked) = unit
                    .split_once(" gsi ")
                    .and_then(|(_, rest)| rest.split_once(" masked "))
                    .ok_or_else(|| format!("unreadable I/O APIC line: {unit:?}"))?;
                let (lo, hi) = range
                    .split_once("..")
                    .ok_or_else(|| format!("no GSI range in {unit:?}"))?;
                let lo: u32 = lo.trim().parse().map_err(|_| format!("bad GSI base in {unit:?}"))?;
                let hi: u32 = hi.trim().parse().map_err(|_| format!("bad GSI top in {unit:?}"))?;
                let (n, total) = masked
                    .trim()
                    .split_once('/')
                    .ok_or_else(|| format!("no mask count in {unit:?}"))?;
                let n: u32 = n.parse().map_err(|_| format!("bad mask count in {unit:?}"))?;
                let total: u32 = total
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .parse()
                    .map_err(|_| format!("bad entry count in {unit:?}"))?;
                // `hi` is printed as `lo + total - 1`, so comparing them is a
                // tautology. What is checkable is the bound the driver refuses
                // past — a floating bus reports 256 here.
                if hi < lo || !(1..=240).contains(&total) {
                    return Err(format!(
                        "I/O APIC claims gsi {lo}..{hi}, {total} entries — not a redirection table: {unit:?}"
                    ));
                }
                covered.push((lo, hi));
                // The whole reason this driver runs before the first sti: an
                // entry firmware left armed at a vector with no gate is a #GP
                // that kills the boot.
                if n != total {
                    return Err(format!(
                        "{n} of {total} redirection entries masked — {} left armed: {unit:?}",
                        total - n
                    ));
                }
            }
            // Independent of any number the log derived from another: the two
            // pins the i8042 needs have to fall inside some unit's range, or
            // `route` returns `NoUnit` and there is no PS/2 input at all.
            for gsi in [1u32, 12] {
                if !covered.iter().any(|&(lo, hi)| (lo..=hi).contains(&gsi)) {
                    return Err(format!(
                        "no I/O APIC covers GSI {gsi}; units cover {covered:?}"
                    ));
                }
            }
            // IRQ 1 and IRQ 12 must be uncovered by the override table, or
            // the i8042 driver's identity assumption is wrong on this machine.
            let Some(isos) = log
                .lines()
                .find_map(|l| l.split("ioapic: iso bus:irq->gsi [").nth(1))
                .and_then(|r| r.split(']').next())
            else {
                return Err(format!("no `ioapic: iso` line in the boot log:\n{log}"));
            };
            // q35 always overrides at least IRQ 0, so an empty table means the
            // parse found nothing rather than that the machine has nothing.
            if isos.is_empty() {
                return Err(format!("the override table is empty; q35 always has IRQ 0:\n{log}"));
            }
            eprintln!("  [ioapic] {} unit(s), overrides {isos}", units.len());
            Ok(())
        }
        "input_merge" => {
            // The check runs in the kernel and panics on mismatch, so a
            // failure arrives as a dead boot; the marker is the only proof it
            // ran at all.
            let qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    kernel_features: &["test-input-merge"],
                    ..Default::default()
                },
            );
            let log = qemu.boot_log();
            if !log.contains("input-merge: ok") {
                return Err(format!("the input core check never reported:\n{log}"));
            }
            Ok(())
        }
        "i8042_keyboard" => {
            // On metal-sim, because that is the machine the driver is for and
            // the absent USB HID is what makes the test measure anything:
            // QEMU routes injected keys to one handler per device class, and
            // with a usb-kbd present that handler is not the PS/2 one.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                kernel_features: &["i8042-trace"],
                ..Default::default()
            };
            metal_sim_argv_check(&qemu::profile_argv(&options))?;
            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);
            let boot = qemu.boot_log().to_string();
            if !boot.contains("i8042: kbd set2+xlat (readback 0x41)") {
                return Err(format!("the PS/2 keyboard never came up:\n{boot}"));
            }

            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                "===I8042_READY===",
                |socket| {
                    for key in ["h", "e", "l", "l", "o"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                    qemu::qmp_send_keys(
                        socket,
                        &[("shift", true), ("b", true), ("b", false), ("shift", false)],
                    );
                    thread::sleep(Duration::from_millis(20));
                    for key in ["left", "esc"] {
                        qemu::qmp_send_keys(socket, &[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                    // A modifier on its own, so a stuck one is visible.
                    qemu::qmp_send_keys(socket, &[("shift", true)]);
                    thread::sleep(Duration::from_millis(20));
                    qemu::qmp_send_keys(socket, &[("shift", false)]);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }

            let events = parse_key_events(&result.stdout);
            if events.is_empty() {
                return Err(format!("no key event reached userland:\n{}", result.stdout));
            }
            // Presses spell the injected text: IRQ delivery, set-1 decode,
            // the HID mapping, the shared translate/layout path, and arrival
            // in a userland process, in one assertion.
            let typed: String = events
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!("typed {typed:?}, want it to contain \"hello\""));
            }
            if !typed.contains('B') {
                return Err(format!("typed {typed:?} — Shift+b did not produce a capital"));
            }
            if !typed.contains("\u{1b}[D") {
                return Err(format!("typed {typed:?} — Left arrow produced no escape sequence"));
            }
            for want in [0x29u8, 0x50, 0xE1] {
                if !events.iter().any(|e| e.usage == want) {
                    return Err(format!("no event for HID usage {want:#04x} in {events:?}"));
                }
            }
            // Every press is matched by a release.
            for usage in [0x0Bu8, 0x08, 0x0F, 0x12, 0x05, 0x29, 0x50, 0xE1] {
                let presses = events.iter().filter(|e| e.usage == usage && e.modifiers & 0x10 == 0).count();
                let releases = events.iter().filter(|e| e.usage == usage && e.modifiers & 0x10 != 0).count();
                if presses == 0 || presses != releases {
                    return Err(format!(
                        "usage {usage:#04x}: {presses} presses, {releases} releases"
                    ));
                }
            }
            // Nothing is left held: the bare Shift came back up.
            let last = events.last().unwrap();
            if last.modifiers & !0x10 != 0 {
                return Err(format!("a modifier is stuck down: last event {last:?}"));
            }
            // And they came from the i8042, not from somewhere else.
            let drained: usize = qemu
                .boot_log()
                .lines()
                .chain(result.serial.lines())
                .filter_map(trace_keys)
                .filter(|&k| k > 0)
                .sum();
            if drained == 0 {
                return Err("no i8042 drain reported a key event".to_string());
            }
            eprintln!("  [i8042] {} events to userland, {drained} from the driver", events.len());
            Ok(())
        }
        "i8042_no_spurious_wake" => {
            // The direct regression for the readiness defect: a stimulus that
            // produces bytes and no events must produce no wake. Pause is
            // that stimulus — six bytes, deliberately swallowed.
            //
            // It drives the same in-guest reader as `i8042_keyboard`, and not
            // only for the userland half of the assertion: on a fully idle
            // machine the kernel's log ring flushes one line behind, so the
            // last trace line would never reach the console (filed in
            // known-issues). A guest polling its fd keeps the ring moving.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    kernel_features: &["i8042-trace"],
                    ..Default::default()
                },
            );
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(20),
                "===I8042_READY===",
                |socket| {
                    for _ in 0..2 {
                        qemu::qmp_send_keys(socket, &[("pause", true), ("pause", false)]);
                        thread::sleep(Duration::from_millis(50));
                        qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                        thread::sleep(Duration::from_millis(50));
                    }
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }

            let mut zero_event_drains = 0;
            let mut key_drains = 0;
            for line in result.serial.lines() {
                let Some(keys) = trace_keys(line) else { continue };
                let woke = line.contains("woke_kb=1");
                if keys == 0 {
                    zero_event_drains += 1;
                    if woke {
                        return Err(format!("a drain with no events woke the queue: {line}"));
                    }
                } else {
                    key_drains += 1;
                    if !woke {
                        return Err(format!("a drain with events did not wake the queue: {line}"));
                    }
                }
            }
            if zero_event_drains == 0 {
                return Err(format!(
                    "no drain produced zero events — the stimulus never landed:\n{}",
                    result.serial
                ));
            }
            if key_drains == 0 {
                return Err(format!("no drain produced any event:\n{}", result.serial));
            }
            // And the swallowed bytes stayed swallowed all the way out.
            let events = parse_key_events(&result.stdout);
            if events.iter().any(|e| e.usage == 0x48) {
                return Err(format!("Pause reached userland as a key: {events:?}"));
            }
            if !events.iter().any(|e| e.usage == 0x04) {
                return Err(format!("the real key never arrived: {events:?}"));
            }
            eprintln!(
                "  [i8042] {zero_event_drains} zero-event drains, none woke; {key_drains} real ones, all did"
            );
            Ok(())
        }
        "i8042_mouse" => {
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    kernel_features: &["i8042-trace"],
                    ..Default::default()
                },
            );
            let boot = qemu.boot_log().to_string();
            if !boot.contains("i8042: aux rate=100") {
                return Err(format!("the TrackPoint path never came up:\n{boot}"));
            }

            const BURST: usize = 1000;
            let result = qemu.run_test_hooked(
                "test_rs_i8042_mouse",
                Duration::from_secs(30),
                "===I8042_MOUSE_READY===",
                |socket| {
                    let mut input = qemu::QmpInput::open(socket);
                    // Off the origin first: the position clamps at 0, so a
                    // move up from there would be invisible.
                    input.mouse(100, 100, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(40, -30, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(0, 0, Some(("left", true)));
                    thread::sleep(Duration::from_millis(50));
                    input.mouse(0, 0, Some(("left", false)));
                    thread::sleep(Duration::from_millis(100));
                    // One command per packet, because QEMU syncs input once
                    // per command: 1000 commands is 1000 packets, 3000 bytes
                    // through the framer.
                    for i in 0..BURST {
                        input.mouse(if i % 2 == 0 { 1 } else { -1 }, 0, None);
                    }
                    thread::sleep(Duration::from_millis(200));
                    input.mouse(0, 0, Some(("left", true)));
                    thread::sleep(Duration::from_millis(50));
                    input.mouse(0, 0, Some(("left", false)));
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }

            let events = parse_mouse_events(&result.stdout);
            if events.len() < BURST / 2 {
                return Err(format!(
                    "only {} pointer events reached userland, want at least {}",
                    events.len(),
                    BURST / 2
                ));
            }
            // A sign error in dy is invisible to any test that only checks
            // "it moved", and the PS/2 wire points the opposite way to the
            // screen — so both directions are asserted separately.
            if !events.windows(2).any(|w| w[1].x > w[0].x) {
                return Err("the pointer never moved right".to_string());
            }
            if !events.windows(2).any(|w| w[1].y < w[0].y) {
                return Err(format!(
                    "the pointer never moved up — dy inverted? ys: {:?}",
                    events.iter().take(8).map(|e| e.y).collect::<Vec<_>>()
                ));
            }
            // PS/2 bit 0 is left, and so is HID boot-mouse bit 0.
            if !events.iter().any(|e| e.buttons == 0x01) {
                return Err(format!(
                    "no left-button-down event; buttons seen: {:?}",
                    events.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()
                ));
            }
            // And after 3000 bytes of packets the framer is still aligned:
            // the last click is reported as a click, not as motion or as the
            // wrong button.
            let last_press = events.iter().rposition(|e| e.buttons == 0x01);
            let Some(last_press) = last_press else {
                return Err("no button press at all".to_string());
            };
            if events[last_press..].last().map(|e| e.buttons) != Some(0x00) {
                return Err(format!(
                    "framing drifted: after the final click the button state is {:?}",
                    events.last()
                ));
            }
            eprintln!(
                "  [i8042] {} pointer events, last button state {:#04x}",
                events.len(),
                events.last().unwrap().buttons
            );
            Ok(())
        }
        "i8042_absent" => {
            // A/B in one session: the guest's own `Boot: complete (Nms)` is
            // the instrument, because host-side timing here is dominated by
            // image builds. A wait-loop bug that costs a second on a machine
            // with a controller costs a minute on one without.
            let with = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions { profile: qemu::Profile::Metal, ..Default::default() },
            );
            let with_log = with.boot_log().to_string();
            let with_ms = boot_millis(&with_log)
                .ok_or_else(|| format!("no `Boot: complete` line:\n{with_log}"))?;
            drop(with);

            let without = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    i8042: false,
                    ..Default::default()
                },
            );
            let log = without.boot_log().to_string();
            // Reaching the ready marker at all is most of the assertion: the
            // boot got past a controller that answers nothing.
            let Some(absent) = log.lines().find(|l| l.contains("i8042: absent")) else {
                return Err(format!("no `i8042: absent` line on a machine with no i8042:\n{log}"));
            };
            // Measured: `-machine q35,i8042=off` also clears the FADT
            // IAPC_BOOT_ARCH 8042 bit, which was unverified when this driver
            // was designed. So the gate must be what fires — any other
            // absence line means the kernel probed 0x60/0x64 on a machine
            // that may have something else decoding them.
            if !absent.contains("iapc_boot_arch") {
                return Err(format!(
                    "the FADT gate did not fire; the kernel touched the ports instead: {absent}"
                ));
            }
            let without_ms = boot_millis(&log)
                .ok_or_else(|| format!("no `Boot: complete` line:\n{log}"))?;
            if without_ms > with_ms + 1000 {
                return Err(format!(
                    "boot took {without_ms}ms without an i8042 and {with_ms}ms with one — a wait is not bounded"
                ));
            }
            // Which line it is settles empirically whether QEMU also clears
            // the FADT bit, which was unverified when this was designed.
            eprintln!("  [i8042] absent via: {}", absent.trim());
            eprintln!("  [i8042] boot {without_ms}ms without vs {with_ms}ms with");
            Ok(())
        }
        "i8042_quarantine" => {
            // A controller producing bytes faster than the ISR's bound can
            // drain them is the one case the bound alone still lets livelock
            // a CPU. It must cost a keyboard, not a CPU.
            let mut qemu = QemuInstance::boot_with_options(
                test_config,
                c_bins,
                rust_bins,
                BootOptions {
                    profile: qemu::Profile::Metal,
                    qmp: true,
                    kernel_features: &["i8042-fault"],
                    ..Default::default()
                },
            );
            if !qemu.boot_log().contains("i8042: fault injection armed") {
                return Err(format!(
                    "the fault was never armed — did init fail?\n{}",
                    qemu.boot_log()
                ));
            }
            // The in-guest reader keeps a CPU doing work, so a livelocked
            // one is visible as a dead test rather than as a quiet pass.
            let result = qemu.run_test_hooked(
                "test_rs_i8042_keyboard",
                Duration::from_secs(30),
                "===I8042_READY===",
                |socket| {
                    qemu::qmp_send_keys(socket, &[("a", true), ("a", false)]);
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("the guest did not survive the wedge: {err}"));
            }
            let Some(line) = result.serial.lines().find(|l| l.contains("i8042: quarantined"))
            else {
                return Err(format!("no quarantine line:\n{}", result.serial));
            };
            // The count the driver actually achieved, not the word "masked"
            // in a format string: a quarantine that does not take the line
            // down leaves the CPU exposed to the next flood.
            let masked: u32 = line
                .split("masked=")
                .nth(1)
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| format!("unreadable quarantine line: {line}"))?;
            if masked == 0 {
                return Err(format!("quarantined without masking any line: {line}"));
            }
            // "A keyboard, not a CPU" is the claim, so measure the CPU. The
            // idle loop logs its health every 1000 iterations and halts when
            // there is nothing to do, so a spinning CPU is loud: the first
            // version of this driver left the `irq_ring` record undrained
            // after quarantine and produced 2685 of these lines in 5 s,
            // against 1 on a healthy run.
            let health = result.serial.matches("sched: cpu=").count();
            if health > 50 {
                return Err(format!(
                    "{health} idle-health lines after the quarantine — a CPU is spinning, not halting"
                ));
            }
            eprintln!("  [i8042] {}", line.trim());
            eprintln!("  [i8042] {health} idle-health lines — the CPU still halts");
            Ok(())
        }
        "metal_sim_input" => {
            // M2's exit criterion, on the machine shape and the kernel that
            // get flashed: no virtio device, no USB HID — so the i8042 is the
            // guest's only input device — and no kernel feature turned on for
            // the occasion, unlike the four tests above it.
            //
            // What it asserts is the events, read by an in-guest process and
            // printed. The first version asserted screen pixels after a click
            // at a fixed taskbar coordinate, which made the compositor's
            // layout part of a kernel-delivery criterion and needed thresholds
            // to survive the taskbar's own once-a-second repaint. M2 owns
            // delivery — pin to userland process — so that is what this
            // measures, and nothing here says the compositor reacted.
            // `metal_sim_compositor` is what covers the compositor.
            let options = BootOptions {
                profile: qemu::Profile::Metal,
                qmp: true,
                ..Default::default()
            };
            let argv = qemu::profile_argv(&options);
            metal_sim_argv_check(&argv)?;
            if argv.iter().any(|a| a.contains("i8042=off")) {
                return Err("metal-sim turned the i8042 off".to_string());
            }

            let mut qemu =
                QemuInstance::boot_with_options(test_config, c_bins, rust_bins, options);

            // `kernel/src/mouse.rs` scales each relative count into the
            // 0..32767 space the compositor consumes, so one PS/2 count
            // arrives as this many units of reported position.
            const REL_SCALE: i32 = 64;
            const DX: i32 = 40;
            const DY: i32 = -30;
            let result = qemu.run_test_hooked(
                "test_rs_input_events",
                Duration::from_secs(30),
                "===INPUT_READY===",
                |socket| {
                    // One connection for both halves: QEMU serves one QMP
                    // client at a time.
                    let mut input = qemu::QmpInput::open(socket);
                    // Off the origin first — the accumulated position clamps
                    // at 0, so a move up or left from there is invisible.
                    // Under 256 counts, or the packet's overflow bit is set
                    // and the motion is dropped by design.
                    input.mouse(200, 200, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(DX, DY, None);
                    thread::sleep(Duration::from_millis(100));
                    input.mouse(0, 0, Some(("left", true)));
                    thread::sleep(Duration::from_millis(50));
                    input.mouse(0, 0, Some(("left", false)));
                    thread::sleep(Duration::from_millis(100));
                    for key in ["h", "e", "l", "l", "o"] {
                        input.keys(&[(key, true), (key, false)]);
                        thread::sleep(Duration::from_millis(20));
                    }
                },
            );
            if let Some(err) = &result.error {
                return Err(format!("{err}\n{}", result.stdout));
            }

            let keys = parse_key_events(&result.stdout);
            let typed: String = keys
                .iter()
                .filter(|e| e.modifiers & 0x10 == 0)
                .map(|e| e.translated.as_str())
                .collect();
            if !typed.contains("hello") {
                return Err(format!(
                    "typed {typed:?}, want it to contain \"hello\" — the keyboard never reached userland:\n{}",
                    result.stdout
                ));
            }

            let pointer = parse_mouse_events(&result.stdout);
            // The delta the wire carried, not "it moved": a sign error in dy
            // and a dropped high bit both survive "it moved", and the PS/2
            // wire points the opposite way to the screen. Relative, so it
            // says nothing about where any compositor would draw a cursor.
            let want = (DX * REL_SCALE, DY * REL_SCALE);
            let deltas: Vec<(i32, i32)> = pointer
                .windows(2)
                .map(|w| (w[1].x as i32 - w[0].x as i32, w[1].y as i32 - w[0].y as i32))
                .collect();
            if !deltas.contains(&want) {
                return Err(format!(
                    "no pointer event moved by {want:?}; deltas seen: {deltas:?}\n{}",
                    result.stdout
                ));
            }
            let Some(down) = pointer.iter().position(|e| e.buttons == 0x01) else {
                return Err(format!(
                    "no left-button-down event; buttons seen: {:?}",
                    pointer.iter().map(|e| e.buttons).collect::<std::collections::BTreeSet<_>>()
                ));
            };
            if !pointer[down + 1..].iter().any(|e| e.buttons == 0x00) {
                return Err(format!(
                    "the left button went down and never came up: {pointer:?}"
                ));
            }
            eprintln!(
                "  [metal-sim] {} key events (typed {typed:?}), {} pointer events, delta {want:?} delivered",
                keys.len(),
                pointer.len()
            );
            Ok(())
        }
        other => Err(format!("unknown input test {other}")),
    }
}

#[derive(Debug)]
struct KeyLine {
    usage: u8,
    modifiers: u8,
    translated: String,
}

/// `kev usage=0x04 mods=0x00 tr="a"` — what the in-guest reader prints.
fn parse_key_events(stdout: &str) -> Vec<KeyLine> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.split("kev usage=0x").nth(1)?;
            let (usage, rest) = rest.split_once(" mods=0x")?;
            let (modifiers, rest) = rest.split_once(" tr=")?;
            let translated = rest.trim().trim_matches('"');
            Some(KeyLine {
                usage: u8::from_str_radix(usage, 16).ok()?,
                modifiers: u8::from_str_radix(modifiers, 16).ok()?,
                translated: unescape(translated),
            })
        })
        .collect()
}

/// The guest prints through `{:?}`, so an escape sequence arrives as the
/// four characters `\u{1b}` rather than the byte.
fn unescape(s: &str) -> String {
    s.replace("\\u{1b}", "\u{1b}").replace("\\\"", "\"").replace("\\\\", "\\")
}

#[derive(Debug)]
struct MouseLine {
    buttons: u8,
    x: u16,
    y: u16,
}

/// `mev buttons=0x01 x=6400 y=6400` — what the in-guest reader prints.
fn parse_mouse_events(stdout: &str) -> Vec<MouseLine> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.split("mev buttons=0x").nth(1)?;
            let (buttons, rest) = rest.split_once(" x=")?;
            let (x, y) = rest.split_once(" y=")?;
            Some(MouseLine {
                buttons: u8::from_str_radix(buttons, 16).ok()?,
                x: x.parse().ok()?,
                y: y.trim().parse().ok()?,
            })
        })
        .collect()
}

/// The guest's own boot duration, out of `Boot: complete (123ms)`.
fn boot_millis(log: &str) -> Option<u64> {
    log.lines()
        .find_map(|l| l.split("Boot: complete (").nth(1))?
        .split("ms)")
        .next()?
        .parse()
        .ok()
}

/// The `keys=` field of an `i8042: drain ...` trace line.
fn trace_keys(line: &str) -> Option<usize> {
    line.split("i8042: drain ")
        .nth(1)?
        .split("keys=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
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
        for name in MACHINE_TESTS {
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
    let machine_to_run: Vec<&str> = MACHINE_TESTS
        .iter()
        .copied()
        .filter(|n| filter.map_or(true, |f| n.contains(f)))
        .collect();

    if tests_to_run.is_empty()
        && audio_to_run.is_empty()
        && screen_to_run.is_empty()
        && machine_to_run.is_empty()
    {
        eprintln!("No tests match filter {:?}", filter);
        std::process::exit(1);
    }

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let total = tests_to_run.len()
        + audio_to_run.len() * AUDIO_SMP.len()
        + screen_to_run.len()
        + machine_to_run.len();
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

    // These own their QEMU because their machine shape is the test.
    // MACHINE_TESTS keeps the plain-kernel ones first for the same reason
    // SCREEN_TESTS does.
    if !machine_to_run.is_empty() {
        eprintln!("  --- machine ---");
        for name in &machine_to_run {
            let start = std::time::Instant::now();
            let outcome = catching(|| run_machine_test(name, &test_config, &c_bins, &rust_bins));
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
                    let summary = reason.lines().next().unwrap_or("machine check failed");
                    failures.push((name.to_string(), summary.to_string()));
                }
            }
        }
    }

    // Last, because they are the only tests that build a kernel with features
    // on. Three of them do, one feature each, so the block costs three kernel
    // rebuilds; running it here keeps every plain-kernel test above it out of
    // the thrash, and SCREEN_TESTS puts the three at the end for the same
    // reason.
    if !screen_to_run.is_empty() {
        eprintln!("  --- screen ---");
        for name in &screen_to_run {
            let start = std::time::Instant::now();
            let outcome = catching(|| run_screen_test(name, &test_config, &c_bins, &rust_bins));
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
