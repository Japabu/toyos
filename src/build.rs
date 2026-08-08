use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::Deserialize;

use crate::assets;
use crate::buildlock;
use crate::image;
use crate::toolchain;

// --- Config ---

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SystemConfig {
    init: Vec<String>,
    #[serde(default)]
    programs: HashMap<String, ProgramConfig>,
    #[serde(default)]
    symlinks: HashMap<String, String>,
    #[serde(default)]
    hosted_rustc: bool,
    #[serde(default)]
    assets: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct ProgramConfig {
    path: Option<String>,
    no_default_features: bool,
}

impl ProgramConfig {
    /// Resolve the crate directory for this program.
    /// Defaults to `userland/<name>` if no explicit path is set.
    fn crate_dir(&self, root: &Path, name: &str) -> PathBuf {
        match &self.path {
            Some(p) => root.join(p),
            None => root.join("userland").join(name),
        }
    }

    /// Whether this program is a workspace member of the userland workspace.
    /// Programs with explicit paths or special flags are standalone.
    fn is_workspace_member(&self) -> bool {
        self.path.is_none() && !self.no_default_features
    }
}

fn parse_config(path: &Path) -> SystemConfig {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    toml::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()))
}

// --- Freshness checking ---

/// Fingerprint all external build dependencies that cargo cannot track.
fn external_fingerprint(root: &Path) -> String {
    let host = toolchain::host_triple();
    let sysroot = toolchain::rust_dir(root).join(format!("build/{host}/stage2/lib/rustlib"));
    let mut entries = Vec::new();

    for triple in ["x86_64-unknown-toyos", "x86_64-unknown-none", "x86_64-unknown-uefi"] {
        let lib_dir = sysroot.join(format!("{triple}/lib"));
        let Ok(rd) = fs::read_dir(&lib_dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str());
            if !matches!(ext, Some("rlib" | "rmeta")) {
                continue;
            }
            if let Ok(meta) = path.metadata() {
                let name = path.file_name().unwrap().to_string_lossy();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                entries.push(format!("{triple}/{name}:{}:{mtime}", meta.len()));
            }
        }
    }

    let linker = toolchain::toyos_ld_binary(root);
    if let Ok(meta) = linker.metadata() {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        entries.push(format!("toyos-ld:{}:{mtime}", meta.len()));
    }

    entries.sort();
    entries.join("\n")
}

/// How much of a crate's target directory goes when the external deps change.
#[derive(Clone, Copy)]
enum Clean {
    All,
    /// Crates with explicit paths (toyos-ld, toyos-cc) also have host builds
    /// that must survive: the host toyos-ld *is* the cross linker.
    ToyosOnly,
}

fn stale(crate_dir: &Path, fingerprint: &str) -> bool {
    let stamp = crate_dir.join("target/.deps-stamp");
    fs::read_to_string(&stamp).map_or(true, |stored| stored != fingerprint)
}

fn clean(crate_dir: &Path, kind: Clean, fingerprint: &str) {
    match kind {
        Clean::All => {
            eprintln!("external deps changed: cleaning {}", crate_dir.display());
            let _ = Command::new("cargo")
                .arg("clean")
                .current_dir(crate_dir)
                .status();
        }
        Clean::ToyosOnly => {
            let toyos_dir = crate_dir.join("target/x86_64-unknown-toyos");
            if toyos_dir.exists() {
                eprintln!("external deps changed: cleaning {}", toyos_dir.display());
                fs::remove_dir_all(&toyos_dir).ok();
            }
        }
    }

    fs::create_dir_all(crate_dir.join("target")).ok();
    fs::write(crate_dir.join("target/.deps-stamp"), fingerprint).ok();
}

/// Drop the target directories the changed external deps invalidated.
///
/// Deciding and acting under one exclusive section is the whole point. Each of
/// these cleans removes a tree another builder may be compiling into, and
/// cargo's own lock cannot cover it — the lock lives at
/// `target/<profile>/.cargo-lock`, inside what the clean deletes. Two processes
/// that each decided before either acted would still both clean, which is the
/// pair of `cargo clean`s that died with ENOENT on each other's files.
fn invalidate_stale(root: &Path, lock: &mut buildlock::Held, targets: &[(PathBuf, Clean)]) {
    lock.act_if(
        buildlock::Scope::Worktree,
        "clean crate targets against changed external deps",
        || {
            let fp = external_fingerprint(root);
            let work: Vec<(PathBuf, Clean)> = targets
                .iter()
                .filter(|(dir, _)| stale(dir, &fp))
                .cloned()
                .collect();
            (!work.is_empty()).then_some((fp, work))
        },
        |(fp, work)| {
            for (dir, kind) in work {
                clean(&dir, kind, &fp);
            }
        },
    );
}

