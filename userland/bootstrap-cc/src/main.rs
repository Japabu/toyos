use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

fn run(cmd: &mut Command) {
    let status = cmd.status().expect("failed to run command");
    assert!(status.success());
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tcc_dir = root.join("tinycc");

    write_minimal_config(&tcc_dir);

    // ── Build toyos-cc and toyos-ld ──────────────
    let toyos_cc_dir = root.join("../toyos-cc");
    let toyos_ld_dir = root.join("../toyos-ld");
    run(Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&toyos_cc_dir));
    run(Command::new("cargo")
        .args(["build", "--quiet"])
        .current_dir(&toyos_ld_dir));

    let toyos_cc = toyos_cc_dir.join("target/debug/toyos-cc");
    let toyos_ld = toyos_ld_dir.join("target/debug/toyos-ld");

    // ── Stage 1: compile with toyos-cc, link with toyos-ld ───────────
    let stage1_obj = root.join("tcc-stage1.o");
    let stage1_bin = root.join("tcc-stage1");

    println!("[stage1] compiling with toyos-cc");

    run(Command::new(&toyos_cc)
        .arg("-c")
        .arg("-DONE_SOURCE=1")
        .args(system_include_args())
        .arg("-o").arg(&stage1_obj)
        .arg("tcc.c")
        .current_dir(&tcc_dir));

    println!("[stage1] linking with toyos-ld");

    run(Command::new(&toyos_ld)
        .arg("--macho")
        .arg("-e").arg("_main")
        .arg("-o").arg(&stage1_bin)
        .arg(&stage1_obj));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stage1_bin, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // ── Stage 2: self-host with TCC ──────────────
    let stage2 = root.join("tcc-stage2");

    println!("[stage2] self-hosting");

    run(Command::new(&stage1_bin)
        .arg("-o").arg(&stage2)
        .arg("tcc.c")
        .arg("-one-source")
        .current_dir(&tcc_dir));

    if cfg!(target_os = "macos") {
        run(Command::new("codesign")
            .args(["--sign", "-"])
            .arg(&stage2));
    }

    println!("Bootstrapped TCC: {}", stage2.display());
}


fn write_minimal_config(dir: &Path) {
    fs::write(dir.join("config.h"), r#"
#define TCC_VERSION "0.9.27"
#define CONFIG_TCC_PREDEFS 1
#define CONFIG_TCC_SEMLOCK 0
#define GCC_MAJOR 4
#define GCC_MINOR 0
"#).unwrap();
}


fn sdk_path() -> String {
    let output = Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
        .expect("failed to run xcrun");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn system_include_args() -> Vec<String> {
    if cfg!(target_os = "macos") {
        vec!["-I".to_string(), format!("{}/usr/include", sdk_path())]
    } else {
        panic!("not implemented")
    }
}