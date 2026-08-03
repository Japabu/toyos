use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::buildlock;
use crate::stamps;

/// Which x.py build a stale toolchain needs.
#[derive(Clone, Copy, PartialEq)]
enum Bootstrap {
    /// `invalidate_hosted` separates "the compiler changed" from "the rustup
    /// link is missing". Only the first makes the ToyOS-hosted rustc stale, and
    /// rebuilding that one costs minutes.
    Full { invalidate_hosted: bool },
    Std,
}

/// Ensure the toolchain is up to date.
///
/// Every step decides under the caller's shared lock and acts under the
/// exclusive one, so the common answer — nothing to do — costs no
/// serialisation, and two agents cannot both conclude the toolchain is stale
/// and both start `x.py build` in the same directory. That pair is what left a
/// half-written `librustc_driver` for cargo to probe, and cargo memoises a
/// failed probe (known-issues §6).
///
/// The steps are ordered, and each invalidates what it makes stale rather than
/// threading a `rebuilt` flag through: a step that decides for itself still
/// decides correctly when the process before it was killed halfway.
pub fn ensure(root: &Path, force_rebuild: bool, lock: &mut buildlock::Held) {
    let rust_dir = root.join("rust");
    let stamps_dir = root.join("target/stamps");
    fs::create_dir_all(&stamps_dir).ok();

    // Needed as the cross-linker for bootstrap and for every build.
    let ld_src = root.join("toyos-ld/src");
    let ld_stamp = stamps_dir.join("linker.stamp");
    lock.act_if(
        "build toyos-ld",
        || {
            (stamps::dir_changed(&ld_src, &ld_stamp) || !toyos_ld_binary(root).exists())
                .then_some(())
        },
        |()| {
            eprintln!("Building toyos-ld...");
            build_toyos_ld(root);
            stamps::write_dir_stamp(&ld_src, &ld_stamp);
        },
    );

    // Used as a host tool by doom's build.rs.
    let cc_src = root.join("toyos-cc/src");
    let cc_inc = root.join("toyos-cc/include");
    let cc_stamp = stamps_dir.join("toyos-cc.stamp");
    let cc_inc_stamp = stamps_dir.join("toyos-cc-include.stamp");
    lock.act_if(
        "build toyos-cc",
        || {
            (stamps::dir_changed(&cc_src, &cc_stamp)
                || stamps::dir_changed(&cc_inc, &cc_inc_stamp)
                || !toyos_cc_binary(root).exists())
            .then_some(())
        },
        |()| {
            eprintln!("Building toyos-cc...");
            build_toyos_cc(root);
            stamps::write_dir_stamp(&cc_src, &cc_stamp);
            stamps::write_dir_stamp(&cc_inc, &cc_inc_stamp);
        },
    );

    let compiler_stamp = stamps_dir.join("compiler.stamp");
    let std_stamp = stamps_dir.join("std.stamp");
    let abi_stamp = stamps_dir.join("abi.stamp");
    let net_stamp = stamps_dir.join("net.stamp");
    let hosted_stamp = stamps_dir.join("hosted-rustc.stamp");
    let libc_stamp = stamps_dir.join("toyos-libc.stamp");
    // toyos-abi and toyos are dependencies of std — changes require an std rebuild.
    let abi_src = root.join("toyos-abi/src");
    let net_src = root.join("toyos/src");
    lock.act_if(
        "build the rust toolchain",
        || {
            let toolchain_exists = Command::new("rustup")
                .args(["run", "toyos", "rustc", "--version"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if stamps::dir_changed(&rust_dir.join("compiler"), &compiler_stamp) || force_rebuild {
                Some(Bootstrap::Full { invalidate_hosted: true })
            } else if !toolchain_exists {
                Some(Bootstrap::Full { invalidate_hosted: false })
            } else if stamps::dir_changed(&rust_dir.join("library"), &std_stamp)
                || stamps::dir_changed(&abi_src, &abi_stamp)
                || stamps::dir_changed(&net_src, &net_stamp)
            {
                Some(Bootstrap::Std)
            } else {
                None
            }
        },
        |kind| {
            match kind {
                Bootstrap::Full { .. } => {
                    eprintln!("Building full toolchain (this takes a while on first run)...");
                    full_bootstrap(root, &rust_dir);
                    stamps::write_dir_stamp(&rust_dir.join("compiler"), &compiler_stamp);
                }
                Bootstrap::Std => {
                    eprintln!("Rebuilding std (fast path)...");
                    rebuild_std(root, &rust_dir);
                }
            }
            stamps::write_dir_stamp(&rust_dir.join("library"), &std_stamp);
            stamps::write_dir_stamp(&abi_src, &abi_stamp);
            stamps::write_dir_stamp(&net_src, &net_stamp);
            if kind == (Bootstrap::Full { invalidate_hosted: true }) {
                let _ = fs::remove_file(&hosted_stamp);
            }
            // The sysroot rlibs were replaced: toyos-libc's archive and its
            // cross artifacts were compiled against the ones that are gone, and
            // cargo judges them fresh against source that has not changed.
            let _ = fs::remove_file(&libc_stamp);
            let _ = fs::remove_dir_all(root.join("userland/libc/target/x86_64-unknown-toyos"));
        },
    );

    let hosted_rustc = rust_dir.join("build/x86_64-unknown-toyos/stage2/bin/rustc");
    lock.act_if(
        "build the ToyOS-hosted rustc",
        || (!hosted_stamp.exists() || !hosted_rustc.exists()).then_some(()),
        |()| {
            build_hosted_rustc(&rust_dir, &toyos_ld_binary(root));
            assert!(hosted_rustc.exists(), "Failed to build hosted rustc");
            fs::write(&hosted_stamp, "").unwrap();
        },
    );

    let stage2 = rust_dir.join(format!("build/{}/stage2", host_triple()));
    lock.act_if(
        "link the toyos rustup toolchain",
        || link_stale(&stage2).then_some(()),
        |()| run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]),
    );

    // Before any cargo build uses the toolchain, otherwise cargo may
    // fingerprint an incomplete sysroot on first run.
    lock.act_if(
        "add the host target to the ToyOS sysroot",
        || host_target_missing(root).then_some(()),
        |()| link_host_target(root),
    );

    lock.act_if(
        "build toyos-libc for the sysroot",
        || crate::libc::stale(root, &rust_dir).then_some(()),
        |()| crate::libc::build(root, &rust_dir),
    );
}

