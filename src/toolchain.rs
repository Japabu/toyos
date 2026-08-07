use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::buildlock;
use crate::buildlock::Scope;
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

/// Which checkout holds the `rust/` submodule and the toolchain built from it.
pub enum Owner {
    Us,
    /// The primary checkout, named so a refusal can point at it.
    Elsewhere(PathBuf),
}

/// One `rust/` per repository, in the primary checkout, and every worktree
/// compiles against it.
///
/// Not a policy — an affordance. A second checkout of that submodule is a
/// 913 MiB clone (git gives a linked worktree its own, sharing no objects), and
/// a second `build/` beside it is 47 GiB. `git worktree add` leaves `rust/` an
/// empty stub, and leaving it empty is what keeps `git status` clean: git
/// refuses a symlink where a gitlink belongs, and errors out of every command
/// rather than just that one.
pub fn owner(root: &Path) -> Owner {
    let primary = crate::primary_checkout(root);
    let same = fs::canonicalize(root).map(|r| r == primary).unwrap_or(false);
    if same {
        return Owner::Us;
    }
    assert!(
        primary.join("rust/x.py").exists(),
        "{} is a linked worktree, so the shared rust checkout should be at {}, \
         and there is nothing there.\n\
         A repository laid out with --separate-git-dir cannot be located this way.",
        root.display(),
        primary.join("rust").display()
    );
    Owner::Elsewhere(primary)
}

/// The shared rust checkout: source, `build/`, and the sysroot every worktree
/// compiles against.
pub fn rust_dir(root: &Path) -> PathBuf {
    match owner(root) {
        Owner::Us => root.join("rust"),
        Owner::Elsewhere(primary) => primary.join("rust"),
    }
}

/// The per-worktree sources that end up *inside* the shared sysroot: std links
/// `toyos-abi` and `toyos`, and `libtoyos_c.a` is `userland/libc`.
const SYSROOT_SOURCES: [&str; 3] = ["toyos-abi/src", "toyos/src", "userland/libc/src"];

/// Of those, the ones a change to obliges an std rebuild.
const STD_SOURCES: [&str; 2] = ["toyos-abi/src", "toyos/src"];

/// What the sysroot on disk was built from.
fn witness_path(rust_dir: &Path) -> PathBuf {
    rust_dir.join("build/toyos-sysroot-witness")
}

/// *Who* built it, and when.
///
/// The witness says a refused worktree disagrees; it cannot say with whom, and
/// "merge the change that is already in the sysroot" is not an instruction
/// anyone can follow without that. Written by every process that writes the
/// witness, so it never names a worktree that is no longer the answer.
fn claimant_path(rust_dir: &Path) -> PathBuf {
    rust_dir.join("build/toyos-sysroot-claimant")
}

/// Record this checkout as the one the sysroot was built from.
fn record_claimant(root: &Path, rust_dir: &Path) {
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "an unknown branch".to_string(), |b| b.trim().to_string());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Advisory, like every holder note in `buildlock`: it improves a message and
    // decides nothing, so failing to write it must not fail a build.
    let _ = fs::write(
        claimant_path(rust_dir),
        format!("{now} {} {branch}\n", root.display()),
    );
}

/// What [`record_claimant`] last wrote: the checkout, and it as a sentence.
fn claimant(rust_dir: &Path) -> Option<(PathBuf, String)> {
    let text = fs::read_to_string(claimant_path(rust_dir)).ok()?;
    let mut parts = text.trim().splitn(3, ' ');
    let (when, where_, branch) = (parts.next()?, parts.next()?, parts.next()?);
    let when: u64 = when.parse().ok()?;
    let ago = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().saturating_sub(when));
    Some((
        PathBuf::from(where_),
        format!("{where_} (branch {branch}), {} minutes ago", ago / 60),
    ))
}

fn holder(rust_dir: &Path) -> String {
    claimant(rust_dir).map_or_else(|| "a checkout that left no record".to_string(), |(_, s)| s)
}