/// Every crate a config builds into, and how much of each goes when stale.
fn config_targets(root: &Path, config: &SystemConfig) -> Vec<(PathBuf, Clean)> {
    let mut targets = vec![
        (root.join("kernel"), Clean::All),
        (root.join("bootloader"), Clean::All),
        (root.join("userland"), Clean::All),
    ];
    for (name, cfg) in &config.programs {
        if !cfg.is_workspace_member() {
            targets.push((cfg.crate_dir(root, name), Clean::ToyosOnly));
        }
    }
    targets
}

// --- Cargo helpers ---

/// The profile every guest binary is built with, and the directory cargo puts
/// it in.
///
/// One name, passed to every `cargo build` here and declared by every crate
/// root the image is made of. `--release` used to be a flag on `cargo run`, and
/// it silently turned `debug-assertions` and `overflow-checks` off — the two
/// knobs `specs/known-issues.md`'s crafted-ELF panics were *found* by. There is
/// no longer a second profile to pick, which is why there is no longer a flag.
pub const PROFILE: &str = "toyos";

fn cargo_build(
    crate_dir: &Path,
    target: &str,
    extra_args: &[&str],
    path_env: &str,
    extra_env: &[(&str, &str)],
    quiet: bool,
) {
    let mut args = vec!["build", "--target", target, "--profile", PROFILE];
    if quiet {
        args.push("--quiet");
    }
    args.extend_from_slice(extra_args);
    let mut cmd = Command::new("cargo");
    cmd.args(&args)
        .current_dir(crate_dir)
        .env("RUSTUP_TOOLCHAIN", "toyos")
        .env_remove("RUSTFLAGS")
        .env("PATH", path_env)
        .env_remove("RUSTC");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // `Command::output()` pipes any stream the builder left unset, so reaching
    // for it is a decision to capture *both* modes, not just the quiet one.
    // Diagnostics must survive a successful build either way: a warning nobody
    // sees is a warning nobody fixes.
    let status = if quiet {
        // The caller owns the terminal (the test harness interleaves this with
        // its own progress), so hold cargo's output until the crate is done and
        // replay it as one block. `--quiet` reduces that block to diagnostics.
        let output = cmd
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap_or_else(|e| {
                panic!("cargo build failed to launch in {}: {e}", crate_dir.display())
            });
        std::io::Write::write_all(&mut std::io::stderr(), &output.stderr).ok();
        output.status
    } else {
        cmd.status().unwrap_or_else(|e| {
            panic!("cargo build failed to launch in {}: {e}", crate_dir.display())
        })
    };
    if !status.success() {
        panic!("cargo build failed in {}", crate_dir.display());
    }
}

// --- Artifact staging ---
//
// The bootloader's init list is *compiled in* (`bootloader/build.rs` declares
// `rerun-if-env-changed=INIT_PROGRAMS`; `main.rs` reads `env!("INIT_PROGRAMS")`),
// and the kernel's features likewise change its binary. But cargo keys the
// artifact path on (crate, target, profile) and nothing else, so every config
// writes and reads one path.
//
// The window is not a moment: `build_test_image` builds the bootloader, then
// runs the entire userland build and initrd assembly, and only then reads the
// `.efi`. Seconds to minutes, during which another config's build overwrites it.
// Observed: an image carrying metalcase's initrd and another config's bootloader,
// whose 28-byte init string was `"/bin/soundd;/bin/test-runner"`. The compositor
// was never spawned and the test failed as though the daemon under test were
// broken.
//
// So: hold [`buildlock::artifact`] across each build→stage pair, and copy the
// artifact to a name carrying what it is actually keyed by. Readers use the
// staged name, which no other config can overwrite.

fn key_hash(parts: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
    }
    h.finish()
}

/// Copy a just-built artifact to a path carrying its build key, and return that
/// path. Must be called with [`buildlock::artifact`] held, before anything else
/// can rebuild the same crate.
fn stage_artifact(root: &Path, built: &Path, stem: &str, key: u64) -> PathBuf {
    let staged = root.join(format!("target/{stem}-{key:016x}"));
    fs::create_dir_all(root.join("target")).ok();
    fs::copy(built, &staged).unwrap_or_else(|e| {
        panic!("stage {} -> {}: {e}", built.display(), staged.display())
    });
    staged
}

