use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Deserialize;

use crate::assets;
use crate::buildlock;
use crate::hostws;
use crate::image;
use crate::toolchain;

thread_local! {
    /// Time this worker has spent constructing memoized boot artifacts.
    ///
    /// A suite worker is also the thread that asks for its boot image, so a
    /// cumulative thread-local clock lets the harness remove a cold build from
    /// the test that happened to ask for it first. A process-wide counter would
    /// subtract another worker's concurrent build instead.
    static ARTIFACT_BUILD_TIME: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// One reading of the artifact-build clock for the current thread.
///
/// Test duration profiles are execution prices, not ownership of a shared
/// cache miss. Without this distinction, each CI shard charges its first
/// shipping- and test-kernel users tens of seconds, relegating those names;
/// the next run then charges the same builds to two different names. The raw
/// suite wall clock still includes every build. Only per-test prices use this
/// mark to remove construction of memoized kernel, bootloader, and initrd
/// artifacts.
#[derive(Clone, Copy)]
pub struct ArtifactBuildMark(Duration, PhantomData<Rc<()>>);

/// Read the current thread's cumulative artifact-build time.
pub fn mark_artifact_build_time() -> ArtifactBuildMark {
    ArtifactBuildMark(ARTIFACT_BUILD_TIME.get(), PhantomData)
}

impl ArtifactBuildMark {
    /// Remove artifact construction since this mark from a raw elapsed time.
    pub fn execution_part(self, raw: Duration) -> Duration {
        let built = ARTIFACT_BUILD_TIME.get().saturating_sub(self.0);
        raw.saturating_sub(built)
    }
}

/// Charges the slow, cache-filling half of [`build_test_image`] to the build
/// clock even if it unwinds. Image creation on a memo hit is deliberately
/// outside this guard: every boot pays that work, so it is part of the test's
/// repeatable execution price.
struct ArtifactBuildTimer(Instant);

impl ArtifactBuildTimer {
    fn start() -> Self {
        Self(Instant::now())
    }
}

impl Drop for ArtifactBuildTimer {
    fn drop(&mut self) {
        let elapsed = self.0.elapsed();
        ARTIFACT_BUILD_TIME.set(ARTIFACT_BUILD_TIME.get().saturating_add(elapsed));
    }
}

// --- Config ---

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct SystemConfig {
    #[serde(default)]
    programs: HashMap<String, ProgramConfig>,
    #[serde(default)]
    symlinks: HashMap<String, String>,
    #[serde(default)]
    hosted_rustc: bool,
    #[serde(default)]
    assets: Vec<String>,
    /// What `/bin/init` starts at boot. Program *keys*, never paths — a path
    /// here is a second spelling of a `[programs]` key and is what let a boot
    /// list smuggle an argument through. Arguments live on the program entry's
    /// `args` instead.
    #[serde(default)]
    boot: BootConfig,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct BootConfig {
    start: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
struct ProgramConfig {
    path: Option<String>,
    no_default_features: bool,
    /// Argv this program is started with, after argv[0].
    args: Vec<String>,
    /// Names init creates **one machine-wide port** for and endows this program
    /// the *acceptor* of.
    serves: Vec<String>,
    /// Names this program creates a port for **itself**, once per instance, and
    /// hands the connector down to its own children. init creates nothing and
    /// holds nothing for these — `surface` is the whole of this kind.
    provides: Vec<String>,
    /// Names in this program's namespace, each a *connector*.
    receives: Vec<String>,
    /// Device classes init mints a claim for and endows.
    devices: Vec<String>,
    /// Rights on the `SysCap` duplicate init endows this program, by the names
    /// `toyos_manifest::syscap_rights` takes. A handful of rows in the whole
    /// tree declare one.
    syscap: Vec<String>,
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

    /// Whether this program is a member of the **userland** workspace, the one
    /// `-p` selects a package from and whose `target/` holds the result.
    /// Programs with explicit paths or special flags are built from their own
    /// directory instead — which is not the same as being built into it:
    /// `toyos-ld` and `toyos-cc` have explicit paths and are members of the
    /// *host* workspace, so cargo writes them to the repository root's
    /// `target/`. `hostws::target_dir` is what answers that, never this.
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

    // Content, not `len:mtime`: `toyos-ld` relinks every CI job because a fresh
    // `actions/checkout` gives its sources a mtime cargo's own path-dependency
    // fingerprint has never seen, and the relink is a few MB — hashing it is
    // milliseconds against the `cargo clean` a changed mtime used to trigger on
    // every crate this fingerprint gates, `tests/toyos-rust-tests/tls-cranelift`
    // among them at 570 MiB. The sysroot rlibs above stay on `len:mtime`: CI
    // unpacks a byte-identical toolchain artifact with `tar`, which restores
    // mtimes, so their triple is already stable across jobs of one toolchain
    // tag and hashing every rlib would cost what the clean does today.
    let linker = toolchain::toyos_ld_binary(root);
    if let Ok(data) = fs::read(&linker) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut h);
        entries.push(format!("toyos-ld:{:016x}", h.finish()));
    }

    entries.sort();
    entries.join("\n")
}

/// How much of a crate's target directory goes when the external deps change.
#[derive(Clone, Copy)]
enum Clean {
    All,
    /// Crates with explicit paths (toyos-ld, toyos-cc) also have host builds
    /// that must survive: the host toyos-ld *is* the cross linker. Both are
    /// host-workspace members, so the directory this empties is the
    /// workspace's `target/x86_64-unknown-toyos` — the guest halves of the two,
    /// and nothing the host builds.
    ToyosOnly,
}

fn stale(root: &Path, crate_dir: &Path, fingerprint: &str) -> bool {
    let stamp = hostws::target_dir(root, crate_dir).join(".deps-stamp");
    fs::read_to_string(&stamp).map_or(true, |stored| stored != fingerprint)
}

fn clean(root: &Path, crate_dir: &Path, kind: Clean, fingerprint: &str) {
    // Where cargo actually wrote it. `toyos-ld` and `toyos-cc` are members of
    // the host workspace, so their guest builds land in the root's `target/`
    // and both answer with the same directory: the second clean of a pass finds
    // it already gone and does nothing, which is the right amount of work.
    let target = hostws::target_dir(root, crate_dir);
    match kind {
        Clean::All => {
            // `cargo clean` in a member's directory cleans the whole workspace,
            // this build system's own target directory included. Nothing asks
            // for that today; refusing it by name is cheaper than finding out.
            assert!(
                !hostws::is_member(root, crate_dir),
                "{} is a host-workspace member, and `cargo clean` there would empty the \
                 workspace's whole target directory rather than this crate's",
                crate_dir.display(),
            );
            eprintln!("external deps changed: cleaning {}", crate_dir.display());
            let _ = Command::new("cargo")
                .arg("clean")
                .current_dir(crate_dir)
                .status();
        }
        Clean::ToyosOnly => {
            let toyos_dir = target.join("x86_64-unknown-toyos");
            if toyos_dir.exists() {
                eprintln!("external deps changed: cleaning {}", toyos_dir.display());
                fs::remove_dir_all(&toyos_dir).ok();
            }
        }
    }

    fs::create_dir_all(&target).ok();
    fs::write(target.join(".deps-stamp"), fingerprint).ok();
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
                .filter(|(dir, _)| stale(root, dir, &fp))
                .cloned()
                .collect();
            (!work.is_empty()).then_some((fp, work))
        },
        |(fp, work)| {
            for (dir, kind) in work {
                clean(root, &dir, kind, &fp);
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
/// knobs `issues/`'s crafted-ELF panics were *found* by. There is
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
// A kernel's feature list changes its binary, but cargo keys the artifact path
// on (crate, target, profile) and nothing else, so every config writes and reads
// one path.
//
// The window is not a moment: `build_test_image` builds, then runs the entire
// userland build and initrd assembly, and only then reads the artifact back.
// Seconds to minutes, during which another config's build overwrites it.
//
// So: hold [`buildlock::artifact`] across each build→stage pair, and copy the
// artifact to a name carrying what it is actually keyed by. Readers use the
// staged name, which no other config can overwrite.
//
// The bootloader used to be here for the same reason and no longer is: its init
// list was compiled into it, so the `.efi` was a function of the boot config,
// and an image once carried metalcase's initrd beside another config's
// bootloader whose 28-byte init string was `"/bin/soundd;/bin/test-runner"` —
// the compositor was never spawned and the test failed as though the daemon
// under test were broken. The bootloader carries no config now, so it is
// memoized once per profile and that hazard is not expressible.

/// The staged-artifact key of a kernel built with `features`.
///
/// **Both build paths go through this and that is the whole of the claim** that
/// a test which asks for no feature boots the binary an image ships: the staged
/// file is named for this key, so an equal key is not a similar kernel but the
/// same file. `cargo run --build-only` passes what `kernel_features` made of an
/// empty request; the harness passes `BootOptions::kernel_features` joined.
/// Nothing between them may add a name — which is what `qemu::fold_inert` used
/// to do to every boot in the suite.
fn kernel_key(features: &str) -> u64 {
    key_hash(&[PROFILE, features])
}

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

/// Refuse to build a kernel whose target has hardware float.
///
/// `arch::entry`'s bracket saves the user machine state at the ring transition
/// and nowhere else, which is sound only because kernel code cannot disturb it:
/// the FPU may be left dirty for a whole Ring 0 excursion because nothing in
/// Ring 0 reads or writes it. That rests on one line of the target spec —
/// `RustcAbi::Softfloat` and `+soft-float` in
/// `rust/compiler/rustc_target/src/spec/targets/x86_64_unknown_none.rs` — and an
/// edit turning it off would make every bracket in the kernel insufficient
/// without changing a byte of `kernel/`.
///
/// Asked of the compiler rather than of the manifest, and once per process: it
/// is a property of the toolchain rather than of any one image.
fn assert_kernel_is_softfloat(path_env: &str) {
    static CHECKED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CHECKED.get_or_init(|| {
        let out = Command::new("rustc")
            .args(["--print", "cfg", "--target", "x86_64-unknown-none"])
            .env("RUSTUP_TOOLCHAIN", "toyos")
            .env("PATH", path_env)
            .env_remove("RUSTFLAGS")
            .env_remove("RUSTC")
            .output()
            .expect("rustc --print cfg failed to launch");
        assert!(out.status.success(), "rustc --print cfg failed for the kernel target");
        let cfg = String::from_utf8_lossy(&out.stdout);
        assert!(
            cfg.lines().any(|l| l == r#"target_feature="x87""#),
            "the kernel target no longer reports x87, so `arch::fpu`'s FXSAVE64 image is not \
             the state this machine has:\n{cfg}"
        );
        assert!(
            !cfg.lines().any(|l| l == r#"target_feature="sse""#),
            "the kernel target has hardware float, so kernel code may now clobber the user \
             machine state between `arch::entry`'s save and its restore:\n{cfg}"
        );
    });
}

/// Refuse to build an image whose kernel does not carry its overflow checks.
///
/// [`PROFILE`] states them and `--release` is gone from this build system, so
/// the way they can still be lost is somebody editing `[profile.toyos]`. This
/// asks the artifact rather than the manifest, which is the only question worth
/// asking: `issues/`'s two crafted-ELF kernel panics were both
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
/// The one program the kernel starts, in **every** image whatever `[programs]`
/// says. It reads the manifest below and starts what that names, so an initrd
/// without it is a machine with a kernel and no userland at all.
const INIT_PROGRAM: &str = "init";

/// Names `/bin/init` serves itself.
///
/// init is in every image and is no `[programs]` key, so these have no
/// declaration to come from. They travel in the manifest so init creates
/// exactly the ports the build-time gate counted as provided — one producer,
/// rather than a constant here and a string in init.
const INIT_SERVED: &[&str] = &["launcher"];

/// The resolved config as the records `/bin/init` reads.
///
/// The format, the renderer and the parser are `toyos-manifest/`, whose
/// round-trip test is what makes "what the build writes is what init reads" a
/// fact rather than two hand-matched implementations.
fn render_manifest(config: &SystemConfig) -> Vec<u8> {
    let mut names: Vec<&String> = config.programs.keys().collect();
    names.sort();
    let manifest = toyos_manifest::Manifest {
        programs: names
            .iter()
            .map(|name| {
                let cfg = &config.programs[*name];
                toyos_manifest::Program {
                    name: (*name).clone(),
                    path: format!("/bin/{name}"),
                    args: cfg.args.clone(),
                    serves: cfg.serves.clone(),
                    provides: cfg.provides.clone(),
                    receives: cfg.receives.clone(),
                    devices: cfg.devices.clone(),
                    syscap: cfg.syscap.clone(),
                }
            })
            .collect(),
        init_serves: INIT_SERVED.iter().map(|s| (*s).to_string()).collect(),
        start: config.boot.start.clone(),
    };
    toyos_manifest::render(&manifest)
        .unwrap_or_else(|e| panic!("system.toml cannot be rendered as a manifest: {e:?}"))
}

fn build_and_assemble(
    root: &Path,
    config: &SystemConfig,
    path_env: &str,
    extra_files: &[(String, Vec<u8>)],
    quiet: bool,
) -> Vec<u8> {
    let userland_dir = root.join("userland");

    let mut workspace_packages: Vec<&str> = vec![INIT_PROGRAM];
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
                hostws::target_dir(root, &crate_dir)
                    .join(format!("x86_64-unknown-toyos/{PROFILE}/{name}"))
            };
            let data =
                fs::read(&binary).unwrap_or_else(|_| panic!("Failed to read binary for {name}"));
            initrd_files.push((format!("bin/{name}"), data));
        }

        let init = ws_target.join(INIT_PROGRAM);
        let data = fs::read(&init).expect("Failed to read binary for init");
        initrd_files.push((format!("bin/{INIT_PROGRAM}"), data));
        initrd_files.push((toyos_manifest::PATH.to_string(), render_manifest(config)));

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
    /// `diag/system.toml`: the config declares no `devices`, so nothing
    /// started there claims the framebuffer and the kernel's last boot
    /// checkpoint stays on screen. `tests/toyos.rs`'s
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
/// lines down with it. That is what a temporary feature needs: once one is
/// deleted, an invocation still asking for it fails saying so instead of
/// quietly producing a kernel with no diagnostic in it, which is the same
/// image and a different machine.
///
/// Cargo would refuse an unknown feature too — after the build lock, the
/// toolchain check and the userland build, and with `kernel` in the message
/// rather than the flag the user typed. This runs before any of them.
fn kernel_features(
    root: &Path,
    debug: bool,
    requested: &[String],
    params: &[String],
) -> String {
    let mut features: Vec<&str> = Vec::new();
    if debug {
        features.push(DEBUG_KERNEL_BUILD);
    }
    // A parameter names an actuator, and only a kernel compiled with them can
    // be told to arm one. This is the whole of what `--kernel-param` decides
    // about the build — which actuator is a boot's business, not a build's.
    if !params.is_empty() {
        features.push("boot-actuators");
    }
    if !requested.is_empty() {
        let declared = declared_kernel_features(root);
        for name in requested {
            assert!(
                declared.contains(name),
                "--kernel-feature {name}: the kernel declares no such feature.\n\
                 Features it declares: {}.\n\
                 Every actuator is now a --kernel-param; `cargo run -- --kernel-param --help` \
                 lists them.",
                declared.join(", ")
            );
            features.push(name);
        }
    }
    features.join(",")
}

/// The boot parameter this build writes to the ESP, with every name checked
/// against `kernel/src/actuator.rs`.
///
/// Refused here as well as in the kernel, and before any lock, so that deleting
/// an actuator takes its stale command lines down with it instead of quietly
/// producing an image that arms nothing — the same rule `--kernel-feature` runs
/// on, one layer further in.
fn kernel_cmdline(root: &Path, params: &[String]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let declared = declared_actuators(root);
    for name in params {
        assert!(
            declared.contains(name),
            "--kernel-param {name}: the kernel declares no such actuator.\n\
             Actuators it declares: {}.",
            declared.join(", ")
        );
    }
    params.join(",")
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

/// The kernel every test that needs an actuator boots: all of them compiled in
/// and none of them armed, plus the `SYS_DEBUG` number.
///
/// **One list, so one build.** `kernel_key` is over the joined string, so a
/// second spelling of this set would be a second kernel and nothing would say
/// so.
pub const TEST_KERNEL: &[&str] = &["boot-actuators", "test-actuators"];

/// Kernel builds the ordinary test suite is allowed to make.
pub const TEST_SUITE_KERNEL_BUILDS: [&str; 4] =
    ["", "boot-actuators,test-actuators", "fpu-save-nothing", "sched-check"];

/// The scheduler core's own asserts, compiled in: `toyos-sched/check`.
///
/// One name, read from here by the one test that boots it, for
/// [`TEST_KERNEL`]'s reason — a second spelling is a second kernel and nothing
/// would say so.
pub const SCHED_CHECK_KERNEL: &[&str] = &["sched-check"];

/// The kernel build used only by the harness's interactive debugger.
pub const DEBUG_KERNEL_BUILD: &str = "debug-wait";

/// Whether the test harness's current mode declares this kernel build.
pub fn harness_kernel_build_is_declared(features: &str, debug_wait: bool) -> bool {
    if debug_wait {
        features == DEBUG_KERNEL_BUILD
    } else {
        TEST_SUITE_KERNEL_BUILDS.contains(&features)
    }
}

/// Every name in which a process of this boot config can speak a console line
/// that is not the program under test's.
///
/// **Derived, never listed.** The point of reading it out of the config is that
/// a daemon added to `[boot] start` tomorrow is in this set the moment it
/// exists — a hardcoded list would let the next `netd`'s lines start deciding C
/// tests again, which is what task #84 was (`tests/common/console.rs`).
/// `/bin/init` itself is added by hand because it is the one speaker that is not a
/// `[programs]` key: it is the parent that starts every one of them, and it
/// speaks before any of them exists (`init: netd: no nic on this machine` is on
/// the console before netd is loaded).
///
/// The union of the two lists rather than `[boot] start` alone: a program the
/// config declares is a binary this image carries and a name init can be asked
/// to speak in, and the whole value of deriving the set is that it is the
/// config's answer rather than an author's.
///
/// **A program also speaks in the name of every device it claims, and that is
/// measured rather than supposed.** `userland/soundd/src/virtio.rs` writes
/// `virtio-sound: configured stream 0: 44100Hz 2ch s16le` — the driver layer
/// says which device is talking, not which program — and a plain
/// `tests/testcases` boot puts three such lines on the console before the test
/// runner is ready. `devices` is where those names are declared, so it is where
/// they are read from; `c_capture_ignores_daemon_lines` walks a real boot log
/// and reds on any line this set cannot account for, which is what keeps this
/// derivation honest as the tree grows.
///
/// `config` is the `system.toml` itself, not its directory.
pub fn console_speakers(config: &Path) -> std::collections::BTreeSet<String> {
    let parsed = parse_config(config);
    let mut names = std::collections::BTreeSet::new();
    for (program, entry) in parsed.programs {
        names.insert(program);
        names.extend(entry.devices);
    }
    names.extend(parsed.boot.start);
    names.insert("init".to_string());
    names
}

/// Every actuator `kernel/src/actuator.rs` declares, read out of the file that
/// declares them.
///
/// Read rather than listed here for `declared_kernel_features`' reason, one
/// layer in: deleting an actuator has to take its own command lines and its own
/// `BootOptions` with it, rather than leaving a name that quietly arms nothing.
/// The kernel's own parser refuses an unknown token as well, so this is the
/// early half of a two-sided answer and not the only one.
pub fn declared_actuators(root: &Path) -> Vec<String> {
    let path = root.join("kernel/src/actuator.rs");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    let body = text
        .split_once("\nactuators! {\n")
        .expect("kernel/src/actuator.rs has no `actuators!` block")
        .1;
    let body = body.split_once("\n}\n").expect("the `actuators!` block does not end").0;
    let names: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("///"))
        .filter_map(|line| line.split_once(" = \"")?.1.strip_suffix("\";").map(str::to_string))
        .collect();
    assert!(!names.is_empty(), "{} declares no actuators", path.display());
    names
}

/// Refuse to write an image whose kernel does not carry exactly the actuators
/// its feature set says it does.
///
/// [`assert_overflow_checked`]'s shape and for its reason: the property is
/// about the artifact, so the artifact is what is asked. Two builds quietly
/// becoming one build with test hooks in it is the failure mode this exists
/// for, and a convention nothing enforces is not a bar.
///
/// **Both directions, because one of them is a spelling of `true`.** A shipping
/// kernel must name none of them; the test kernel must name all of them, which
/// is what says the search works at all — measured on the two binaries this
/// build produced when the tree declared 47 of them: 0 of 47 at 3,829,440 bytes
/// and 47 of 47 at 4,247,272. The count below is read off the file, so adding
/// one moves the assertion and not this sentence.
///
/// A kernel built with no features is the shipping one, because that is what
/// "shipping" means here: `--kernel-feature`, `--kernel-param` and `--debug`
/// each say out loud that this image is not one.
fn assert_actuators_match_features(root: &Path, features: &str, kernel: &[u8]) {
    let want = match features {
        "" => false,
        f if f == TEST_KERNEL.join(",") => true,
        _ => return,
    };
    let names = declared_actuators(root);
    let named = |name: &String| {
        let needle = name.as_bytes();
        kernel.windows(needle.len()).any(|w| w == needle)
    };
    let wrong: Vec<&String> = names.iter().filter(|n| named(n) != want).collect();
    assert!(
        wrong.is_empty(),
        "the {} kernel {} {} of the {} actuators `kernel/src/actuator.rs` declares: {wrong:?}.\n\
         Everything under that file belongs to a kernel built with `boot-actuators`, and an \
         image that ships must not be able to be told to break.",
        if want { "test" } else { "shipping" },
        if want { "is missing" } else { "names" },
        wrong.len(),
        names.len(),
    );
}

/// The scheduler core's `feature = "check"` instruments, by their own text, and
/// the two kernels that must disagree about carrying them.
///
/// Every one of these is a `#[cfg(feature = "check")]` site in `toyos-sched`.
/// Two are asserts from `invariants::check_cpu` — invariant T's armed-timer
/// bound and the container-versus-state-word agreement. The third is the
/// pass-cost report (`cpu::PassCostReport::PREFIX`), which is a *measurement*
/// and not an assert: a pass's elapsed time includes any interval a hypervisor
/// took the CPU away, so it is recorded and gated in the harness rather than
/// panicked over. Their format strings are the only part of the check build
/// with a literal the linker keeps, which is what makes the artifact answerable
/// at all — and the report's literal is kept out of the shipping kernel by
/// nothing but dead-code elimination, which the `want == false` direction below
/// is what checks.
const SCHED_CHECK_LITERALS: [&str; 3] = [
    "sched-check pass-costs cpu=",
    "invariant T: cpu",
    "disagrees with its state word",
];

/// Refuse to write an image whose scheduler instruments do not match the
/// feature set that decides whether they exist.
///
/// [`assert_actuators_match_features`]'s shape and its reason: the property is
/// about the artifact, so the artifact is what is asked, and a convention
/// nothing enforces is not a bar. **Both directions, because one of them is a
/// spelling of `true`** — a shipping kernel must carry none of these, and the
/// `sched-check` kernel must carry all of them, which is what says the search
/// works at all.
///
/// This is the half of the check-build gate that a booted guest cannot supply.
/// A guest proves the asserts did not *fire* and the report was published; a
/// kernel with the feature quietly dropped proves the first of those too, and
/// rather more easily. Measured on the two binaries this build produces: 0 of 3
/// in the shipping kernel, 3 of 3 in the `sched-check` one.
fn assert_sched_check_matches_features(features: &str, kernel: &[u8]) {
    let want = match features {
        "" => false,
        f if f == SCHED_CHECK_KERNEL.join(",") => true,
        _ => return,
    };
    let named = |needle: &&str| {
        let needle = needle.as_bytes();
        kernel.windows(needle.len()).any(|w| w == needle)
    };
    let wrong: Vec<&&str> = SCHED_CHECK_LITERALS.iter().filter(|a| named(a) != want).collect();
    assert!(
        wrong.is_empty(),
        "the {} kernel {} {} of the {} scheduler check instruments: {wrong:?}.\n\
         `sched-check` forwards to `toyos-sched/check`, so a build that carries the feature \
         and not the instruments is a check build in name only — which is what a green \
         `sched_check_build` would then be certifying.",
        if want { "sched-check" } else { "shipping" },
        if want { "is missing" } else { "names" },
        wrong.len(),
        SCHED_CHECK_LITERALS.len(),
    );
}

/// Full build: kernel, bootloader, all programs, boot image. Returns the image.
pub fn build(
    root: &Path,
    debug: bool,
    boot: Boot,
    rebuild_toolchain: bool,
    claim_sysroot: bool,
    kernel_feature: &[String],
    kernel_param: &[String],
) -> PathBuf {
    // Before the locks: a misspelled name is the user's own command line and
    // has to come back now, not after this build has waited out every other
    // worktree's hold on the sysroot.
    let kernel_features = kernel_features(root, debug, kernel_feature, kernel_param);
    let cmdline = kernel_cmdline(root, kernel_param);

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
                &[],
                false,
            );
        }
        kernel_handle.join().expect("kernel build thread panicked");
        (
            stage_artifact(
                root,
                &root.join(format!("kernel/target/x86_64-unknown-none/{PROFILE}/kernel")),
                "kernel",
                kernel_key(&kernel_features),
            ),
            stage_artifact(
                root,
                &root.join(format!(
                    "bootloader/target/x86_64-unknown-uefi/{PROFILE}/bootloader.efi"
                )),
                "bootloader.efi",
                key_hash(&[PROFILE]),
            ),
        )
    };

    let initrd_bytes =
        build_and_assemble(root, &config, &path_env, &[], false);

    let kernel_bytes = fs::read(&kernel_art).expect("Failed to read staged kernel");
    assert_overflow_checked("kernel", &kernel_bytes);
    assert_actuators_match_features(root, &kernel_features, &kernel_bytes);
    assert_sched_check_matches_features(&kernel_features, &kernel_bytes);
    assert_kernel_is_softfloat(&path_env);
    let bl_bytes = fs::read(&bl_art).expect("Failed to read staged bootloader");
    let disk_bytes = image::create_boot_image(&kernel_bytes, &bl_bytes, &initrd_bytes, &cmdline);
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
/// A `cargo test` run boots ~76 machines, and most of those boots ask for an
/// image some earlier boot already built; the three `cargo` invocations then
/// take ~1.4 s between them to answer "nothing changed". In memory and never on
/// disk, so a run gets one answer for the tree it started against and the next
/// run asks cargo again.
///
/// Per part rather than per image, because a part is what a key can be true of:
/// the kernel is its feature set, the bootloader is its init list, the initrd is
/// its config and the caller's extra files. That is the same split
/// [`stage_artifact`] already writes into the artifact names, and it is what
/// makes this affordable — the kernels a full run builds share a handful of
/// initrds, and an initrd is hundreds of megabytes.
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
    kernel_params: &[&str],
    quiet: bool,
    extra_files: &[(String, Vec<u8>)],
) -> Vec<u8> {
    let config = parse_config(config_path);
    let features = kernel_features.join(",");
    // **The kernel is not keyed on this and that is the whole change.** A
    // parameter picks which actuator the one test kernel arms, so 45 builds
    // became two; keying the image on it is what keeps two boots asking for
    // different actuators from sharing one disk.
    assert!(
        kernel_params.is_empty() || kernel_features == TEST_KERNEL,
        "a boot asking for {kernel_params:?} must boot the test kernel, not {kernel_features:?}"
    );
    let cmdline = kernel_params.join(",");
    let kernel_key = kernel_key(&features);
    let bl_key = key_hash(&[PROFILE]);
    let initrd_key = initrd_key(config_path, extra_files);

    // Nothing left to build, so nothing for the lock, the toolchain check or the
    // staleness sweep to protect.
    if let (Some(kernel), Some(bl), Some(initrd)) =
        (KERNEL.get(kernel_key), BOOTLOADER.get(bl_key), INITRD.get(initrd_key))
    {
        return image::create_boot_image(&kernel, &bl, &initrd, &cmdline);
    }

    // A cache miss is shared setup, not a property of whichever test happened
    // to be first on this shard. Keep it on a separate clock until all missing
    // memo parts have been constructed. The fresh per-boot image below remains
    // outside the charge because every execution needs one.
    let build_timer = ArtifactBuildTimer::start();

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
                assert_actuators_match_features(root, &features, &bytes);
                assert_sched_check_matches_features(&features, &bytes);
                assert_kernel_is_softfloat(&path_env);
                bytes
            }
        });
        let bl = BOOTLOADER.get_or_build(bl_key, || {
            cargo_build(
                &root.join("bootloader"),
                "x86_64-unknown-uefi",
                &[],
                &path_env,
                &[],
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

    drop(build_timer);

    image::create_boot_image(&kernel_bytes, &bl_bytes, &initrd_bytes, &cmdline)
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

    #[test]
    fn an_artifact_build_is_not_part_of_a_test_execution_price() {
        let before = mark_artifact_build_time();
        ARTIFACT_BUILD_TIME.set(
            ARTIFACT_BUILD_TIME
                .get()
                .saturating_add(Duration::from_millis(70)),
        );
        assert_eq!(
            before.execution_part(Duration::from_millis(83)),
            Duration::from_millis(13)
        );

        let after = mark_artifact_build_time();
        assert_eq!(
            after.execution_part(Duration::from_millis(13)),
            Duration::from_millis(13)
        );

        // A coarse build clock must not underflow a very short failed outcome.
        ARTIFACT_BUILD_TIME.set(
            ARTIFACT_BUILD_TIME
                .get()
                .saturating_add(Duration::from_millis(70)),
        );
        assert_eq!(
            after.execution_part(Duration::from_millis(13)),
            Duration::ZERO
        );
    }

    /// A test that asks for no kernel feature boots the binary an image ships.
    ///
    /// **That claim is a file, not a resemblance.** A kernel is staged under
    /// [`kernel_key`] and read back from there, so the two paths agreeing about
    /// the key means one artifact — and the day something re-inserts a name
    /// between `BootOptions::kernel_features` and the build, this goes red.
    /// Until 2026-08-10 something did: `qemu::fold_inert` prepended
    /// `test-actuators` to every boot in the suite, so no test had ever booted
    /// the shipping kernel and nothing in the tree could have said so.
    ///
    /// The third assertion is the negative control: a key that ignored its
    /// features would satisfy the first two and certify nothing.
    #[test]
    fn a_boot_that_asks_for_no_feature_gets_the_shipping_kernel() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let shipping = kernel_features(root, false, &[], &[]);
        assert_eq!(shipping, "", "`cargo run` asks the kernel for {shipping:?}, not nothing");
        let harness = <&[&str]>::default().join(",");
        assert_eq!(
            kernel_key(&shipping),
            kernel_key(&harness),
            "a featureless boot and the shipping build stage different kernels"
        );
        assert_ne!(
            kernel_key(&shipping),
            kernel_key("test-actuators"),
            "the key ignores the features, so it cannot tell two kernels apart"
        );
    }

    /// Interactive debug mode deliberately builds one variant the ordinary
    /// suite does not, and the mode bit must not become a blanket exemption.
    #[test]
    fn debug_mode_declares_only_its_debug_kernel() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let debug = kernel_features(root, true, &[], &[]);
        assert_eq!(debug, DEBUG_KERNEL_BUILD);
        assert!(!TEST_SUITE_KERNEL_BUILDS.contains(&debug.as_str()));
        assert!(harness_kernel_build_is_declared(&debug, true));
        assert!(!harness_kernel_build_is_declared(&debug, false));
        assert!(!harness_kernel_build_is_declared(
            "fpu-save-nothing,debug-wait",
            true
        ));
        for suite_build in TEST_SUITE_KERNEL_BUILDS {
            assert!(harness_kernel_build_is_declared(suite_build, false));
            assert!(!harness_kernel_build_is_declared(suite_build, true));
        }
    }

    /// **An actuator is a boot parameter and never a kernel build.**
    ///
    /// A name that reappears in `kernel/Cargo.toml` is a 46th kernel, and the
    /// suite would build it without anything saying so — which is the state
    /// collapsing seven per-actuator features into `test-actuators` got out of.
    /// The two lists are read from the two files that declare them, so neither
    /// can be satisfied by editing this test.
    #[test]
    fn no_actuator_is_also_a_cargo_feature() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let actuators = declared_actuators(root);
        let features = declared_kernel_features(root);
        let both: Vec<&String> = actuators.iter().filter(|a| features.contains(a)).collect();
        assert!(both.is_empty(), "declared as both an actuator and a kernel feature: {both:?}");
        assert!(
            actuators.iter().all(|a| a.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')),
            "an actuator name is ASCII `[a-z0-9-]`, and these are not: {actuators:?}"
        );
    }

    /// The features a kernel build may still carry, and the whole list.
    ///
    /// **The gate on the count.** Each name here is a kernel `cargo test` may
    /// build beside the two, so adding one is a decision to pay the ~6.9 s of
    /// wall clock and ~29.6 s of CPU measured for one extra kernel build per
    /// full run after any kernel edit — and `boot-actuators` exists so that
    /// the answer is almost always a parameter instead.
    #[test]
    fn the_kernel_declares_only_the_builds_that_earned_one() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut declared = declared_kernel_features(root);
        declared.sort();
        assert_eq!(
            declared,
            [
                "boot-actuators",
                "debug-wait",
                "fpu-save-nothing",
                // `sched-tripwire`'s twin one layer down: a band of known bytes
                // on each side of every heap allocation, read back at `dealloc`
                // and — for the running task's kernel stack — at every pass. It
                // earns a build of its own because the bands change what
                // `GlobalAlloc::alloc` returns, which no boot parameter can
                // reach: an allocation minted under one arm and freed under the
                // other is a miscomputed base address. No suite builds it, so a
                // full run pays nothing and a boot storm asks for it by name.
                "heap-tripwire",
                // `wake-fence-off`'s twin, for the completion core: turned on
                // only by `kernel-loom`, to make the inbox's record
                // publication relaxed and prove `inbox` reds without the
                // release.
                "inbox-release-off",
                // The five below cost no kernel build at all, for
                // `wake-fence-off`'s reason: each is declared only so `cfg`
                // checking knows the name, and turned on only by
                // `kernel-loom`, one at a time, to relax the single edge its
                // named model rests on and prove that model reds without it.
                "lock-acquire-off",
                "log-commit-release-off",
                "loom",
                "reap-raise-relaxed",
                "sched-check",
                // The stray-write tripwire on the per-CPU `CpuSched` record: a
                // byte shadow taken and compared at both ends of the driver's
                // exclusive region, plus a walk of its three containers. It
                // earns a build of its own because what it watches cannot be
                // reached from a boot parameter — the shadow's subject is a
                // whole record and the walk's is a container, and both are
                // decided at compile time by a dependency's cargo feature, the
                // same wall `sched-check` is behind. No suite builds it: it is
                // not in `TEST_SUITE_KERNEL_BUILDS`, so a full run pays nothing
                // for it and a boot storm asks for it by name.
                "sched-tripwire",
                "shard-publish-relaxed",
                "shootdown-serve-relaxed",
                // The eighth loom control, and the first over a *contended*
                // acquire: `src/sleeplock.rs`'s two loads of `now` go
                // `Relaxed` and `kernel-loom/tests/sleep_lock.rs` reds. Costs
                // no kernel build, for the same reason as the six above it.
                "sleeplock-acquire-off",
                "test-actuators",
                // Costs no kernel build at all, for `loom`'s reason: declared
                // so `cfg` checking knows the name, and turned on only by
                // `kernel-loom` — to remove the log wake path's two `SeqCst`
                // fences and prove `log_wake` reds without them.
                "wake-fence-off",
            ],
            "the kernel declares a feature this list does not account for"
        );
    }

    /// Every negative control `kernel-loom/Cargo.toml` and
    /// `toyos-sched/loom/Cargo.toml` declare — every feature name besides the
    /// structural ones both crates carry for other reasons.
    ///
    /// `loom` selects loom's instrumented atomics; `check`, `protocol-port` and
    /// `tripwire` mirror `toyos-sched`'s own features so the shared sources
    /// compile identically and name nothing a model turns on. Everything else
    /// declared in either file is, by construction, a `--features <name>`
    /// command that must red a named model — each file's own comment beside the
    /// name carries the argument for why.
    fn declared_loom_controls(root: &Path) -> Vec<(&'static str, String)> {
        const NOT_A_CONTROL: &[&str] = &["loom", "check", "protocol-port", "tripwire", "default"];
        let mut out = Vec::new();
        for (crate_name, manifest) in [
            ("kernel-loom", "kernel-loom/Cargo.toml"),
            ("toyos-sched-loom", "toyos-sched/loom/Cargo.toml"),
        ] {
            let path = root.join(manifest);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
            let parsed: KernelManifest = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()));
            for name in parsed.features.into_keys() {
                if !NOT_A_CONTROL.contains(&name.as_str()) {
                    out.push((crate_name, name));
                }
            }
        }
        out
    }

    /// **A control nobody runs is a control nobody has shown can fail.** Every
    /// name [`declared_loom_controls`] finds must appear as `--features <name>`
    /// somewhere in `host-tests.yml` — the one place these are wired, by every
    /// existing comment's own account — or a new control can be declared and
    /// run nowhere, silently, which is exactly how five of `kernel-loom`'s six
    /// and `toyos-sched-loom`'s `doorbell-kick-relaxed` went unwired until
    /// 2026-08-17: nothing before this test required a declared control to
    /// have a step.
    ///
    /// A substring check and not a YAML parse, for `src/ci.rs`'s `nameless`
    /// reason: the shape a step's command line has is fixed, and a real parse
    /// would have to reconstruct multi-line `run:` blocks to find it in.
    #[test]
    fn every_loom_control_is_wired_into_host_tests() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let workflow = fs::read_to_string(root.join(".github/workflows/host-tests.yml"))
            .expect("host-tests.yml is readable");
        let mut missing = Vec::new();
        for (crate_name, control) in declared_loom_controls(root) {
            let needle = format!("--features {control}");
            if !workflow.contains(&needle) {
                missing.push(format!("{crate_name}: {control} ({needle:?} not found in host-tests.yml)"));
            }
        }
        assert!(
            missing.is_empty(),
            "a loom negative control is declared with no CI step running it — \
             wire it into .github/workflows/host-tests.yml beside the others:\n  {}",
            missing.join("\n  ")
        );
    }

    /// `test-actuators` is one name and pulls in nothing.
    ///
    /// The seven it replaced were seven kernel builds differing only in which
    /// unreachable `SYS_DEBUG` arm they carried. Re-introducing one as an implied
    /// feature rebuilds that, silently, and only this notices.
    #[test]
    fn the_actuator_umbrella_is_a_leaf() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let text = fs::read_to_string(root.join("kernel/Cargo.toml")).expect("read the manifest");
        let manifest: KernelManifest = toml::from_str(&text).expect("parse the manifest");
        let implied = manifest
            .features
            .get("test-actuators")
            .expect("the kernel declares no `test-actuators`");
        assert!(
            implied.is_empty(),
            "`test-actuators` implies {implied:?}, so it is several kernel builds again"
        );
    }

    /// No image this repository ships starts sshd.
    ///
    /// It listens on every interface and authenticates against a file that is
    /// absent on a fresh install, so on a default boot it would be a port that
    /// accepts connections and refuses all of them. Whoever wants it runs
    /// `/bin/sshd` themselves. It stays in `[programs]` — the gate is on what
    /// init starts, not on the binary being present.
    #[test]
    fn no_shipped_boot_config_starts_sshd() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for boot in [Boot::Normal, Boot::Diag, Boot::Console] {
            let config = boot.config();
            let start = parse_config(&root.join(config)).boot.start;
            assert!(
                !start.iter().any(|p| p == "sshd"),
                "{config} starts sshd: {start:?}",
            );
        }
    }

    /// **Every config with a `[boot] start` runs `/bin/logd`, and `logread` is
    /// held by exactly the programs that read a cursor.**
    ///
    /// The kernel writes no file — `/bin/logd` owns `/log` and reads records off
    /// a cursor — so a boot config that does not start `logd` is an image whose
    /// log partition stays empty for the whole of that boot — and on
    /// the machine this subsystem exists for, a T14 with no serial port, that is
    /// the boot with no record of itself anywhere. A thirteenth config added
    /// later fails the first clause **by default**, which is the direction this
    /// bound has to fail in.
    ///
    /// The second clause is the capability half: `logread` is
    /// `Rights::LOG | Rights::WAIT` on a `SysCap` duplicate, which is authority
    /// over every record every CPU wrote, and a right with no caller is a
    /// capability handed out for a plan. Two programs read a cursor —
    /// `/bin/logd`, which writes the file, and `test-runner`, which runs the
    /// conservation gates inside itself — so those two carry it and nothing
    /// else may. `/bin/console` is the near miss: it *could* show this boot's
    /// records live off a cursor instead of seeding from the previous boot's
    /// files, and it does not hold the right until something in it reads one.
    ///
    /// It reads the **parsed** `ProgramConfig` and never the file text: a grep
    /// over the TOML would pass on a row that is commented out and on a key
    /// `serde` never saw.
    #[test]
    fn every_boot_config_runs_logd() {
        const READERS: &[&str] = &["logd", "test-runner"];
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for config in ALL_CONFIGS {
            let parsed = parse_config(&root.join(config));
            assert!(
                parsed.boot.start.iter().any(|p| p == "logd"),
                "{config} declares `[boot] start = {:?}` and no `logd` in it, so this image's \
                 /log is empty for the whole boot",
                parsed.boot.start,
            );
            assert!(
                parsed.programs.contains_key("logd"),
                "{config} starts `logd` and has no `[programs.logd]` row to say what it holds",
            );
            for (name, program) in &parsed.programs {
                let holds = program.syscap.iter().any(|s| s == "logread");
                assert_eq!(
                    holds,
                    READERS.contains(&name.as_str()),
                    "{config}: `{name}` {} `logread`, and the programs that read a cursor are \
                     exactly {READERS:?}",
                    if holds { "holds" } else { "does not hold" },
                );
            }
        }
    }

    /// The twelve `system.toml` files this repository builds an image from.
    /// `every_shipped_boot_config_is_covered` asserts this equals what a walk of
    /// the tree finds, so a config added without a gate row reds rather than
    /// slipping through uncovered.
    const ALL_CONFIGS: &[&str] = &[
        "system.toml",
        "diag/system.toml",
        "console/system.toml",
        "tests/desktopcase/system.toml",
        "tests/desktopaudiocase/system.toml",
        "tests/doomcase/system.toml",
        "tests/doommusiccase/system.toml",
        "tests/logrotatecase/system.toml",
        "tests/metalcase/system.toml",
        "tests/netcase/system.toml",
        "tests/sshdcase/system.toml",
        "tests/testcases/system.toml",
    ];

    fn load(cfg: &str) -> SystemConfig {
        parse_config(&Path::new(env!("CARGO_MANIFEST_DIR")).join(cfg))
    }

    /// Every name a program `receives` must be served or provided by some
    /// program in the *same* config, or served by init. The build-time form of
    /// "a client cannot name a service the system does not have", and the gate
    /// with the sharpest teeth: no guest, no mutated tree.
    fn receives_have_providers(cfg: &SystemConfig) -> Result<(), String> {
        let mut providers: Vec<&str> = INIT_SERVED.to_vec();
        for prog in cfg.programs.values() {
            providers.extend(prog.serves.iter().map(String::as_str));
            providers.extend(prog.provides.iter().map(String::as_str));
        }
        for (name, prog) in &cfg.programs {
            for r in &prog.receives {
                if !providers.contains(&r.as_str()) {
                    return Err(format!(
                        "program `{name}` receives `{r}`, which no program serves or provides"
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn every_receives_names_a_provider() {
        for cfg in ALL_CONFIGS {
            receives_have_providers(&load(cfg)).unwrap_or_else(|e| panic!("{cfg}: {e}"));
        }
        let bad: SystemConfig =
            toml::from_str("init = []\n[programs.client]\nreceives = [\"ghost\"]\n").unwrap();
        assert!(receives_have_providers(&bad).is_err());
    }

    /// A `serves` name is one port machine-wide; a `provides` name is one port
    /// per instance. A name declared both ways is a config where init makes a
    /// port nobody accepts from while the real one is made elsewhere.
    fn provides_disjoint_from_serves(cfg: &SystemConfig) -> Result<(), String> {
        let serves: Vec<&str> = cfg
            .programs
            .values()
            .flat_map(|p| p.serves.iter().map(String::as_str))
            .collect();
        for (name, prog) in &cfg.programs {
            for p in &prog.provides {
                if serves.contains(&p.as_str()) {
                    return Err(format!(
                        "`{p}` is both a serves name and `{name}`'s provides name"
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn a_provides_name_is_never_also_a_serves_name() {
        for cfg in ALL_CONFIGS {
            provides_disjoint_from_serves(&load(cfg)).unwrap_or_else(|e| panic!("{cfg}: {e}"));
        }
        let bad: SystemConfig = toml::from_str(
            "init = []\n[programs.a]\nserves = [\"x\"]\n[programs.b]\nprovides = [\"x\"]\n",
        )
        .unwrap();
        assert!(provides_disjoint_from_serves(&bad).is_err());
    }

    /// A device class init can mint exactly one claim for, so two programs
    /// naming the same class is a config init cannot satisfy — a runtime
    /// first-come race today.
    fn one_claimant_per_device(cfg: &SystemConfig) -> Result<(), String> {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (name, prog) in &cfg.programs {
            for d in &prog.devices {
                if let Some(prev) = seen.insert(d, name) {
                    return Err(format!(
                        "device class `{d}` is claimed by both `{prev}` and `{name}`"
                    ));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn every_device_class_has_at_most_one_claimant() {
        for cfg in ALL_CONFIGS {
            one_claimant_per_device(&load(cfg)).unwrap_or_else(|e| panic!("{cfg}: {e}"));
        }
        let bad: SystemConfig = toml::from_str(
            "init = []\n[programs.a]\ndevices = [\"framebuffer\"]\n\
             [programs.b]\ndevices = [\"framebuffer\"]\n",
        )
        .unwrap();
        assert!(one_claimant_per_device(&bad).is_err());
    }

    /// A class name the ABI does not know renders fine and leaves init with a
    /// `devices` entry it cannot mint — a dead machine for a typo, where this is
    /// a red in milliseconds. Same for a `syscap` right.
    fn names_only_real_capabilities(cfg: &SystemConfig) -> Result<(), String> {
        for (name, prog) in &cfg.programs {
            for class in &prog.devices {
                if toyos_manifest::DeviceType::from_class_name(class).is_none() {
                    return Err(format!("`{name}` names device class `{class}`, which is not one"));
                }
            }
            toyos_manifest::syscap_rights(&prog.syscap)
                .map_err(|e| format!("`{name}`: {e}"))?;
        }
        Ok(())
    }

    #[test]
    fn every_declared_capability_is_one_the_abi_has() {
        for cfg in ALL_CONFIGS {
            names_only_real_capabilities(&load(cfg)).unwrap_or_else(|e| panic!("{cfg}: {e}"));
        }
        let bad_class: SystemConfig =
            toml::from_str("[programs.a]\ndevices = [\"gpu\"]\n").unwrap();
        assert!(names_only_real_capabilities(&bad_class).is_err());
        let bad_right: SystemConfig =
            toml::from_str("[programs.a]\nsyscap = [\"root\"]\n").unwrap();
        assert!(names_only_real_capabilities(&bad_right).is_err());
    }

    fn claims_no_device(cfg: &SystemConfig) -> Result<(), String> {
        for (name, prog) in &cfg.programs {
            if !prog.devices.is_empty() {
                return Err(format!("program `{name}` claims {:?}", prog.devices));
            }
        }
        Ok(())
    }

    /// The diagnostic image's whole reason for existing: nothing in it can claim
    /// the framebuffer, so the kernel's boot log stays on the panel. `/bin/init`
    /// is in every image and could reach a device, so the property becomes "the
    /// config declares no `devices`" — checkable here for the first time.
    #[test]
    fn no_diag_program_claims_the_screen() {
        claims_no_device(&load("diag/system.toml"))
            .unwrap_or_else(|e| panic!("diag/system.toml: {e}"));
        let bad: SystemConfig =
            toml::from_str("init = []\n[programs.x]\ndevices = [\"framebuffer\"]\n").unwrap();
        assert!(claims_no_device(&bad).is_err());
    }

    /// `[boot] start` names program keys, so a typo is a build error rather than
    /// a refusal `/bin/init` reports at boot.
    fn started_programs_are_declared(cfg: &SystemConfig) -> Result<(), String> {
        for name in &cfg.boot.start {
            if !cfg.programs.contains_key(name) {
                return Err(format!("[boot] start names `{name}`, not a [programs] key"));
            }
        }
        Ok(())
    }

    #[test]
    fn every_started_program_is_declared() {
        for cfg in ALL_CONFIGS {
            started_programs_are_declared(&load(cfg)).unwrap_or_else(|e| panic!("{cfg}: {e}"));
        }
        let bad: SystemConfig = toml::from_str("init = []\n[boot]\nstart = [\"ghost\"]\n").unwrap();
        assert!(started_programs_are_declared(&bad).is_err());
    }

    fn walk_configs(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                let skip = matches!(name.to_str(), Some("target") | Some("rust"))
                    || name.to_string_lossy().starts_with('.');
                if !skip {
                    walk_configs(&path, root, out);
                }
            } else if name == "system.toml" {
                out.push(path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }

    /// `ALL_CONFIGS` is the list the gates above iterate; a config added without
    /// a row would leave a hole in that coverage. Assert the list is exactly
    /// what a walk of the tree finds, so it cannot silently drift.
    #[test]
    fn every_shipped_boot_config_is_covered() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        walk_configs(root, root, &mut found);
        found.sort();
        let mut expected: Vec<String> = ALL_CONFIGS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(found, expected);
    }
}
