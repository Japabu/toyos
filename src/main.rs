mod qemu;

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn check_prerequisites() {
    let mut missing = Vec::new();

    if Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .status()
        .is_err()
    {
        missing.push("git");
    }

    if Command::new("rustup")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_err()
    {
        missing.push("rustup (install from https://rustup.rs)");
    }

    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .status()
        .is_err()
    {
        missing.push("qemu-system-x86_64 (install QEMU)");
    }

    if !missing.is_empty() {
        eprintln!("Error: missing required tools:");
        for tool in &missing {
            eprintln!("  - {tool}");
        }
        std::process::exit(1);
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let debug = args.iter().any(|a| a == "--debug");
    let release = args.iter().any(|a| a == "--release");
    let build_only = args.iter().any(|a| a == "--build-only");
    let dump_audio = args.iter().any(|a| a == "--dump-audio");
    let rebuild_toolchain = args.iter().any(|a| a == "--rebuild-toolchain");
    let smp = parse_smp(&args);

    check_prerequisites();

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    env::set_current_dir(&root).expect("Failed to cd to project root");

    toyos_build::ensure_submodules(&root);

    // Ensure toolchain is up to date
    toyos_build::toolchain::ensure(&root, rebuild_toolchain);

    // Build everything
    toyos_build::build::build(&root, debug, release);
    println!("Build finished.");

    if !build_only {
        qemu::launch(debug, dump_audio, smp);
    }
}

/// `--smp N` sets the QEMU core count (default 8). `--smp 1` is the
/// single-CPU case the audio spec treats as first-class.
fn parse_smp(args: &[String]) -> u32 {
    let Some(pos) = args.iter().position(|a| a == "--smp") else {
        return 8;
    };
    let value = args
        .get(pos + 1)
        .unwrap_or_else(|| panic!("--smp requires a value"));
    let smp: u32 = value
        .parse()
        .unwrap_or_else(|_| panic!("invalid --smp value: {value:?}"));
    assert!(smp >= 1, "--smp must be at least 1");
    smp
}