/// The panic message rustc emits beside every checked add. Absent from a binary
/// built with `overflow-checks = false`, because then there is no call site to
/// reference it and the linker's liveness pass drops it — measured on this
/// kernel: present at 3,784,872 bytes with the checks on, gone at 3,296,672
/// with them off.
const OVERFLOW_CHECK_MARKER: &[u8] = b"attempt to add with overflow";

/// Refuse to build an image whose kernel does not carry its overflow checks.
///
/// [`PROFILE`] states them and `--release` is gone from this build system, so
/// the way they can still be lost is somebody editing `[profile.toyos]`. This
/// asks the artifact rather than the manifest, which is the only question worth
/// asking: `specs/known-issues.md`'s two crafted-ELF kernel panics were both
/// *found* by an overflow check, and one of them had no configuration in which
/// it was an error return.
fn assert_overflow_checked(what: &str, image: &[u8]) {
    let found = image
        .iter()
        .enumerate()
        .filter(|&(_, &b)| b == OVERFLOW_CHECK_MARKER[0])
        .any(|(at, _)| image[at..].starts_with(OVERFLOW_CHECK_MARKER));
    assert!(
        found,
        "the {what} was built without overflow checks: nothing in {} bytes references \
         {:?}. `[profile.toyos]` states `overflow-checks = true` in every crate root the \
         image is made of; something has stopped being true.",
        image.len(),
        core::str::from_utf8(OVERFLOW_CHECK_MARKER).unwrap()
    );
}

// --- Shared initrd assembly ---

/// Build all programs from a config and assemble an initrd.
fn build_and_assemble(
    root: &Path,
    config: &SystemConfig,
    path_env: &str,
    extra_files: &[(String, Vec<u8>)],
    quiet: bool,
) -> Vec<u8> {
    let userland_dir = root.join("userland");

    let mut workspace_packages: Vec<&str> = Vec::new();
    let mut standalone: Vec<(&String, &ProgramConfig)> = Vec::new();
    for (name, cfg) in &config.programs {
        let crate_dir = cfg.crate_dir(root, name);
        assert!(
            crate_dir.join("Cargo.toml").exists(),
            "Program '{name}' crate not found at {}",
            crate_dir.display()
        );
        if cfg.is_workspace_member() {
            workspace_packages.push(name);
        } else {
            standalone.push((name, cfg));
        }
    }

    let mut initrd_files: Vec<(String, Vec<u8>)> = Vec::new();
    let ws_target = userland_dir.join(format!("target/x86_64-unknown-toyos/{PROFILE}"));

    // Build and read under one hold, exactly as `build_toyos_bins` does and for
    // the same reason: a program's path is keyed on (crate, target, profile)
    // alone, so every config in this run writes and reads the same
    // `userland/target/.../toybox`. Cargo's own lock orders the two *builds* and
    // says nothing about a read between them — `ioapic_topology` died on
    // `Failed to read binary for toybox` while another worker's config was
    // relinking it, and was green the moment it was re-run alone.
    {
        let _artifact = buildlock::artifact(root);
        if !workspace_packages.is_empty() {
            let mut extra: Vec<&str> = Vec::new();
            for pkg in &workspace_packages {
                extra.push("-p");
                extra.push(pkg);
            }
            cargo_build(
                &userland_dir,
                "x86_64-unknown-toyos",
                &extra,
                path_env,
                &[],
                quiet,
            );
        }

        for (name, cfg) in &standalone {
            let crate_dir = cfg.crate_dir(root, name);
            let mut extra: Vec<&str> = Vec::new();
            if cfg.no_default_features {
                extra.push("--no-default-features");
            }
            cargo_build(
                &crate_dir,
                "x86_64-unknown-toyos",
                &extra,
                path_env,
                &[],
                quiet,
            );
        }

        for (name, cfg) in &config.programs {
            let binary = if cfg.is_workspace_member() {
                ws_target.join(name)
            } else {
                let crate_dir = cfg.crate_dir(root, name);
                crate_dir.join(format!("target/x86_64-unknown-toyos/{PROFILE}/{name}"))
            };
            let data =
                fs::read(&binary).unwrap_or_else(|_| panic!("Failed to read binary for {name}"));
            initrd_files.push((format!("bin/{name}"), data));
        }

        if config.hosted_rustc {
            collect_hosted_rustc(root, &mut initrd_files);
        }
    }

    if !config.assets.is_empty() {
        initrd_files.extend(assets::collect(&config.assets));
    }

    // Extra files (test binaries, shared libs)
    for (name, data) in extra_files {
        initrd_files.push((name.clone(), data.clone()));
    }

    let symlinks: Vec<(String, String)> = config.symlinks.iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    image::create_initrd(&initrd_files, &symlinks, quiet)
}