/// Whether this checkout has an ABI of its own at all.
///
/// **The question `--claim-sysroot` never asked, and the whole of task #134.**
/// On 2026-08-04 five worktrees whose `toyos-abi` and `toyos` were byte-
/// identical to main's were refused by a sysroot one worktree had legitimately
/// claimed for an unlanded SDK change — and each one's attempt to unblock itself
/// by claiming refused the only worktree that needed it. A checkout with no
/// delta of its own has nothing to build a sysroot *from*: claiming would put
/// main's sources there, which is a fight it cannot win and does not need to
/// have, because the holder landing ends the refusal by itself.
///
/// Committed and uncommitted alike, because an uncommitted struct layout is
/// exactly as incompatible as a committed one — and because that is how the
/// holder on 2026-08-04 was found.
#[derive(PartialEq, Debug)]
enum Standing {
    /// This checkout's witnessed sources differ from `main`'s. It has something
    /// only it can build.
    Diverged,
    /// They are `main`'s. Whatever is wrong with the sysroot, this checkout is
    /// not the answer to it.
    MatchesMain,
    /// git could not say — no `main` ref, not a repository. Never read as
    /// standing: a claim is destructive and an unanswered question is not
    /// permission.
    Unknown,
}

/// **Against the merge base, not against main's tip.** `git diff main` is
/// symmetric: a worktree that has merely not merged somebody else's landed ABI
/// change looked exactly like one holding an unlanded change of its own, and
/// could claim — rebuilding the shared sysroot from sources *older* than main's
/// and refusing the checkout whose change had already landed. `main...HEAD` asks
/// what this branch added and answers nothing for a checkout that is only
/// behind, whose whole answer is to merge.
///
/// The working tree is asked separately and with `status` rather than `diff`,
/// because a new file in `toyos-abi/src` changes the witness and no `diff`
/// against a commit reports an untracked one.
/// What a claimant is told to do about the fact that a claim blocks everybody.
///
/// One sysroot serves N worktrees, so a checkout with a real ABI change takes a
/// turn during which the others cannot build at all — measured twice on
/// 2026-08-07 at about 35 and about 50 minutes, both of them a whole task long
/// because the claim was held for the whole task. It does not have to be: the
/// ABI half of a change is usually a few lines that compile on their own, and
/// landing it by itself makes the window one landing instead. Applied once that
/// day, successfully. Said here rather than only in the spec, because the
/// refusal is what an agent in this situation is actually reading.
const CLAIM_WINDOW: &str = "\
    The window is yours to make small: land the toyos-abi/toyos change on its own commit \
    first, before the work that depends on it. Every other worktree is refused for as long \
    as you hold the sysroot, and holding it for a whole task is what cost ~35 and ~50 \
    minutes of eight agents' time on 2026-08-07 (specs/worktrees.md §3.2).";

fn standing(root: &Path) -> Standing {
    let mut ahead = vec!["diff", "--quiet", "main...HEAD", "--"];
    ahead.extend(SYSROOT_SOURCES);
    let committed =
        Command::new("git").args(&ahead).current_dir(root).status().ok().and_then(|s| s.code());

    let mut local = vec!["status", "--porcelain", "--"];
    local.extend(SYSROOT_SOURCES);
    let uncommitted = Command::new("git").args(&local).current_dir(root).output().ok();

    match (committed, uncommitted) {
        (Some(1), _) => Standing::Diverged,
        (Some(0), Some(out)) if out.status.success() => {
            if out.stdout.is_empty() { Standing::MatchesMain } else { Standing::Diverged }
        }
        _ => Standing::Unknown,
    }
}

/// Fingerprint the sources that get compiled into the shared sysroot.
///
/// By content and by repository-relative path, where [`stamps`] uses mtime and
/// absolute path. Two checkouts of one commit agree on the first and on neither
/// of the second, and the question here is across checkouts: a worktree whose
/// `toyos-abi` differs from the one the sysroot holds would compile its kernel
/// against another worktree's struct layouts. Nothing downstream can catch
/// that — the build succeeds and the guest corrupts memory.
fn witness(root: &Path) -> String {
    let mut lines = Vec::new();
    for tree in SYSROOT_SOURCES {
        let mut files = Vec::new();
        collect_sources(&root.join(tree), &mut files);
        files.sort();
        for path in files {
            let data = fs::read(&path)
                .unwrap_or_else(|e| panic!("witness {}: {e}", path.display()));
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut hasher);
            lines.push(format!("{}:{:016x}", rel.display(), hasher.finish()));
        }
    }
    lines.join("\n")
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !name.starts_with('.') && name != "target" {
                collect_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs" || e == "toml" || e == "h") {
            out.push(path);
        }
    }
}

