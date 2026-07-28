mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;

use common::qemu::{self, BootOptions, QemuInstance, TestResult};
use common::{audio, compile};

// ---------------------------------------------------------------------------
// Test definition
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Check functions
// ---------------------------------------------------------------------------

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
    ok
}

/// Select check function by test name convention.
fn check_for(name: &str) -> fn(&TestResult) -> bool {
    match name {
        "panic_recovery" => check_panic_recovery,
        _ => check_rust_result,
    }
}

// ---------------------------------------------------------------------------
// Audio glitch tests
// ---------------------------------------------------------------------------

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
}

type AudioBaseline = BTreeMap<String, BTreeMap<String, AudioBaselineEntry>>;

struct ConfigBaseline {
    gaps: BTreeMap<u32, u32>,
    counters: audio::CounterLimits,
}

fn load_audio_baseline() -> AudioBaseline {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/audio-baseline.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Baseline for one (test, smp) config. Every config must be recorded: an
/// ungated config would pass by omission.
fn config_baseline(baseline: &AudioBaseline, name: &str, smp: u32) -> ConfigBaseline {
    let entry = baseline
        .get(name)
        .and_then(|per_smp| per_smp.get(&format!("smp{smp}")))
        .unwrap_or_else(|| panic!("audio-baseline.toml: no [{name}.smp{smp}] section"));
    ConfigBaseline {
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

/// Boot a fresh QEMU with the given CPU count, run one in-guest audio test,
/// then assert on two independent instruments: soundd's in-guest counters
/// (wake lateness, pipeline drains, periods of silence submitted) and the
/// captured wav (mid-signal silence must not regress the recorded histogram,
/// and there must be no hard sample-to-sample discontinuities).
fn run_audio_test(
    name: &str,
    smp: u32,
    baseline: &ConfigBaseline,
    test_config: &Path,
    c_bins: &[(String, Vec<u8>)],
    rust_bins: &[(String, Vec<u8>)],
) -> Result<(), String> {
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

    // The record gate A tracks across migration stages — always printed, so
    // every green run leaves comparable numbers in the log, including the ones
    // that pass.
    let gaps = audio::gap_histogram(&analysis, wav.sample_rate);
    let counters = audio::parse_soundd_counters(&serial)?;
    eprintln!(
        "        {name} smp={smp} gaps: {} (baseline {}) peak {} active {:.2}s dither {:.1}%",
        audio::format_histogram(&gaps),
        audio::format_histogram(&baseline.gaps),
        analysis.peak,
        secs(analysis.active_samples),
        analysis.dither_ratio.unwrap_or(0.0) * 100.0,
    );
    eprintln!(
        "        {name} smp={smp} soundd: wake_lat {}us ({:.2} pipelines, limit {}us) \
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

    let mut problems = Vec::new();
    if let Err(regression) = audio::check_counters(&counters, &baseline.counters) {
        problems.push(regression);
    }
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
    if let Err(regression) = audio::check_gap_regression(&gaps, &baseline.gaps) {
        let mut msg = format!(
            "{regression}\n      {} mid-signal underruns (silence >= 2ms inside the tone):",
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
        problems.push(msg);
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

    if problems.is_empty() {
        return Ok(());
    }

    // Keep the capture for post-mortem listening; the boot's Drop removes
    // the original path.
    let kept = qemu
        .audio_wav_path()
        .with_file_name(format!("failed-{name}-smp{smp}.wav"));
    let _ = fs::rename(qemu.audio_wav_path(), &kept);
    Err(format!(
        "{}\n    wav kept at {}",
        problems.join("\n    "),
        kept.display()
    ))
}

// ---------------------------------------------------------------------------
// Test registry
// ---------------------------------------------------------------------------

fn build_test_registry(
    rust_bins: &[(String, Vec<u8>)],
    c_names: &[String],
) -> Vec<TestDef> {
    let mut tests = Vec::new();

    // Rust tests first
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

    // Then C tests
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

// ---------------------------------------------------------------------------
// Debug mode
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let debug_mode = args.iter().any(|a| a == "--debug");
    let list_mode = args.iter().any(|a| a == "--list");
    let nocapture = args.iter().any(|a| a == "--nocapture" || a == "--show-output");

    if nocapture || debug_mode {
        common::qemu::VERBOSE.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Filter: first positional arg that isn't a flag
    let filter: Option<&str> = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| s.as_str());

    // Discover and compile C tests
    let c_names = discover_c_tests();
    eprintln!("[toyos] Compiling {} C tests...", c_names.len());
    let c_bins = compile_c_tests(&c_names);
    let c_compiled: Vec<String> = c_bins.iter().map(|(n, _)| n.clone()).collect();

    // Build Rust tests
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
        return;
    }

    if debug_mode {
        run_debug_mode(&c_bins, &rust_bins);
        return;
    }

    // Build registry and apply filter
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

    if tests_to_run.is_empty() && audio_to_run.is_empty() {
        eprintln!("No tests match filter {:?}", filter);
        std::process::exit(1);
    }

    let test_config = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testcases");
    let total = tests_to_run.len() + audio_to_run.len() * AUDIO_SMP.len();
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

    let suite_elapsed = suite_start.elapsed();

    // Summary
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