// --- Public API ---

/// Which boot the image being built is for.
///
/// The two differ only in the config they read, and that is the point: the
/// diagnostic image's kernel and bootloader are byte-identical to the ordinary
/// one's, so what the owner reads off a diag boot is what the shipping kernel
/// does. A `#[cfg]` could not have given us that.
#[derive(Clone, Copy, PartialEq)]
pub enum Boot {
    Normal,
    /// `diag/system.toml`: nothing in the image can claim the framebuffer, so
    /// the kernel's last boot checkpoint stays on screen. `tests/toyos.rs`'s
    /// `screen_diag_boot` boots this same config, so the tested image and the
    /// flashed image are the same image.
    Diag,
    /// `console/system.toml`: `/bin/console` claims the framebuffer and runs
    /// the shell on it. A third mode rather than a replacement for [`Diag`] —
    /// claiming the screen is what stops the boot checkpoints painting, so a
    /// machine that wedges before userland is readable in that mode and in no
    /// other. `screen_console_shell` boots this config.
    Console,
}

impl Boot {
    fn config(self) -> &'static str {
        match self {
            Self::Normal => "system.toml",
            Self::Diag => "diag/system.toml",
            Self::Console => "console/system.toml",
        }
    }

    /// A separate output, so a diag build never leaves `bootable.img` quietly
    /// contradicting the committed config. The previous flashed artifact was
    /// made by editing `system.toml` and reverting it afterwards, which is
    /// exactly the state this avoids.
    fn image(self) -> &'static str {
        match self {
            Self::Normal => "target/bootable.img",
            Self::Diag => "target/bootable-diag.img",
            Self::Console => "target/bootable-console.img",
        }
    }
}

/// The cargo feature list this build's kernel is compiled with, as one comma-
/// separated argument.
///
/// **Every name the caller asked for is checked against `kernel/Cargo.toml`,
/// and an unknown one stops the build by name.** Read from the manifest rather
/// than listed here, so the check cannot drift from what cargo would accept —
/// and, more to the point, so that deleting a feature takes its own command
/// lines down with it. That is what a temporary feature needs: when
/// `hda-probe` goes at `specs/hda-driver-plan.md` H9, an invocation still
/// asking for it fails saying so instead of quietly producing a kernel with no
/// probe in it, which is the same image and a different machine.
///
/// Cargo would refuse an unknown feature too — after the build lock, the
/// toolchain check and the userland build, and with `kernel` in the message
/// rather than the flag the user typed. This runs before any of them.
fn kernel_features(root: &Path, debug: bool, requested: &[String]) -> String {
    let mut features: Vec<&str> = Vec::new();
    if debug {
        features.push("debug-wait");
    }
    if !requested.is_empty() {
        let declared = declared_kernel_features(root);
        for name in requested {
            assert!(
                declared.contains(name),
                "--kernel-feature {name}: the kernel declares no such feature.\n\
                 Features it declares: {}.",
                declared.join(", ")
            );
            features.push(name);
        }
    }
    features.join(",")
}

