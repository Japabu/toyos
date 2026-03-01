use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

fn run(cmd: &mut Command) {
    let status = cmd.status().unwrap_or_else(|e| panic!("failed to run {:?}: {e}", cmd.get_program()));
    assert!(status.success(), "{:?} failed with {status}", cmd.get_program());
}

fn main() {
    let project_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tcc_dir = project_dir.join("tinycc");
    // ── Stage 0: Download TCC ───────────────────────────────────────────
    if !tcc_dir.join("tcc.c").exists() {
        println!("[stage 0] Downloading TinyCC...");
        download_tcc(&tcc_dir);
        println!("[stage 0] Done.");
    } else {
        println!("[stage 0] TinyCC already present, skipping download.");
    }

    // ── Build toyos-cc ──────────────────────────────────────────────────
    let toyos_cc_dir = project_dir.join("../toyos-cc");
    println!("[stage 1] Building toyos-cc...");
    run(Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&toyos_cc_dir));
    let toyos_cc = toyos_cc_dir.join("target/debug/toyos-cc");
    assert!(toyos_cc.exists(), "toyos-cc binary not found at {}", toyos_cc.display());

    // ── Stage 1: Compile TCC with toyos-cc ──────────────────────────────
    let stage1_obj = project_dir.join("tcc-stage1.o");
    let stage1_bin = project_dir.join("tcc-stage1");

    println!("[stage 1] Compiling TCC with toyos-cc...");
    run(Command::new(&toyos_cc)
        .args(["-c", "-o"])
        .arg(&stage1_obj)
        .args(tcc_defines())
        .arg("-I").arg(tcc_dir.join("include"))
        .arg("-I").arg(&tcc_dir)
        .args(system_include_args())
        .arg(tcc_dir.join("tcc.c"))
        .current_dir(&project_dir));

    println!("[stage 1] Linking tcc-stage1...");
    link(&stage1_obj, &stage1_bin);
    println!("[stage 1] Done.");

    // ── Stage 2: Compile TCC with stage1 TCC ────────────────────────────
    let stage2_obj = project_dir.join("tcc-stage2.o");
    let stage2_bin = project_dir.join("tcc-stage2");

    println!("[stage 2] Compiling TCC with tcc-stage1...");
    run(Command::new(&stage1_bin)
        .args(["-c", "-o"])
        .arg(&stage2_obj)
        .args(tcc_defines())
        .arg("-I").arg(&tcc_dir)
        .arg(tcc_dir.join("tcc.c"))
        .current_dir(&project_dir));

    println!("[stage 2] Linking tcc-stage2...");
    link(&stage2_obj, &stage2_bin);
    println!("[stage 2] Done.");

    // ── Verify ──────────────────────────────────────────────────────────
    println!("[verify] Running tcc-stage2 --help...");
    let output = Command::new(&stage2_bin)
        .arg("--help")
        .output()
        .expect("failed to run tcc-stage2");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "tcc-stage2 --help failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));

    println!("[done] Bootstrapped TCC at {}", stage2_bin.display());
}

fn download_tcc(dest: &Path) {
    let url = "https://repo.or.cz/tinycc.git/snapshot/HEAD.tar.gz";
    let response = ureq::get(url).call().unwrap_or_else(|e| panic!("failed to download TCC: {e}"));

    let mut body = Vec::new();
    response.into_reader().read_to_end(&mut body).expect("failed to read response");

    let decoder = flate2::read::GzDecoder::new(&body[..]);
    let mut archive = tar::Archive::new(decoder);

    let tmp = dest.with_file_name(".tinycc-tmp");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).ok();
    }
    fs::create_dir_all(&tmp).expect("failed to create temp dir");
    archive.unpack(&tmp).expect("failed to extract tarball");

    // The tarball extracts to a single directory with a hash name — find it and rename
    let inner = fs::read_dir(&tmp)
        .expect("failed to read temp dir")
        .filter_map(|e| e.ok())
        .find(|e| e.path().is_dir())
        .expect("tarball contained no directory")
        .path();

    fs::rename(&inner, dest).expect("failed to rename extracted directory");
    fs::remove_dir_all(&tmp).ok();

    // Generate config.h — normally produced by ./configure
    fs::write(dest.join("config.h"), "\
#define CONFIG_TCC_PREDEFS 0\n\
#define GCC_MAJOR 4\n\
#define GCC_MINOR 0\n\
").expect("failed to write config.h");
}

fn tcc_defines() -> Vec<&'static str> {
    vec![
        "-DONE_SOURCE=1",
        "-DTCC_TARGET_X86_64",
        "-DCONFIG_TRIPLET=\"x86_64-linux-gnu\"",
        "-DTCC_VERSION=\"0.9.27\"",
        "-DCONFIG_TCCDIR=\"/usr/local/lib/tcc\"",
        "-DCONFIG_TCC_CRTPREFIX=\"/usr/lib\"",
        "-DCONFIG_TCC_LIBPATHS=\"/usr/lib\"",
        "-DCONFIG_TCC_SYSINCLUDEPATHS=\"/usr/include\"",
        "-DCONFIG_LDDIR=\"lib\"",
        // HACK: defining as 0 still makes #ifdef true in tcc.h, activating
        // comma-expression wrappers. But without it, tcc.h defaults to 1 which
        // pulls in <dispatch/dispatch.h> on macOS — unparseable by toyos-cc.
        "-DCONFIG_TCC_SEMLOCK=0",
    ]
}

fn system_include_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        let output = Command::new("xcrun")
            .args(["--show-sdk-path"])
            .output()
            .expect("failed to run xcrun");
        let sdk = String::from_utf8(output.stdout).unwrap().trim().to_string();
        vec!["-I".to_string(), format!("{sdk}/usr/include")]
    } else {
        vec!["-I".to_string(), "/usr/include".to_string()]
    }
}

fn link(obj: &Path, out: &Path) {
    let mut cmd = Command::new("cc");
    cmd.arg("-o").arg(out).arg(obj);

    if cfg!(target_os = "linux") {
        cmd.args(["-lm", "-ldl"]);
    } else if cfg!(target_os = "macos") {
        cmd.arg("-lm");
    }

    run(&mut cmd);
}