/// Whether the `toyos` rustup toolchain points anywhere other than `stage2`.
///
/// `rustup toolchain link` unlinks and recreates the symlink rather than
/// replacing it atomically, so every call opens a window in which
/// `~/.rustup/toolchains/toyos` does not resolve. Any concurrent `rustc` proxy
/// invocation landing in that window dies with `'rustc' is not installed for the
/// custom toolchain 'toyos'` — which reads as a broken toolchain rather than as
/// contention, because a probe run a moment later succeeds.
///
/// This ran unconditionally on every `ensure`, i.e. every build. With five
/// agents building in one tree it cost one of them eleven consecutive
/// `cargo test` invocations over about fifteen minutes, while
/// `RUSTUP_TOOLCHAIN=toyos rustc --version` succeeded 20 out of 20 between the
/// attempts.
///
/// A mismatched or absent link still re-links, so a moved tree or a fresh clone
/// behaves as before; only the no-op case is skipped.
fn link_stale(stage2: &Path) -> bool {
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rustup")));

    let Some(home) = rustup_home else {
        return true;
    };
    fs::read_link(home.join("toolchains/toyos")).map_or(true, |current| current != stage2)
}

fn full_bootstrap(root: &Path, rust_dir: &Path) {
    let toyos_ld = toyos_ld_binary(root);

    // Ensure library/backtrace is checked out — std depends on it.
    // Other rust submodules (llvm, docs, cargo) are handled by bootstrap on demand.
    crate::ensure_submodule(rust_dir, "library/backtrace");

    // Write bootstrap.toml — ToyOS as target only, not host (fast rebuilds)
    let host = host_triple();
    write_config(rust_dir, &host, &toyos_ld, false);

    // Clean cached std for all ToyOS targets so bootstrap picks up compiler changes
    // (e.g. target spec changes like default_uwtable that affect codegen).
    for target in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let stage1_std = rust_dir.join(format!("build/{host}/stage1-std/{target}"));
        if stage1_std.exists() {
            fs::remove_dir_all(&stage1_std).ok();
        }
    }

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build");

    if !status.success() {
        // Check if essential artifacts exist (rustdoc for ToyOS may fail, that's ok)
        let stage2 = rust_dir.join(format!("build/{host}/stage2"));
        assert!(
            stage2.join("bin/rustc").exists(),
            "Toolchain build failed and rustc artifacts are missing"
        );
        eprintln!("Note: some targets may have failed to link (expected), but rustc built successfully.");
    }
}

fn rebuild_std(root: &Path, rust_dir: &Path) {
    // Ensure cross-only config (no hosted rustc) — if a previous hosted build
    // was interrupted, bootstrap.toml may still have ToyOS as host.
    let toyos_ld = toyos_ld_binary(root);
    write_config(rust_dir, &host_triple(), &toyos_ld, false);

    // Clean bootstrap's cached std for ToyOS targets so it picks up toyos-abi changes.
    // Bootstrap caches compiled std artifacts and won't notice external dep changes.
    let host = host_triple();
    for target in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let stage1_std = rust_dir.join(format!("build/{host}/stage1-std/{target}"));
        if stage1_std.exists() {
            fs::remove_dir_all(&stage1_std).ok();
        }
    }

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "library", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build library");
    assert!(status.success(), "std rebuild failed");
}