#[derive(Deserialize)]
struct KernelManifest {
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

fn declared_kernel_features(root: &Path) -> Vec<String> {
    let path = root.join("kernel/Cargo.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    let manifest: KernelManifest = toml::from_str(&text)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()));
    manifest.features.into_keys().collect()
}

/// Full build: kernel, bootloader, all programs, boot image. Returns the image.
pub fn build(
    root: &Path,
    debug: bool,
    boot: Boot,
    rebuild_toolchain: bool,
    claim_sysroot: bool,
    kernel_feature: &[String],
) -> PathBuf {
    // Before the locks: a misspelled feature is the user's own command line and
    // has to come back now, not after this build has waited out every other
    // worktree's hold on the sysroot.
    let kernel_features = kernel_features(root, debug, kernel_feature);

    // Outermost, before any build lock, and that order is the whole deadlock
    // argument: every acquirer of both takes the sysroot lock first. It waits
    // for every suite run in flight — replacing the sysroot under one turns its
    // every later build into a refusal, which is what a dead gate and 156
    // identical refusals looked like on 2026-08-04.
    let _claim = claim_sysroot.then(|| buildlock::claim_sysroot(root, "--claim-sysroot"));

    // After the sysroot lock and before every build lock, which is the order
    // the module header fixes. What it bounds is the host: ten agents' builds
    // spend the same fourteen cores, and nothing was counting them.
    let _slot = buildlock::build_slot(root, "cargo run");

    // Held until the last staged artifact has been read back, so no other
    // agent's clean or toolchain rebuild can land inside this build.
    let mut lock = buildlock::shared(root, "build");
    toolchain::ensure(root, rebuild_toolchain, claim_sysroot, &mut lock);

    let path_env = toolchain::path_with_toyos_ld(root);
    let config = parse_config(&root.join(boot.config()));

    invalidate_stale(root, &mut lock, &config_targets(root, &config));

    let init_programs = config.init.join(";");

    // Same lock-and-stage as `build_test_image`: `cargo run --build-only` and
    // `cargo test` share these paths, so this races the harness too.
    let (kernel_art, bl_art) = {
        let _artifact = buildlock::artifact(root);
        let kernel_handle = {
            let root = root.to_path_buf();
            let path_env = path_env.clone();
            let features = kernel_features.clone();
            std::thread::spawn(move || {
                let mut extra = Vec::new();
                if !features.is_empty() {
                    extra.push("--features");
                    extra.push(&features);
                }
                cargo_build(
                    &root.join("kernel"),
                    "x86_64-unknown-none",
                    &extra,
                    &path_env,
                    &[],
                    false,
                );
            })
        };
        {
            cargo_build(
                &root.join("bootloader"),
                "x86_64-unknown-uefi",
                &[],
                &path_env,
                &[("INIT_PROGRAMS", init_programs.as_str())],
                false,
            );
        }
        kernel_handle.join().expect("kernel build thread panicked");
        (
            stage_artifact(
                root,
                &root.join(format!("kernel/target/x86_64-unknown-none/{PROFILE}/kernel")),
                "kernel",
                key_hash(&[PROFILE, &kernel_features]),
            ),
            stage_artifact(
                root,
                &root.join(format!(
                    "bootloader/target/x86_64-unknown-uefi/{PROFILE}/bootloader.efi"
                )),
                "bootloader.efi",
                key_hash(&[PROFILE, &init_programs]),
            ),
        )
    };

    let initrd_bytes =
        build_and_assemble(root, &config, &path_env, &[], false);

    let kernel_bytes = fs::read(&kernel_art).expect("Failed to read staged kernel");
    assert_overflow_checked("kernel", &kernel_bytes);
    let bl_bytes = fs::read(&bl_art).expect("Failed to read staged bootloader");
    let disk_bytes = image::create_boot_image(&kernel_bytes, &bl_bytes, &initrd_bytes);
    let image_path = root.join(boot.image());
    fs::write(&image_path, disk_bytes).expect("Failed to write image");

    let nvme_path = root.join("target/nvme.img");
    if !nvme_path.exists() {
        create_sparse(&nvme_path, 1024 * 1024 * 1024);
    }

    image_path
}

/// Create an empty disk image the guest sees at full size and the host pays
/// nothing for until something is written. A materialized image caps how big
/// a device the tests may present, and device *size* is a shape dimension:
/// an index sized per device block is invisible on a small disk and fatal on
/// a real one.
///
/// Designates the result, because every caller here is making a scratch disk
/// for a guest that expects a working `/home`, and the kernel will not format
/// an undesignated one. Leaving it to the call sites would mean two places to
/// forget; forgetting is not silent (the boot says so and `/home` is volatile)
/// but it is not worth the chance.
pub fn create_sparse(path: &Path, len: u64) {
    let file = fs::File::create(path)
        .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    file.set_len(len)
        .unwrap_or_else(|e| panic!("set_len {} on {}: {e}", len, path.display()));
    designate_for_format(path, len);
}

/// Stamp block 0 so the kernel is allowed to format this image.
///
/// The kernel never formats a device that does not carry this, which is what
/// stops it taking the disk of any machine it is booted on. So a throwaway
/// image has to say so, and this is the whole of the test harness's opt-in:
/// **data on a scratch file, not a build flag.** The kernel binary and the
/// code path are identical either way — `probe` runs the same three-way match
/// on metal as it does here — so the configuration under test is the
/// configuration that ships, which a `#[cfg]` could not have given us.
///
/// Only ever called on a file this build system just created. It is a
/// destructive write by construction: on a device with anything on it, this
/// overwrites the partition table.
pub fn designate_for_format(path: &Path, len: u64) {
    use std::io::{Seek, SeekFrom, Write};

    let mut block = [0u8; 4096];
    block[..bcachefs::DESIGNATION_MAGIC.len()].copy_from_slice(&bcachefs::DESIGNATION_MAGIC);
    let blocks = (len / 4096).to_le_bytes();
    let at = bcachefs::DESIGNATION_BLOCKS_OFFSET;
    block[at..at + blocks.len()].copy_from_slice(&blocks);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {} to designate: {e}", path.display()));
    file.seek(SeekFrom::Start(0))
        .unwrap_or_else(|e| panic!("seek {}: {e}", path.display()));
    file.write_all(&block)
        .unwrap_or_else(|e| panic!("stamp {}: {e}", path.display()));
}

/// One part of a boot image, built once per key for the life of this process.
///
/// A `cargo test` run boots ~76 machines, and 41 of those boots ask for an image
/// some earlier boot already built; the three `cargo` invocations then take
/// ~1.4 s between them to answer "nothing changed" (`specs/test-cost-audit.md`
/// §1.4). In memory and never on disk, so a run gets one answer for the tree it
/// started against and the next run asks cargo again.
///
/// Per part rather than per image, because a part is what a key can be true of:
/// the kernel is its feature set, the bootloader is its init list, the initrd is
/// its config and the caller's extra files. That is the same split
/// [`stage_artifact`] already writes into the artifact names, and it is what
/// makes this affordable — the 31 kernel feature sets a full run builds share a
/// handful of initrds, and an initrd is hundreds of megabytes.
///
/// What it does not see is a source edit that lands mid-run. A run is a
/// measurement of one tree, so that is the behaviour wanted either way; a run
/// that *starts* after a kernel edit still rebuilds every variant it uses.
struct Memo(std::sync::Mutex<BTreeMap<u64, Arc<Vec<u8>>>>);

impl Memo {
    const fn new() -> Self {
        Self(std::sync::Mutex::new(BTreeMap::new()))
    }

    fn get(&self, key: u64) -> Option<Arc<Vec<u8>>> {
        self.0.lock().expect("a build panicked holding the artifact memo").get(&key).cloned()
    }

    /// The lock is deliberately not held across `make`: a build that panics
    /// under it would poison the memo, and every later boot would then fail on
    /// the poison instead of on whatever went wrong with it.
    fn get_or_build(&self, key: u64, make: impl FnOnce() -> Vec<u8>) -> Arc<Vec<u8>> {
        if let Some(hit) = self.get(key) {
            return hit;
        }
        let made = Arc::new(make());
        self.0
            .lock()
            .expect("a build panicked holding the artifact memo")
            .insert(key, Arc::clone(&made));
        made
    }
}

static KERNEL: Memo = Memo::new();
static BOOTLOADER: Memo = Memo::new();
static INITRD: Memo = Memo::new();

/// What an initrd is a function of: the config naming the programs, and the
/// files the caller adds to it. Hashed whole — the test binaries in
/// `extra_files` are the bulk of the image, and a key over their names and
/// lengths would call two different builds of one binary the same image.
fn initrd_key(config_path: &Path, extra_files: &[(String, Vec<u8>)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    config_path.hash(&mut h);
    for (name, data) in extra_files {
        name.hash(&mut h);
        data.hash(&mut h);
    }
    h.finish()
}

/// Build a test image from a system.toml config. Returns the raw disk image bytes.
/// The caller writes these to a temp file for QEMU.
///
/// The image itself is never memoized, only the three parts it is made of:
/// [`image::create_boot_image`] mints a fresh partition GUID per call and writes
/// it into both the GPT and the ESP, and a boot that did not get its own is a
/// boot `log_partition_identity` is entitled to catch.
pub fn build_test_image(
    root: &Path,
    config_path: &Path,
    kernel_features: &[&str],
    quiet: bool,
    extra_files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let config = parse_config(config_path);
    let init_programs = config.init.join(";");
    let features = kernel_features.join(",");
    let kernel_key = key_hash(&[PROFILE, &features]);
    let bl_key = key_hash(&[PROFILE, &init_programs]);
    let initrd_key = initrd_key(config_path, extra_files);

    // Nothing left to build, so nothing for the lock, the toolchain check or the
    // staleness sweep to protect.
    if let (Some(kernel), Some(bl), Some(initrd)) =
        (KERNEL.get(kernel_key), BOOTLOADER.get(bl_key), INITRD.get(initrd_key))
    {
        return image::create_boot_image(&kernel, &bl, &initrd);
    }

    // **Below the memo's early return, so a boot that builds nothing queues for
    // nothing.** Above every build lock, per the module header. This is the
    // acquisition the eight-landing day was about: twelve suite workers each
    // hold a guest slot and the first thing each does is compile its kernel
    // variant, so the semaphore that bounds guests was bounding the phase that
    // was not scarce.
    let _slot = buildlock::build_slot(root, "a test image");

    // Held to the end of the function: the staged artifacts below are read
    // back after the userland build, and a clean landing in between is the
    // same defect as one landing mid-compile.
    let mut lock = buildlock::shared(root, "test image");
    crate::toolchain::ensure(root, false, false, &mut lock);
    let path_env = toolchain::path_with_toyos_ld(root);

    invalidate_stale(root, &mut lock, &config_targets(root, &config));

    // Build and stage under one lock. Releasing before `build_and_assemble` is
    // deliberate: the userland build is long, takes no shared artifact path, and
    // the staged copies below are already immune to another config's rebuild.
    let (kernel_bytes, bl_bytes) = {
        let _artifact = buildlock::artifact(root);
        let kernel = KERNEL.get_or_build(kernel_key, || {
            let mut kernel_extra: Vec<&str> = Vec::new();
            if !features.is_empty() {
                kernel_extra.push("--features");
                kernel_extra.push(&features);
            }
            cargo_build(
                &root.join("kernel"),
                "x86_64-unknown-none",
                &kernel_extra,
                &path_env,
                &[],
                quiet,
            );
            let staged = stage_artifact(
                root,
                &root.join(format!("kernel/target/x86_64-unknown-none/{PROFILE}/kernel")),
                "kernel",
                kernel_key,
            );
            {
                let bytes = fs::read(&staged).expect("Failed to read staged kernel");
                assert_overflow_checked("kernel", &bytes);
                bytes
            }
        });
        let bl = BOOTLOADER.get_or_build(bl_key, || {
            cargo_build(
                &root.join("bootloader"),
                "x86_64-unknown-uefi",
                &[],
                &path_env,
                &[("INIT_PROGRAMS", init_programs.as_str())],
                quiet,
            );
            let staged = stage_artifact(
                root,
                &root.join(format!("bootloader/target/x86_64-unknown-uefi/{PROFILE}/bootloader.efi")),
                "bootloader.efi",
                bl_key,
            );
            fs::read(&staged).expect("Failed to read staged bootloader")
        });
        (kernel, bl)
    };

    let initrd_bytes = INITRD.get_or_build(initrd_key, || {
        build_and_assemble(root, &config, &path_env, extra_files, quiet)
    });

    image::create_boot_image(&kernel_bytes, &bl_bytes, &initrd_bytes)
}

/// Build all binaries in a multi-binary crate. Returns vec of (binary_name, bytes).
/// Also builds any cdylib subcrates and includes their .so files.
pub fn build_toyos_bins(root: &Path, crate_path: &Path, quiet: bool) -> Vec<(String, Vec<u8>)> {
    let _slot = buildlock::build_slot(root, "the test binaries");
    let mut lock = buildlock::shared(root, "test binaries");
    crate::toolchain::ensure(root, false, false, &mut lock);
    let path_env = toolchain::path_with_toyos_ld(root);

    let mut targets = vec![(crate_path.to_path_buf(), Clean::All)];
    for entry in fs::read_dir(crate_path).into_iter().flatten().flatten() {
        let sub_path = entry.path();
        if sub_path.is_dir() && sub_path.join("Cargo.toml").exists() {
            targets.push((sub_path, Clean::All));
        }
    }
    invalidate_stale(root, &mut lock, &targets);

    let mut results = Vec::new();

    // Every build→read pair below is under one hold, for the reason the
    // "Artifact staging" section above gives: cargo keys an artifact path on
    // (crate, target, profile), so a second `cargo test` in this tree writes the
    // very `.so` and test binaries this one reads back. Between the `read_dir`
    // and the `read` that was enough to kill a run outright — four concurrent
    // suites, one dead on `Result::unwrap()` on a `NotFound` naming no file.
    let _artifact = buildlock::artifact(root);

    // Build cdylib subcrates first
    let mut lib_search_dirs = Vec::new();
    for entry in fs::read_dir(crate_path).unwrap() {
        let entry = entry.unwrap();
        let sub_path = entry.path();
        if !sub_path.is_dir() {
            continue;
        }
        let cargo_toml = sub_path.join("Cargo.toml");
        if !cargo_toml.exists() {
            continue;
        }
        let toml_text = fs::read_to_string(&cargo_toml).unwrap();
        if !toml_text.contains("cdylib") {
            continue;
        }

        let lib_name = sub_path.file_name().unwrap().to_str().unwrap();
        if !quiet {
            eprintln!("[build] Building cdylib subcrate: {lib_name}");
        }
        cargo_build(&sub_path, "x86_64-unknown-toyos", &[], &path_env, &[], quiet);

        let lib_out = sub_path.join(format!("target/x86_64-unknown-toyos/{PROFILE}"));
        lib_search_dirs.push(lib_out.clone());

        for so_entry in fs::read_dir(&lib_out).unwrap() {
            let so_entry = so_entry.unwrap();
            let name = so_entry.file_name().to_str().unwrap().to_string();
            if name.ends_with(".so") {
                let path = so_entry.path();
                let data = fs::read(&path)
                    .unwrap_or_else(|e| panic!("read the cdylib {}: {e}", path.display()));
                results.push((name, data));
            }
        }
    }

    // Build test binaries — pass -L flags for cdylib .so locations
    let mut link_flags = String::new();
    for dir in &lib_search_dirs {
        link_flags.push_str(&format!("-L {} ", dir.display()));
    }
    let extra_env: Vec<(&str, &str)> = if link_flags.is_empty() {
        vec![]
    } else {
        vec![("RUSTFLAGS", link_flags.trim_end())]
    };
    cargo_build(
        crate_path,
        "x86_64-unknown-toyos",
        &["--bins"],
        &path_env,
        &extra_env,
        quiet,
    );

    let bin_dir = crate_path.join(format!("target/x86_64-unknown-toyos/{PROFILE}"));
    let bin_src = crate_path.join("src/bin");
    if bin_src.exists() {
        for entry in fs::read_dir(&bin_src).unwrap() {
            let entry = entry.unwrap();
            let name = entry
                .file_name()
                .to_str()
                .unwrap()
                .strip_suffix(".rs")
                .unwrap()
                .to_string();
            let binary = bin_dir.join(&name);
            if binary.exists() {
                let data = fs::read(&binary)
                    .unwrap_or_else(|e| panic!("read the test binary {}: {e}", binary.display()));
                results.push((name, data));
            }
        }
    }

    results
}

// --- Internal helpers ---

fn collect_hosted_rustc(root: &Path, initrd_files: &mut Vec<(String, Vec<u8>)>) {
    let sysroot = toolchain::rust_dir(root).join("build/x86_64-unknown-toyos/stage2");
    assert!(
        sysroot.exists(),
        "Hosted rustc sysroot missing: {}",
        sysroot.display()
    );

    let rustc = sysroot.join("bin/rustc");
    assert!(
        rustc.exists(),
        "Hosted rustc binary missing: {}",
        rustc.display()
    );
    initrd_files.push(("bin/rustc".to_string(), fs::read(&rustc).unwrap()));

    if let Ok(entries) = fs::read_dir(sysroot.join("lib")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "so") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                let data = fs::read(&path).unwrap();
                initrd_files.push((format!("lib/{name}"), data));
            }
        }
    }

