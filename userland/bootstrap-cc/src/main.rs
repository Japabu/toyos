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
    configure_tcc(&tcc_dir);

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
        .args(tcc_defines(&tcc_dir))
        .arg("-I").arg(tcc_dir.join("include"))
        .arg("-I").arg(&tcc_dir)
        .args(system_include_args())
        .arg(tcc_dir.join("tcc.c"))
        .current_dir(&project_dir));

    println!("[stage 1] Linking tcc-stage1...");
    link(&stage1_obj, &stage1_bin);
    println!("[stage 1] Done.");

    // ── Build libtcc1.a with stage1 TCC ─────────────────────────────────
    // TCC needs libtcc1.a at link time. Must be compiled by TCC itself
    // because TCC's object format may differ from the host compiler's.
    println!("[stage 2] Building libtcc1.a...");
    build_libtcc1(&tcc_dir);

    // ── Stage 2: Compile TCC with stage1 TCC ────────────────────────────
    // TCC links its own objects — compile and link in one step.
    // -L$TCCDIR so TCC can find libtcc1.a.
    let stage2_bin = project_dir.join("tcc-stage2");

    println!("[stage 2] Compiling TCC with tcc-stage1...");
    run(Command::new(&stage1_bin)
        .arg("-o").arg(&stage2_bin)
        .args(tcc_defines(&tcc_dir))
        .arg("-I").arg(tcc_dir.join("include"))
        .arg("-I").arg(&tcc_dir)
        .arg("-L").arg(&tcc_dir)
        .arg(tcc_dir.join("tcc.c"))
        .current_dir(&project_dir));

    // On macOS, binaries linked by TCC are not codesigned and will be SIGKILLed.
    if cfg!(target_os = "macos") {
        println!("[stage 2] Codesigning tcc-stage2...");
        run(Command::new("codesign").args(["--sign", "-"]).arg(&stage2_bin));
    }
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
}

fn configure_tcc(tcc_dir: &Path) {
    // Generate config.h — normally produced by ./configure
    // CONFIG_TCC_PREDEFS=1: bake tccdefs into the TCC binary via tccdefs_.h
    // CONFIG_TCC_SEMLOCK=0: defined so #ifdef is true (tcc.h needs it), but #if is false
    //   (avoids pulling in <dispatch/dispatch.h> on macOS)
    fs::write(tcc_dir.join("config.h"), "\
#define CONFIG_TCC_PREDEFS 1\n\
#define GCC_MAJOR 4\n\
#define GCC_MINOR 0\n\
").expect("failed to write config.h");

    // Build c2str and generate tccdefs_.h (string-ified predefs baked into TCC)
    let c2str = tcc_dir.join("c2str");
    run(Command::new("cc")
        .args(["-DC2STR", "-o"])
        .arg(&c2str)
        .arg(tcc_dir.join("conftest.c")));
    run(Command::new(&c2str)
        .arg(tcc_dir.join("include/tccdefs.h"))
        .arg(tcc_dir.join("tccdefs_.h")));
    fs::remove_file(&c2str).ok();
}

fn tcc_defines(tcc_dir: &Path) -> Vec<String> {
    let mut defs = vec![
        "-DONE_SOURCE=1".into(),
        "-DTCC_VERSION=\"0.9.27\"".into(),
        format!("-DCONFIG_TCCDIR=\"{}\"", tcc_dir.display()),
        "-DCONFIG_LDDIR=\"lib\"".into(),
        // Defining as 0 still makes #ifdef true in tcc.h (activating comma-expression
        // wrappers), but #if is false — avoids pulling in <dispatch/dispatch.h> on macOS.
        "-DCONFIG_TCC_SEMLOCK=0".into(),
    ];

    if cfg!(target_os = "macos") {
        let sdk = sdk_path();
        defs.push("-DTCC_TARGET_MACHO".into());
        defs.push("-DCONFIG_NEW_MACHO=1".into());
        defs.push(format!("-DCONFIG_TCC_CRTPREFIX=\"{sdk}/usr/lib\""));
        defs.push(format!("-DCONFIG_TCC_LIBPATHS=\"{sdk}/usr/lib\""));
        defs.push(format!("-DCONFIG_TCC_SYSINCLUDEPATHS=\"{{B}}/include:{sdk}/usr/include\""));
        if cfg!(target_arch = "aarch64") {
            defs.push("-DTCC_TARGET_ARM64".into());
        } else {
            defs.push("-DTCC_TARGET_X86_64".into());
        }
    } else {
        // Linux
        defs.push("-DCONFIG_TCC_CRTPREFIX=\"/usr/lib\"".into());
        defs.push("-DCONFIG_TCC_LIBPATHS=\"/usr/lib\"".into());
        defs.push("-DCONFIG_TCC_SYSINCLUDEPATHS=\"{B}/include:/usr/include\"".into());
        if cfg!(target_arch = "aarch64") {
            defs.push("-DTCC_TARGET_ARM64".into());
            defs.push("-DCONFIG_TRIPLET=\"aarch64-linux-gnu\"".into());
        } else {
            defs.push("-DTCC_TARGET_X86_64".into());
            defs.push("-DCONFIG_TRIPLET=\"x86_64-linux-gnu\"".into());
        }
    }

    defs
}

fn sdk_path() -> String {
    let output = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("failed to run xcrun");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn build_libtcc1(tcc_dir: &Path) {
    // Build with the host cc — libtcc1.a is a standard Mach-O/ELF archive that TCC's
    // linker can consume. Using the host cc avoids TCC codegen limitations during bootstrap.
    let include_args: &[&str] = if cfg!(target_os = "macos") {
        &[]  // host cc knows its own includes
    } else {
        &[]
    };

    let libtcc1_o = tcc_dir.join("libtcc1.o");
    run(Command::new("cc").args(["-c", "-o"]).arg(&libtcc1_o)
        .args(include_args)
        .arg(tcc_dir.join("lib/libtcc1.c")));

    let mut objs = vec![libtcc1_o];

    if cfg!(target_arch = "aarch64") {
        let lib_arm_o = tcc_dir.join("lib-arm64.o");
        run(Command::new("cc").args(["-c", "-o"]).arg(&lib_arm_o)
            .args(include_args)
            .arg(tcc_dir.join("lib/lib-arm64.c")));
        objs.push(lib_arm_o);
    }

    run(Command::new("ar").args(["rcs"]).arg(tcc_dir.join("libtcc1.a")).args(&objs));
}

fn system_include_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec!["-I".to_string(), format!("{}/usr/include", sdk_path())]
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