fn build_hosted_rustc(rust_dir: &Path, toyos_ld: &Path) {
    eprintln!("Building ToyOS-hosted rustc...");
    write_config(rust_dir, &host_triple(), toyos_ld, true);

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let status = Command::new(x)
        .args(["build", "--stage", "2", "--warnings", "warn"])
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .status()
        .expect("Failed to run x build for hosted rustc");

    // rustdoc for ToyOS may fail to link (expected), but rustc + librustc_driver must exist
    let toyos_stage2 = rust_dir.join("build/x86_64-unknown-toyos/stage2");
    assert!(
        toyos_stage2.join("bin/rustc").exists(),
        "Hosted rustc build failed: {} missing", toyos_stage2.join("bin/rustc").display()
    );
    assert!(
        fs::read_dir(toyos_stage2.join("lib"))
            .map(|d| d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("librustc_driver")))
            .unwrap_or(false),
        "Hosted rustc build failed: librustc_driver*.so missing"
    );
    if !status.success() {
        eprintln!("Note: rustdoc for ToyOS failed to link (expected), but rustc built successfully.");
    }
    // No config restore needed — full_bootstrap and rebuild_std write the
    // cross-only config before they run, so the next non-hosted build
    // always starts with the correct config regardless of what's on disk.
}

fn write_config(rust_dir: &Path, host: &str, toyos_ld: &Path, with_hosted_rustc: bool) {
    let linker = toyos_ld.display();
    let host_line = if with_hosted_rustc {
        format!("host = [\"{host}\", \"x86_64-unknown-toyos\"]")
    } else {
        format!("host = [\"{host}\"]")
    };
    let codegen_backends = if with_hosted_rustc {
        "\ncodegen-backends = [\"cranelift\"]"
    } else {
        ""
    };
    let config = format!(
        r#"change-id = "ignore"
profile = "compiler"

[build]
{host_line}
target = ["{host}", "x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"]

[rust]
incremental = true
lld = false

[target.x86_64-unknown-toyos]
linker = "{linker}"{codegen_backends}

"#
    );
    fs::write(rust_dir.join("bootstrap.toml"), config).unwrap();
}

/// Path to the host toyos-ld binary (stable location, never wiped by sysroot rebuilds).
pub fn toyos_ld_binary(root: &Path) -> PathBuf {
    let host = host_triple();
    root.join(format!("toyos-ld/target/{host}/release/toyos-ld"))
}

fn build_toyos_ld(root: &Path) {
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(root.join("toyos-ld"))
        .status()
        .expect("Failed to build toyos-ld");
    assert!(status.success(), "toyos-ld build failed");
}

/// Path to the host toyos-cc binary.
pub fn toyos_cc_binary(root: &Path) -> PathBuf {
    let host = host_triple();
    root.join(format!("toyos-cc/target/{host}/release/toyos-cc"))
}

fn build_toyos_cc(root: &Path) {
    let host = host_triple();
    let status = Command::new("cargo")
        .args(["build", "--release", "--target", &host])
        .current_dir(root.join("toyos-cc"))
        .status()
        .expect("Failed to build toyos-cc");
    assert!(status.success(), "toyos-cc build failed");
}

/// The host triple, asked of rustc once per process.
///
/// Every path built from it calls this, so an uncached one spent about seven
/// `rustc --version --verbose` spawns per build call — 0.118 s each, measured —
/// and they fell inside the windows the build lock now covers.
pub fn host_triple() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| {
        let output = Command::new("rustc")
            .args(["--version", "--verbose"])
            .output()
            .expect("Failed to run rustc");
        let text = String::from_utf8(output.stdout).unwrap();
        text.lines()
            .find(|l| l.starts_with("host:"))
            .map(|l| l.strip_prefix("host: ").unwrap().to_string())
            .expect("Could not determine host triple")
    })
    .clone()
}

/// PATH with toyos-ld's build directory prepended, so rustc finds it for linking.
pub fn path_with_toyos_ld(root: &Path) -> String {
    let host = host_triple();
    let ld_dir = root.join(format!("toyos-ld/target/{host}/release"));
    match std::env::var("PATH") {
        Ok(p) => format!("{}:{p}", ld_dir.display()),
        Err(_) => ld_dir.display().to_string(),
    }
}

/// Whether the ToyOS sysroot is missing the host target proc-macros compile against.
fn host_target_missing(root: &Path) -> bool {
    let toyos_sysroot = root.join("rust/build/x86_64-unknown-toyos/stage2/lib/rustlib");
    toyos_sysroot.exists() && !toyos_sysroot.join(host_triple()).exists()
}

fn link_host_target(root: &Path) {
    let host = host_triple();
    let host_target_dir = root
        .join("rust/build/x86_64-unknown-toyos/stage2/lib/rustlib")
        .join(&host);

    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .expect("Failed to run rustc");
    let stable_sysroot = String::from_utf8(output.stdout).unwrap();
    let stable_sysroot = stable_sysroot.trim();
    let source = Path::new(stable_sysroot).join("lib/rustlib").join(&host);
    assert!(
        source.exists(),
        "Host target {} not found in stable toolchain at {}",
        host,
        source.display()
    );

    std::os::unix::fs::symlink(&source, &host_target_dir).unwrap_or_else(|e| {
        panic!(
            "Failed to symlink {} -> {}: {}",
            host_target_dir.display(),
            source.display(),
            e
        )
    });
}

fn run(cmd: &str, args: &[&str]) {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("Failed to run {cmd}: {e}"));
    assert!(status.success(), "{cmd} failed");
}