    let backends = sysroot.join("lib/rustlib/x86_64-unknown-toyos/codegen-backends");
    if backends.exists() {
        for entry in fs::read_dir(&backends).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "so") {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                let data = fs::read(&path).unwrap();
                initrd_files.push((
                    format!("lib/rustlib/x86_64-unknown-toyos/codegen-backends/{name}"),
                    data,
                ));
            }
        }
    }

    if let Some(host_rlibs) = find_host_rlibs(root) {
        for entry in fs::read_dir(&host_rlibs).into_iter().flatten().flatten() {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|e| e == "rlib" || e == "rmeta")
            {
                let name = path.file_name().unwrap().to_str().unwrap().to_string();
                initrd_files.push((
                    format!("lib/rustlib/x86_64-unknown-toyos/lib/{name}"),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
}

fn find_host_rlibs(root: &Path) -> Option<PathBuf> {
    let build_dir = toolchain::rust_dir(root).join("build");
    let entries = fs::read_dir(&build_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n == "x86_64-unknown-toyos")
        {
            continue;
        }
        let rlib_dir = path.join("stage2/lib/rustlib/x86_64-unknown-toyos/lib");
        if rlib_dir.exists() {
            return Some(rlib_dir);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No image this repository ships starts sshd.
    ///
    /// It listens on every interface and authenticates against a file that is
    /// absent on a fresh install, so on a default boot it would be a port that
    /// accepts connections and refuses all of them. Whoever wants it runs
    /// `/bin/sshd` themselves. It stays in `[programs]` — the gate is on the
    /// init list, not on the binary being present.
    #[test]
    fn no_shipped_boot_config_starts_sshd() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for boot in [Boot::Normal, Boot::Diag, Boot::Console] {
            let config = boot.config();
            let init = parse_config(&root.join(config)).init;
            assert!(
                !init.iter().any(|p| p.ends_with("/sshd")),
                "{config} starts sshd from init: {init:?}",
            );
        }
    }
}
