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
    let claim_sysroot = args.iter().any(|a| a == "--claim-sysroot");
    let smp = parse_smp(&args);
    let profile = parse_profile(&args);
    let mute = args.iter().any(|a| a == "--mute");
    // A machine with no serial port has the framebuffer and nothing else, and
    // the kernel stops painting it the moment userland claims it. `--diag-boot`
    // builds the image that never does; `--console-boot` builds the one that
    // claims it deliberately and puts a shell there, having first copied the
    // boot log into its scrollback.
    let diag = args.iter().any(|a| a == "--diag-boot");
    let console = args.iter().any(|a| a == "--console-boot");
    assert!(!(diag && console), "--diag-boot and --console-boot are two images; build one");
    let boot = match (diag, console) {
        (true, _) => toyos_build::build::Boot::Diag,
        (_, true) => toyos_build::build::Boot::Console,
        _ => toyos_build::build::Boot::Normal,
    };
    assert!(
        !(dump_audio && profile == qemu::Profile::Metal),
        "--dump-audio needs virtio-sound, which --metal-sim removes"
    );
    assert!(
        !(mute && profile != qemu::Profile::Metal),
        "--mute only means anything under --metal-sim; the others need their console"
    );

    check_prerequisites();

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    env::set_current_dir(&root).expect("Failed to cd to project root");

    if args.iter().any(|a| a == "--regen-font") {
        toyos_build::assets::regen_panic_font(&root);
        return;
    }

    if args.iter().any(|a| a == "--worktree") {
        toyos_build::worktree::dispatch(&root, &args);
        return;
    }

    // Only where the submodules belong. In a linked worktree `rust/` is an empty
    // stub and initialising it clones the whole rust history again, into a git
    // directory of its own that shares no objects with the one beside it.
    if matches!(toyos_build::toolchain::owner(&root), toyos_build::toolchain::Owner::Us) {
        toyos_build::ensure_submodules(&root);
    }

    // Toolchain included: `build` holds the build lock across both, so no other
    // agent's clean or bootstrap can land between the two.
    let image =
        toyos_build::build::build(&root, debug, release, boot, rebuild_toolchain, claim_sysroot);
    println!("Build finished.");
    println!("Boot image: {}", image.display());

    if !build_only {
        qemu::launch(&qemu::Options { debug, dump_audio, profile, smp, mute, image });
    }
}

/// `--gop` swaps virtio-gpu for a firmware framebuffer; `--metal-sim` goes
/// further and removes every virtio device, which is what the target laptop
/// actually presents. `--metal-sim --mute` additionally takes the 16550 away.
fn parse_profile(args: &[String]) -> qemu::Profile {
    let gop = args.iter().any(|a| a == "--gop");
    let metal = args.iter().any(|a| a == "--metal-sim");
    match (gop, metal) {
        (_, true) => qemu::Profile::Metal,
        (true, false) => qemu::Profile::Gop,
        (false, false) => qemu::Profile::Virtio,
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