/// The lines of a witness belonging to `trees`.
fn witness_subset(text: &str, trees: &[&str]) -> String {
    text.lines()
        .filter(|l| trees.iter().any(|t| l.starts_with(t)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which of [`SYSROOT_SOURCES`] this worktree disagrees with the sysroot about.
fn differing_trees(recorded: Option<&str>, current: &str) -> String {
    let Some(recorded) = recorded else {
        return "nothing recorded what the sysroot was built from".to_string();
    };
    let names: Vec<&str> = SYSROOT_SOURCES
        .iter()
        .copied()
        .filter(|t| witness_subset(recorded, &[t]) != witness_subset(current, &[t]))
        .collect();
    if names.is_empty() {
        return "the record is malformed".to_string();
    }
    names.join(", ")
}

/// Where the machine-global `toyos` rustup toolchain currently points.
fn rustup_link() -> Option<PathBuf> {
    let home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".rustup")))?;
    fs::read_link(home.join("toolchains/toyos")).ok()
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
///
/// Only the primary checkout runs the steps that write the shared tree. A
/// linked worktree checks that what is there is the sysroot its sources belong
/// to, and says so by name when it is not.
pub fn ensure(
    root: &Path,
    force_rebuild: bool,
    claim_sysroot: bool,
    lock: &mut buildlock::Held,
) {
    let rust_dir = rust_dir(root);
    let stamps_dir = root.join("target/stamps");
    fs::create_dir_all(&stamps_dir).ok();

    // Needed as the cross-linker for bootstrap and for every build.
    let ld_src = root.join("toyos-ld/src");
    let ld_stamp = stamps_dir.join("linker.stamp");
    lock.act_if(
        Scope::Worktree,
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
        Scope::Worktree,
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

    if let Owner::Elsewhere(primary) = owner(root) {
        adopt_shared_sysroot(root, &rust_dir, &primary, force_rebuild, claim_sysroot, lock);
        return;
    }

    // **The primary's ordinary build is a claim too, and it used to be a silent
    // one.** `std_sources_stale` is true for the owner of the toolchain in
    // exactly two situations: its own sources changed, or a worktree claimed the
    // sysroot for something that is not on main yet. The second is a lease, and
    // rebuilding over it takes the sysroot from the one checkout that cannot
    // merge its way out — which is the other half of the flip-flop watched on
    // 2026-08-05, the witness rewritten at 00:23, 00:26 and 00:47 while a
    // worktree with a real SDK change and a build in the primary took it from
    // each other. Told apart by who wrote the witness, so no cross-checkout git
    // is needed; a sysroot the primary itself built names the primary.
    if !claim_sysroot && std_sources_stale(root, &rust_dir) {
        if let Some((who, since)) = claimant(&rust_dir).filter(|(who, _)| who != root) {
            panic!(
                "the shared sysroot is held by {}, for a change that is not on main yet, \
                 and rebuilding it here would take it from the one checkout that cannot \
                 merge its way out.\n\
                 It was claimed from {since}.\n\
                 Wait for it to land — this refusal ends by itself, because main will then \
                 carry what the sysroot holds. `--claim-sysroot` takes it back deliberately.",
                who.display(),
            );
        }
    }

    let compiler_stamp = stamps_dir.join("compiler.stamp");
    let std_stamp = stamps_dir.join("std.stamp");
    let hosted_stamp = stamps_dir.join("hosted-rustc.stamp");
    let libc_stamp = stamps_dir.join("toyos-libc.stamp");
    lock.act_if(
        Scope::Global,
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
                || std_sources_stale(root, &rust_dir)
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
        Scope::Global,
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
        Scope::Global,
        "link the toyos rustup toolchain",
        || link_stale(&stage2).then_some(()),
        |()| run("rustup", &["toolchain", "link", "toyos", stage2.to_str().unwrap()]),
    );

    // Before any cargo build uses the toolchain, otherwise cargo may
    // fingerprint an incomplete sysroot on first run.
    lock.act_if(
        Scope::Global,
        "add the host target to the ToyOS sysroot",
        || host_target_missing(&rust_dir).then_some(()),
        |()| link_host_target(&rust_dir),
    );

    lock.act_if(
        Scope::Global,
        "build toyos-libc for the sysroot",
        || crate::libc::stale(root, &rust_dir).then_some(()),
        |()| crate::libc::build(root, &rust_dir),
    );

    // Last, so it describes a sysroot that is finished. Every other checkout
    // reads this to decide whether the sysroot is one it may compile against.
    lock.act_if(
        Scope::Global,
        "record what the sysroot was built from",
        || {
            let want = witness(root);
            (fs::read_to_string(witness_path(&rust_dir)).ok().as_deref() != Some(want.as_str()))
                .then_some(want)
        },
        |want| {
            fs::write(witness_path(&rust_dir), &want)
                .unwrap_or_else(|e| panic!("write {}: {e}", witness_path(&rust_dir).display()));
            record_claimant(root, &rust_dir);
        },
    );
}

/// Whether the sysroot's std was built from other `toyos-abi`/`toyos` sources
/// than the ones in this checkout.
///
/// Replaces the mtime stamps these two trees used to have. Those could not
/// answer the question across checkouts — two worktrees of one commit hold
/// identical bytes at different paths with different mtimes — and answered it
/// wrongly within one, since `git checkout` rewriting an unchanged file bought
/// a full std rebuild.
fn std_sources_stale(root: &Path, rust_dir: &Path) -> bool {
    let Ok(recorded) = fs::read_to_string(witness_path(rust_dir)) else {
        // No record is ignorance, not disagreement. The primary checkout is the
        // only thing that writes this sysroot, so what is on disk is what it
        // built from the sources beside it; the step below records that, and
        // every build after this one has an answer to compare against. A linked
        // worktree reads the same absence as a refusal, because for it the
        // sysroot is somebody else's artifact and it has no such standing.
        return false;
    };
    witness_subset(&recorded, &STD_SOURCES) != witness_subset(&witness(root), &STD_SOURCES)
}

/// Use the sysroot the primary checkout built, once it is established that it
/// is the one this worktree's sources belong to.
///
/// The check has teeth because the failure it prevents has none of its own: a
/// worktree whose `toyos-abi` differs from the sysroot's still compiles, still
/// links, and still boots — into a guest whose syscall arguments land at the
/// wrong offsets.
fn adopt_shared_sysroot(
    root: &Path,
    rust_dir: &Path,
    primary: &Path,
    force_rebuild: bool,
    claim: bool,
    lock: &mut buildlock::Held,
) {
    assert!(
        !force_rebuild,
        "--rebuild-toolchain would replace the toolchain at {}, which every \
         worktree of this repository compiles against.\nRun it in {}.",
        rust_dir.display(),
        primary.display()
    );

    let stage2 = rust_dir.join(format!("build/{}/stage2", host_triple()));
    assert!(
        stage2.join("bin/rustc").exists(),
        "there is no toolchain to build against: {} does not exist.\n\
         The primary checkout builds it — run `cargo run -- --build-only` in {} first.",
        stage2.display(),
        primary.display()
    );
    let linked = rustup_link();
    assert!(
        linked.as_deref() == Some(stage2.as_path()),
        "the rustup toolchain `toyos` points at {}, not at {}.\n\
         Only {} links it; something else has taken the name.",
        linked.map_or_else(|| "nothing".to_string(), |p| p.display().to_string()),
        stage2.display(),
        primary.display()
    );

    let want = witness(root);
    let recorded = fs::read_to_string(witness_path(rust_dir)).ok();
    if recorded.as_deref() == Some(want.as_str()) {
        return;
    }

    let differs = differing_trees(recorded.as_deref(), &want);
    // **A checkout with no ABI of its own may not claim, whether or not it asked
    // to.** The refusal above is right; the instruction that used to follow it
    // was not. On 2026-08-04 five worktrees identical to main were refused by a
    // sysroot one worktree legitimately held for an unlanded SDK change, and
    // every one of them read "pass --claim-sysroot" as the way out. Each such
    // claim refuses the only checkout that cannot merge its way out, and the
    // holder claims back: six landing attempts, four witness rewrites in 38
    // minutes, one gate dead with 156 refusals.
    // `panic!` and not `assert_ne!`, because the message is the whole product of
    // this branch and `left: MatchesMain / right: MatchesMain` under it is
    // noise an agent has to read past.
    match standing(root) {
        Standing::MatchesMain => panic!(
            "this worktree and the shared sysroot at {} disagree about {differs}, so a build \
             here would link its kernel against another checkout's struct layouts.\n\
             Your toyos-abi and toyos are byte-identical to main's, so there is nothing here \
             to claim with: the sysroot belongs to {}, for a change that is not on main yet.\n\
             **Wait for it to land and merge main.** This refusal then ends by itself.\n\
             Do not pass --claim-sysroot. It would rebuild the sysroot from main's sources, \
             which is what every other worktree already has, and refuse the one checkout \
             that cannot merge its way out.",
            rust_dir.display(),
            holder(rust_dir),
        ),
        Standing::Unknown => panic!(
            "this worktree and the shared sysroot at {} disagree about {differs}, and git \
             cannot say whether this checkout differs from main — so whether it has any \
             standing to claim is unknown, and a claim is destructive.\n\
             Check `git diff main -- {}`.",
            rust_dir.display(),
            SYSROOT_SOURCES.join(" "),
        ),
        Standing::Diverged => {}
    }

    assert!(
        claim,
        "this worktree and the shared sysroot at {} disagree about {}, so a build \
         here would link its kernel against another checkout's struct layouts.\n\
         It was built from {}.\n\
         This worktree does differ from main in those trees, so it is the one checkout \
         that cannot merge its way out: merge main first if that is enough, otherwise \
         pass --claim-sysroot to rebuild the sysroot from here — which makes every other \
         worktree wait for you to land.\n\
         {CLAIM_WINDOW}",
        rust_dir.display(),
        differs,
        holder(rust_dir),
    );

    // Said before the work rather than after, because the work is minutes long
    // and what it does to every other worktree is not reversible by waiting.
    eprintln!(
        "Claiming the shared sysroot for {}.\n\
         It currently belongs to {}. Land this change as soon as it is ready: until you \
         do, every other worktree is refused, and none of them can fix that from its end.\n\
         {CLAIM_WINDOW}",
        root.display(),
        holder(rust_dir),
    );

    lock.act_if(
        Scope::Global,
        "rebuild the shared sysroot from a linked worktree",
        || {
            (fs::read_to_string(witness_path(rust_dir)).ok().as_deref() != Some(want.as_str()))
                .then_some(())
        },
        |()| {
            eprintln!("Rebuilding the shared sysroot from {}...", root.display());
            rebuild_std(root, rust_dir);
            crate::libc::build(root, rust_dir);
            fs::write(witness_path(rust_dir), &want)
                .unwrap_or_else(|e| panic!("write {}: {e}", witness_path(rust_dir).display()));
            record_claimant(root, rust_dir);
        },
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
///
/// Reached only from the primary checkout, which is what makes the window above
/// a window of one: a linked worktree that re-linked would point the name at a
/// stage2 nobody else has.
fn link_stale(stage2: &Path) -> bool {
    rustup_link().is_none_or(|current| current != stage2)
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
fn host_target_missing(rust_dir: &Path) -> bool {
    let toyos_sysroot = rust_dir.join("build/x86_64-unknown-toyos/stage2/lib/rustlib");
    toyos_sysroot.exists() && !toyos_sysroot.join(host_triple()).exists()
}

fn link_host_target(rust_dir: &Path) {
    let host = host_triple();
    let host_target_dir = rust_dir
        .join("build/x86_64-unknown-toyos/stage2/lib/rustlib")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository with `main` and the three witnessed trees on it.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("toyos-standing-{name}"));
        let _ = fs::remove_dir_all(&dir);
        for tree in SYSROOT_SOURCES {
            fs::create_dir_all(dir.join(tree)).unwrap();
            fs::write(dir.join(tree).join("lib.rs"), b"pub struct A;\n").unwrap();
        }
        fs::create_dir_all(dir.join("kernel/src")).unwrap();
        fs::write(dir.join("kernel/src/main.rs"), b"fn main() {}\n").unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "main"]);
        dir
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "commit.gpgsign=false", "-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git")
            .success();
        assert!(ok, "git {args:?} in {}", dir.display());
    }

    /// **The gate on task #134's sharpened rule.** Five of the six worktrees
    /// refused on 2026-08-04 were byte-identical to main in these trees, and
    /// every one of them was told to claim.
    #[test]
    fn a_checkout_identical_to_main_has_no_standing_to_claim() {
        let root = scratch("identical");
        git(&root, &["checkout", "-qb", "wt/whatever"]);
        assert_eq!(standing(&root), Standing::MatchesMain);

        // Work of its own, in trees the sysroot is not built from, is still not
        // an ABI — which is what makes the rule bite on the common case rather
        // than on an idle checkout.
        fs::write(root.join("kernel/src/main.rs"), b"fn main() { loop {} }\n").unwrap();
        git(&root, &["commit", "-qam", "kernel work"]);
        assert_eq!(standing(&root), Standing::MatchesMain);
    }

    #[test]
    fn an_abi_change_is_standing_committed_or_not() {
        let root = scratch("diverged");
        git(&root, &["checkout", "-qb", "wt/abi"]);

        fs::write(root.join("toyos-abi/src/lib.rs"), b"pub struct A(pub u64);\n").unwrap();
        assert_eq!(standing(&root), Standing::Diverged, "an uncommitted ABI change is one");

        git(&root, &["commit", "-qam", "abi"]);
        assert_eq!(standing(&root), Standing::Diverged, "a committed ABI change is one");

        // And it stops being standing the moment it lands, which is what makes
        // the refusal end by itself rather than by anyone acting.
        git(&root, &["checkout", "-q", "main"]);
        git(&root, &["merge", "-q", "--ff-only", "wt/abi"]);
        git(&root, &["checkout", "-q", "wt/abi"]);
        assert_eq!(standing(&root), Standing::MatchesMain);
    }

    /// **A worktree that is merely *behind* main has nothing to claim with.**
    ///
    /// `git diff main` is symmetric, so a checkout that has simply not merged
    /// somebody else's landed ABI change read as `Diverged` and could claim —
    /// rebuilding the shared sysroot from sources *older* than main's and
    /// refusing the worktree whose change is already landed. That is the
    /// 2026-08-04 fight `specs/worktrees.md` §3.2 exists to prevent, arrived at
    /// from the other direction, and merging is this checkout's whole answer.
    #[test]
    fn a_checkout_behind_main_has_no_standing_to_claim() {
        let root = scratch("behind");
        git(&root, &["checkout", "-qb", "wt/idle"]);

        git(&root, &["checkout", "-q", "main"]);
        fs::write(root.join("toyos-abi/src/lib.rs"), b"pub struct A(pub u64);\n").unwrap();
        git(&root, &["commit", "-qam", "somebody else's ABI change, landed"]);

        git(&root, &["checkout", "-q", "wt/idle"]);
        assert_eq!(
            standing(&root),
            Standing::MatchesMain,
            "a worktree that has not merged main is not diverged from it"
        );
    }

    /// An unanswered question is not permission: a claim is destructive.
    #[test]
    fn git_that_cannot_answer_gives_no_standing() {
        let root = scratch("no-main");
        git(&root, &["checkout", "-qb", "only"]);
        git(&root, &["branch", "-qD", "main"]);
        assert_eq!(standing(&root), Standing::Unknown);
    }
}
