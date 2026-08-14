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
    /// Nobody in this repository: `rust/build` holds a toolchain that arrived
    /// as an artifact, and there is no `rust/` source to have built it from.
    ///
    /// Read off the disk rather than declared, because a checkout with a
    /// toolchain and no compiler source has exactly one thing it can do with
    /// it, and a flag or an env var saying so could disagree with what is
    /// there. This is how a CI runner gets a sysroot: `x.py` on four cores is
    /// an hour, and the product is 1.1 GB.
    Installed,
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
        let installed = !root.join("rust/x.py").exists()
            && root
                .join(format!("rust/build/{}/stage2/bin/rustc", host_triple()))
                .exists();
        return if installed { Owner::Installed } else { Owner::Us };
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
        Owner::Us | Owner::Installed => root.join("rust"),
        Owner::Elsewhere(primary) => primary.join("rust"),
    }
}

/// The per-worktree sources that end up *inside* the shared sysroot: std links
/// `toyos-abi` and `toyos`, and `libtoyos_c.a` is `userland/libc`.
pub const SYSROOT_SOURCES: [&str; 3] = ["toyos-abi/src", "toyos/src", "userland/libc/src"];

/// Of those, the ones a change to obliges an std rebuild.
const STD_SOURCES: [&str; 2] = ["toyos-abi/src", "toyos/src"];

/// The crates `library/std` names by a path relative to itself, which is why
/// [`SourceOverride`] exists at all.
const STD_PATH_DEPS: [&str; 2] = ["toyos-abi", "toyos"];

/// The manifest those two paths are written in.
const STD_MANIFEST: &str = "library/std/Cargo.toml";

/// Point `library/std`'s two ToyOS dependencies at the worktree doing the
/// building, for the length of one `x build`.
///
/// `rust/library/std/Cargo.toml` names them `../../../toyos-abi` and
/// `../../../toyos`, and `rust/` belongs to the primary checkout, so without
/// this every worktree's std is compiled against **main's** ABI while its kernel
/// is compiled against its own — the kernel and std then disagree about struct
/// layouts and both still build, link and boot.
///
/// **The manifest and not a cargo `paths` override.** The override works —
/// measured, the marker reached the sysroot — but cargo warns that overriding
/// `toyos` alters `toyos-abi`'s resolved source and that the warning becomes a
/// hard error. The manifest is where the paths are written, so it is where they
/// are corrected, and nothing restructures a dependency graph behind cargo's
/// back.
///
/// **Rewritten from whatever is there, not from one expected spelling.** A run
/// killed between the edit and the restore leaves an absolute path naming a
/// worktree that may since have been removed; matching either form makes the
/// next build repair it instead of refusing.
struct SourceOverride {
    manifest: PathBuf,
}

impl SourceOverride {
    /// Callers must already hold [`Scope::Global`]: this is one file, in one
    /// tree, that every worktree builds its sysroot out of.
    fn write(rust_dir: &Path, root: &Path) -> Self {
        let manifest = rust_dir.join(STD_MANIFEST);
        let held = Self { manifest };
        held.retarget(|dep| root.join(dep).display().to_string());
        held
    }

    fn retarget(&self, path_of: impl Fn(&str) -> String) {
        let mut text = fs::read_to_string(&self.manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", self.manifest.display()));
        for dep in STD_PATH_DEPS {
            text = retarget_path_dep(&text, dep, &path_of(dep));
        }
        fs::write(&self.manifest, text)
            .unwrap_or_else(|e| panic!("write {}: {e}", self.manifest.display()));
    }
}

impl Drop for SourceOverride {
    fn drop(&mut self) {
        self.retarget(|dep| format!("../../../{dep}"));
    }
}

/// Replace the path of one `<dep> = { path = "…" }` dependency.
///
/// Panics if the manifest does not name it exactly once: the fork moving that
/// line is a thing to be told about, not a substitution to silently perform
/// zero times.
fn retarget_path_dep(manifest: &str, dep: &str, path: &str) -> String {
    let opening = format!("\n{dep} = {{ path = \"");
    let occurrences = manifest.matches(&opening).count();
    assert_eq!(
        occurrences, 1,
        "{STD_MANIFEST} names `{dep} = {{ path = \"…\" }}` {occurrences} times, not once, so \
         the sysroot's ABI cannot be pointed at one checkout. The fork moved it."
    );
    let start = manifest.find(&opening).expect("counted above") + opening.len();
    let end = start + manifest[start..].find('"').expect("an unterminated TOML string");
    format!("{}{path}{}", &manifest[..start], &manifest[end..])
}

/// Every `toyos-abi`/`toyos` source file the built std actually compiled, read
/// out of cargo's dep-info rather than out of what we asked for.
fn std_toyos_sources(rust_dir: &Path) -> Vec<String> {
    let mut deps = Vec::new();
    collect_dep_info(
        &rust_dir.join(format!("build/{}/stage1-std/x86_64-unknown-toyos", host_triple())),
        &mut deps,
    );
    let mut found: Vec<String> = deps
        .iter()
        .flat_map(|text| toyos_sources_in_dep_info(text))
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

fn collect_dep_info(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_dep_info(&path, out);
        } else if path.extension().is_some_and(|e| e == "d") {
            if let Ok(text) = fs::read_to_string(&path) {
                out.push(text);
            }
        }
    }
}

/// The paths in one dep-info file that name a `toyos-abi/src` or `toyos/src`
/// source. Split out from the filesystem so the gate below has a negative
/// control that is a string literal.
fn toyos_sources_in_dep_info(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split_ascii_whitespace() {
        let word = word.strip_suffix(':').unwrap_or(word);
        if !word.ends_with(".rs") {
            continue;
        }
        if STD_SOURCES.iter().any(|src| word.contains(&format!("/{src}/"))) {
            out.push(word.to_string());
        }
    }
    out
}

/// Refuse a sysroot whose std was compiled against another checkout's ABI.
///
/// The witness records what the *builder* believed; this reads what the compiler
/// was handed. They disagreed for the whole life of the defect above, and no
/// test could see it — a worktree's kernel and its std both built, both linked,
/// and the syscall arguments landed at different offsets.
fn assert_std_built_from(root: &Path, rust_dir: &Path) {
    let sources = std_toyos_sources(rust_dir);
    assert!(
        !sources.is_empty(),
        "the std build under {} names no toyos-abi or toyos source at all, so the check that \
         it was built from this worktree cannot answer. Cargo's dep-info moved.",
        rust_dir.display(),
    );
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let foreign: Vec<&String> =
        sources.iter().filter(|p| !Path::new(p).starts_with(&root)).collect();
    assert!(
        foreign.is_empty(),
        "std was compiled against {} sources that are not this worktree's:\n  {}\n\
         The directory override in {} did not take effect, so this sysroot's ABI is another \
         checkout's.",
        foreign.len(),
        foreign.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("\n  "),
        rust_dir.join(".cargo/config.toml").display(),
    );
}

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

/// Whether the checkout a claim names is still there to land it.
///
/// **A claim names its holder, and it is a claim only while that holder is
/// real.** On 2026-08-14 the sysroot carried
/// `1786423818 /Users/jan/Dev/jan/toyos-logabi wt/toyos-logabi`, written at
/// 06:50 on 2026-08-11. The worktree had been removed, the branch survived
/// neither locally nor on origin, and the change it held had landed on main
/// three days earlier as `9d535d6`. Every main-matching checkout was told to
/// wait for it to land — which had already happened and could never happen
/// again — and `--rebuild-toolchain` in the primary was refused by the same
/// record, so the one checkout allowed to rebuild was the one told not to. Both
/// exits closed, on a promise nobody was left to keep.
///
/// **The directory, and not `git worktree list`.** Both answer for the path in
/// the record, and only one of them can be wrong in the direction that costs
/// something: reading a *live* holder as gone is how a worktree with an unlanded
/// ABI change gets the sysroot taken from it, which is the 2026-08-04 fight. The
/// listing risks exactly that three ways, two of them measured on this host on
/// 2026-08-14. It prints git's own realpath — `/private/tmp/…/linked` for a
/// worktree whose `pwd` is `/tmp/…/linked` — while a claim records
/// `CARGO_MANIFEST_DIR`, the path cargo was invoked at, so one live holder
/// spelled two ways reads as absent. It needs git to answer at all, and an
/// unanswerable git question is not permission — the rule [`Standing::Unknown`]
/// already exists for. And it lists a removed worktree anyway, as `prunable
/// gitdir file points to non-existent location`: the missing *directory* is the
/// ground truth that listing is derived from, so asking the listing is asking
/// this question through a layer that can only lose.
///
/// The remaining error is the harmless one: a directory `git worktree remove`
/// left behind reads as live, which costs today's refusal and nothing more.
fn holder_gone(who: &Path) -> bool {
    !who.exists()
}

fn holder(rust_dir: &Path) -> String {
    claimant(rust_dir).map_or_else(
        || "a checkout that left no record".to_string(),
        |(who, said)| {
            if holder_gone(&who) {
                format!("{said} — a checkout that is no longer on disk")
            } else {
                said
            }
        },
    )
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
    minutes of eight agents' time on 2026-08-07.";

fn standing(root: &Path) -> Standing {
    let mut ahead = vec!["diff", "--quiet", "main...HEAD", "--"];
    ahead.extend(SYSROOT_SOURCES);
    let committed =
        Command::new("git").args(&ahead).current_dir(root).status().ok().and_then(|s| s.code());

    if committed == Some(1) {
        return Standing::Diverged;
    }
    // **A merge in progress is not this branch's statement about itself.**
    // An agent resolving `--pr`'s merge of main holds every file main changed as
    // staged local work, and a build in that state would be told it had standing
    // to claim a sysroot it has no delta for. What the branch has of its own is
    // the committed question above.
    if merging(root) {
        return if committed == Some(0) { Standing::MatchesMain } else { Standing::Unknown };
    }

    let mut local = vec!["status", "--porcelain", "--"];
    local.extend(SYSROOT_SOURCES);
    let uncommitted = Command::new("git").args(&local).current_dir(root).output().ok();

    match (committed, uncommitted) {
        (Some(0), Some(out)) if out.status.success() => {
            if out.stdout.is_empty() { Standing::MatchesMain } else { Standing::Diverged }
        }
        _ => Standing::Unknown,
    }
}

fn merging(root: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .current_dir(root)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// What a checkout whose sources disagree with the shared sysroot may do about
/// it.
///
/// [`standing`] asks about this checkout and main. This asks about the sysroot
/// as well, because the same disagreement is two different things depending on
/// whether the checkout named on it is still there — and until 2026-08-14 only
/// the first question was asked, so a claim outlived its holder by three days
/// and refused everybody with a "wait for it to land" about a change that had
/// landed.
#[derive(PartialEq, Debug)]
enum Resolution {
    /// Wait. A checkout that is still there built the sysroot from a change
    /// that is not on main yet, and this one has nothing of its own to build
    /// with. The refusal ends when that change lands, and by nothing else.
    Wait,
    /// Rebuild it, and say why. The record names a checkout that is gone, so
    /// the rebuild takes nothing from anybody, and this one's sources are
    /// main's, so what it puts in the sysroot is what every other worktree
    /// already has. Staleness, exactly as a moved [`std_fork_stale`] is.
    Stale,
    /// Claim it, deliberately and with `--claim-sysroot`. This checkout has an
    /// ABI of its own, and putting it in the shared sysroot refuses every other
    /// worktree whoever held it before.
    Claim,
    /// Refuse: git could not say whether this checkout differs from main, and a
    /// claim is destructive.
    Unclear,
}

fn resolution(root: &Path, rust_dir: &Path) -> Resolution {
    match standing(root) {
        Standing::Diverged => Resolution::Claim,
        Standing::Unknown => Resolution::Unclear,
        // **No record at all is not a dead claim.** [`record_claimant`] is
        // advisory and may fail, so an absent record can be a live holder that
        // could not write one; only a holder that is named *and* gone is dead.
        Standing::MatchesMain if claimant(rust_dir).is_some_and(|(who, _)| holder_gone(&who)) => {
            Resolution::Stale
        }
        Standing::MatchesMain => Resolution::Wait,
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

/// The std fork the sysroot's `library` was compiled from.
///
/// **The fourth source, and it was witnessed by nothing until 2026-08-10.** A
/// worktree that edits `rust/library` edits the primary's, because `rust/` is
/// one submodule for the whole repository and a linked worktree's is an empty
/// stub. [`adopt_shared_sysroot`] then compared the three trees above, found
/// them equal and returned, so the worktree compiled against the std already on
/// disk — silently, with the only symptom being the compiler refusing a method
/// the source plainly has. It cost one agent three cycles and let a branch reach
/// CI with an std that does not compile (`error[E0433]`, run `31370078581`).
///
/// **Its own file rather than a line in [`witness`]**, and the reason is
/// migration rather than taxonomy. Every checkout compares that file for
/// equality, including ones built from a commit older than this one, so a new
/// line in it refuses every worktree on the host until each merges — a build
/// system breaking every agent's tree in order to add a check.
///
/// **A fingerprint and not 2,159 lines.** The three trees above are 34 files,
/// where naming the one that differs is worth a line each; this is 39 MiB and
/// `git -C rust status` is what names a file inside it.
fn std_fork_path(rust_dir: &Path) -> PathBuf {
    rust_dir.join("build/toyos-std-fork-witness")
}

/// What `rust/library` currently hashes to, or `None` where there is no `rust/`
/// source to read.
///
/// `None` is [`Owner::Installed`] and nothing else: a toolchain that arrived as
/// an artifact is named for `HEAD:rust` by the workflow that published it, so
/// its std fork is pinned by the tag — and a checkout that cannot read a source
/// may not report having checked it.
///
/// The cost was measured before it was chosen: 45 ms warm over 2,159 files on
/// the dev host, against 1.3 ms for the other three trees, deciding a rebuild
/// that is minutes long.
fn std_fork_witness(rust_dir: &Path) -> Option<String> {
    let mut files = Vec::new();
    collect_sources(&rust_dir.join("library"), &mut files);
    if files.is_empty() {
        return None;
    }
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for path in files {
        let data = fs::read(&path).unwrap_or_else(|e| panic!("witness {}: {e}", path.display()));
        path.strip_prefix(rust_dir).unwrap_or(&path).hash(&mut hasher);
        data.hash(&mut hasher);
    }
    Some(format!("{:016x}\n", hasher.finish()))
}

/// Whether the sysroot's std was compiled from a different `rust/library` than
/// the one on disk now.
///
/// **No record is staleness here, where [`std_sources_stale`] reads it as
/// ignorance.** That one may assume the primary built what is beside it,
/// because the primary is the only thing that ever wrote the sysroot. This
/// cannot: an edit to `rust/library` that nobody has built is exactly the state
/// this exists to catch, and it is indistinguishable from a sysroot that
/// predates the record. So the first build after this lands rebuilds std once,
/// everywhere, and every build after that has an answer.
fn std_fork_stale(rust_dir: &Path) -> bool {
    let Some(want) = std_fork_witness(rust_dir) else {
        return false;
    };
    fs::read_to_string(std_fork_path(rust_dir)).ok().as_deref() != Some(want.as_str())
}

/// Record the std fork the sysroot was just built from.
fn record_std_fork(rust_dir: &Path) {
    let Some(want) = std_fork_witness(rust_dir) else {
        return;
    };
    fs::write(std_fork_path(rust_dir), want)
        .unwrap_or_else(|e| panic!("write {}: {e}", std_fork_path(rust_dir).display()));
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
/// failed probe (`specs/issues/build/`).
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

    match owner(root) {
        Owner::Elsewhere(primary) => {
            adopt_shared_sysroot(root, &rust_dir, &primary, force_rebuild, claim_sysroot, lock);
            return;
        }
        Owner::Installed => {
            check_installed_toolchain(root, &rust_dir, force_rebuild, claim_sysroot);
            return;
        }
        Owner::Us => {}
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
            assert!(
                holder_gone(&who),
                "the shared sysroot is held by {}, for a change that is not on main yet, \
                 and rebuilding it here would take it from the one checkout that cannot \
                 merge its way out.\n\
                 It was claimed from {since}.\n\
                 Wait for it to land — this refusal ends by itself, because main will then \
                 carry what the sysroot holds. `--claim-sysroot` takes it back deliberately.",
                who.display(),
            );
            // A lease nobody holds. The checkout that wrote it is gone, so there
            // is nothing left to take it from and nothing that could ever end
            // the refusal it used to be — see [`holder_gone`].
            eprintln!(
                "The shared sysroot was built from {}, which is not on disk any more; the \
                 record it left ({since}) names nobody.\n\
                 That makes the disagreement staleness rather than a claim, so this checkout \
                 rebuilds the sysroot from its own sources and the record becomes its own.",
                who.display(),
            );
        }
    }

    let compiler_stamp = stamps_dir.join("compiler.stamp");
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
            } else if std_sources_stale(root, &rust_dir) || std_fork_stale(&rust_dir) {
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
            build_hosted_rustc(root, &rust_dir, &toyos_ld_binary(root));
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
            let stale = fs::read_to_string(witness_path(&rust_dir)).ok().as_deref()
                != Some(want.as_str());
            (stale || std_fork_stale(&rust_dir)).then_some(want)
        },
        |want| {
            fs::write(witness_path(&rust_dir), &want)
                .unwrap_or_else(|e| panic!("write {}: {e}", witness_path(&rust_dir).display()));
            record_std_fork(&rust_dir);
            record_claimant(root, &rust_dir);
        },
    );
}

/// Everything a checkout may do with a toolchain it did not build: check that
/// it is the one this tree needs, and say what to do when it is not.
///
/// The same three questions [`adopt_shared_sysroot`] asks, minus the claim: no
/// amount of source here can rebuild a sysroot without `rust/`, so there is
/// nothing to arbitrate and the answer is always to publish a toolchain built
/// from these sources.
///
/// It is also the one checkout that cannot ask about the std fork at all — see
/// [`std_fork_witness`], which answers `None` here because there is no `rust/`
/// to read, and the artifact's own tag is what pins it instead.
fn check_installed_toolchain(root: &Path, rust_dir: &Path, force_rebuild: bool, claim: bool) {
    let stage2 = rust_dir.join(format!("build/{}/stage2", host_triple()));
    assert!(
        !force_rebuild && !claim,
        "there is no `rust/` source in {}, so neither --rebuild-toolchain nor \
         --claim-sysroot has anything to build from.\n\
         The toolchain at {} arrived as an artifact; rebuild it where it is published.",
        root.display(),
        stage2.display(),
    );

    let linked = rustup_link();
    assert!(
        linked.as_deref() == Some(stage2.as_path()),
        "the rustup toolchain `toyos` points at {}, not at the installed toolchain at {}.\n\
         Link it: rustup toolchain link toyos {}",
        linked.map_or_else(|| "nothing".to_string(), |p| p.display().to_string()),
        stage2.display(),
        stage2.display(),
    );

    // Recreated rather than shipped: it points into whatever stable toolchain
    // this machine has, which is not a path any artifact can know.
    if host_target_missing(rust_dir) {
        link_host_target(rust_dir);
    }

    let want = witness(root);
    let recorded = fs::read_to_string(witness_path(rust_dir)).ok();
    assert!(
        recorded.as_deref() == Some(want.as_str()),
        "this checkout and the installed toolchain at {} disagree about {}, so a build \
         here would link its kernel against another tree's struct layouts.\n\
         Publish a toolchain built from these sources and install that one instead.",
        stage2.display(),
        differing_trees(recorded.as_deref(), &want),
    );
}

/// Whether the sysroot's std was built from other `toyos-abi`/`toyos` sources
/// than the ones in this checkout.
///
/// Replaces the mtime stamps these two trees used to have. Those could not
/// answer the question across checkouts — two worktrees of one commit hold
/// identical bytes at different paths with different mtimes — and answered it
/// wrongly within one, since `git checkout` rewriting an unchanged file bought
/// a full std rebuild. `rust/library` had such a stamp until 2026-08-10 and
/// [`std_fork_stale`] is what replaced it, for the second reason as much as the
/// first — and because a stamp under one worktree's `target/` answers for that
/// worktree and for nobody else.
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
        // **A std fork that has moved is staleness, and staleness is not a
        // claim.** `rust/` is the primary's and every checkout compiles against
        // that one directory, so rebuilding std out of it produces the sysroot
        // each of them wants and takes nothing from any of them — where a
        // `toyos-abi` claim refuses every other worktree until it lands. So
        // this rebuilds rather than refusing, and leaves the claimant alone.
        if std_fork_stale(rust_dir) {
            lock.act_if(
                Scope::Global,
                "rebuild std for a std fork that has moved",
                || std_fork_stale(rust_dir).then_some(()),
                |()| {
                    eprintln!(
                        "The std fork under {} has changed since the sysroot was built. \
                         Rebuilding std.",
                        rust_dir.display()
                    );
                    rebuild_std(root, rust_dir);
                    crate::libc::build(root, rust_dir);
                    record_std_fork(rust_dir);
                },
            );
        }
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
    // this branch and `left: Wait / right: Wait` under it is noise an agent has
    // to read past.
    match resolution(root, rust_dir) {
        Resolution::Wait => panic!(
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
        Resolution::Unclear => panic!(
            "this worktree and the shared sysroot at {} disagree about {differs}, and git \
             cannot say whether this checkout differs from main — so whether it has any \
             standing to claim is unknown, and a claim is destructive.\n\
             Check `git diff main -- {}`.",
            rust_dir.display(),
            SYSROOT_SOURCES.join(" "),
        ),
        // The refusal above would say "wait for it to land", and there is nobody
        // left to land anything: the sysroot outlived the checkout that built
        // it. So the disagreement is staleness, and this rebuilds rather than
        // refusing — with main's sources, which is what every other worktree
        // already has, taking nothing from anybody.
        Resolution::Stale => {
            eprintln!(
                "This worktree and the shared sysroot at {} disagree about {differs}, and the \
                 sysroot was built by {}.\n\
                 A claim names its holder, and nothing here is waiting for that one any more: \
                 your toyos-abi and toyos are main's, so rebuilding the sysroot from them is \
                 staleness rather than a claim. The record becomes this worktree's, unclaimed.",
                rust_dir.display(),
                holder(rust_dir),
            );
            rebuild_shared_sysroot(root, rust_dir, &want, lock);
            return;
        }
        Resolution::Claim => {}
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

    rebuild_shared_sysroot(root, rust_dir, &want, lock);
}

/// Put this worktree's sources in the shared sysroot, and record that they are
/// what is in there.
///
/// The two ways a linked worktree reaches this are opposites in what they cost
/// everybody else — a claim refuses every other worktree until it lands, a
/// staleness rebuild refuses nobody — and identical in what they do, down to
/// deciding again under the exclusive lock: whoever held it in between may have
/// built exactly this.
fn rebuild_shared_sysroot(root: &Path, rust_dir: &Path, want: &str, lock: &mut buildlock::Held) {
    lock.act_if(
        Scope::Global,
        "rebuild the shared sysroot from a linked worktree",
        || {
            (fs::read_to_string(witness_path(rust_dir)).ok().as_deref() != Some(want)
                || std_fork_stale(rust_dir))
            .then_some(())
        },
        |()| {
            eprintln!("Rebuilding the shared sysroot from {}...", root.display());
            rebuild_std(root, rust_dir);
            crate::libc::build(root, rust_dir);
            record_std_fork(rust_dir);
            fs::write(witness_path(rust_dir), want)
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

/// Run bootstrap, streaming its output where it was going anyway and keeping a
/// copy.
///
/// `.status()` was enough while the only question was the exit code. It is not
/// enough for the question [`refuse_on_compile_error`] asks, which is what the
/// failure *was*.
fn x_build(rust_dir: &Path, args: &[&str], what: &str) -> (bool, Vec<String>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    let x = if rust_dir.join("x").exists() { "./x" } else { "./x.py" };
    let mut child = Command::new(x)
        .args(args)
        .env("BOOTSTRAP_SKIP_TARGET_SANITY", "1")
        .current_dir(rust_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("Failed to run {x} for {what}: {e}"));

    // One log in the order the two streams produced it, because the error and
    // the `--> file:line` under it are what a reader needs together.
    let log = Arc::new(Mutex::new(Vec::new()));
    let pump = |stream: Box<dyn Read + Send>, to_stderr: bool, log: Arc<Mutex<Vec<String>>>| {
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if to_stderr {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                    let _ = std::io::stdout().flush();
                }
                log.lock().expect("the log outlives both pumps").push(line);
            }
        })
    };
    let out = pump(Box::new(child.stdout.take().expect("piped")), false, Arc::clone(&log));
    let err = pump(Box::new(child.stderr.take().expect("piped")), true, Arc::clone(&log));
    let status = child.wait().unwrap_or_else(|e| panic!("waiting for {x} {what}: {e}"));
    out.join().expect("the stdout pump");
    err.join().expect("the stderr pump");

    let log = Arc::try_unwrap(log).expect("both pumps are joined").into_inner();
    (status.success(), log.expect("no pump panicked while holding it"))
}

/// Where a compile error starts in an `x build` log, if there is one.
///
/// Both bootstrap callers let a non-zero `x build` through when the artifacts
/// they need are on disk, because rustdoc for ToyOS does not link and never
/// has. That allowance used to be *anything at all*, as long as a `rustc` from
/// some earlier build was still there — so run `31370078581` compiled std with
/// `error[E0433]`, took the allowance, and died 83 seconds and 260 lines later
/// at a missing file. The reported failure was the consequence.
///
/// A compile error cannot be a link failure, so it cannot be the thing that
/// allowance is for.
fn compile_error_at(log: &[String]) -> Option<usize> {
    log.iter().position(|l| {
        let l = l.trim_start();
        l.starts_with("error[") || l.starts_with("error: could not compile")
    })
}

/// Stop on the first real error rather than on what it goes on to break.
fn refuse_on_compile_error(log: &[String], what: &str) {
    let Some(at) = compile_error_at(log) else { return };
    let end = (at + 6).min(log.len());
    panic!("{what} did not compile:\n{}", log[at..end].join("\n"));
}

/// What an `x build` failure the artifact check is willing to tolerate actually
/// said, so that "expected" is a claim the reader can check.
fn tolerated_failure(log: &[String], what: &str) {
    let errors: Vec<&str> =
        log.iter().map(String::as_str).filter(|l| l.trim_start().starts_with("error")).collect();
    eprintln!(
        "Note: {what} exited non-zero and the artifacts it must produce are all there, so this \
         is the ToyOS rustdoc link failure. It reported:\n{}",
        if errors.is_empty() {
            "  (no line beginning `error`)".to_string()
        } else {
            errors.join("\n")
        }
    );
}

fn full_bootstrap(root: &Path, rust_dir: &Path) {
    let _sources = SourceOverride::write(rust_dir, root);
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

    let build = ["build", "--stage", "2", "--warnings", "warn"];
    let (ok, log) = x_build(rust_dir, &build, "the toolchain");

    if !ok {
        refuse_on_compile_error(&log, "the toolchain");
        // rustdoc for ToyOS may fail to link; rustc may not be missing.
        let stage2 = rust_dir.join(format!("build/{host}/stage2"));
        assert!(
            stage2.join("bin/rustc").exists(),
            "the toolchain build failed and {} is not there.\n\
             Nothing in its output was a compile error, so this is a link or a bootstrap \
             failure — the last lines above are the whole of what it said.",
            stage2.join("bin/rustc").display()
        );
        tolerated_failure(&log, "the toolchain build");
    }
    assert_std_built_from(root, rust_dir);
}

fn rebuild_std(root: &Path, rust_dir: &Path) {
    let _sources = SourceOverride::write(rust_dir, root);
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

    let (ok, log) =
        x_build(rust_dir, &["build", "--stage", "2", "library", "--warnings", "warn"], "std");
    refuse_on_compile_error(&log, "std");
    assert!(ok, "the std rebuild failed, and nothing in its output was a compile error");
    assert_std_built_from(root, rust_dir);
}

fn build_hosted_rustc(root: &Path, rust_dir: &Path, toyos_ld: &Path) {
    let _sources = SourceOverride::write(rust_dir, root);
    eprintln!("Building ToyOS-hosted rustc...");
    write_config(rust_dir, &host_triple(), toyos_ld, true);

    let (ok, log) =
        x_build(rust_dir, &["build", "--stage", "2", "--warnings", "warn"], "the hosted rustc");
    refuse_on_compile_error(&log, "the hosted rustc");

    // rustdoc for ToyOS may fail to link; rustc and librustc_driver may not.
    let toyos_stage2 = rust_dir.join("build/x86_64-unknown-toyos/stage2");
    assert!(
        toyos_stage2.join("bin/rustc").exists(),
        "the hosted rustc build failed and {} is not there.\n\
         Nothing in its output was a compile error, so this is a link or a bootstrap failure.",
        toyos_stage2.join("bin/rustc").display()
    );
    assert!(
        fs::read_dir(toyos_stage2.join("lib"))
            .map(|d| d.filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("librustc_driver")))
            .unwrap_or(false),
        "the hosted rustc build failed: librustc_driver*.so is not in {}",
        toyos_stage2.join("lib").display()
    );
    if !ok {
        tolerated_failure(&log, "the hosted rustc build");
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
    /// 2026-08-04 fight the standing rule exists to prevent, arrived at
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

    /// **A landing's own merge must not look like standing.** A branch part-way
    /// through merging main holds every file main changed as local work as far
    /// as `git status` is concerned — and a build in that state would be told it
    /// could claim a sysroot it has no delta for.
    #[test]
    fn a_landing_s_uncommitted_merge_is_not_standing() {
        let root = scratch("mid-landing");
        git(&root, &["checkout", "-qb", "wt/idle"]);
        fs::write(root.join("kernel/src/main.rs"), b"fn main() { loop {} }\n").unwrap();
        git(&root, &["commit", "-qam", "work of its own, outside the witnessed trees"]);

        git(&root, &["checkout", "-q", "main"]);
        fs::write(root.join("toyos-abi/src/lib.rs"), b"pub struct A(pub u64);\n").unwrap();
        git(&root, &["commit", "-qam", "somebody else's ABI change, landed"]);

        git(&root, &["checkout", "-q", "wt/idle"]);
        git(&root, &["merge", "--no-ff", "--no-commit", "main"]);
        assert_eq!(
            standing(&root),
            Standing::MatchesMain,
            "a landing gating its own merge of main was told it could claim"
        );
    }

    /// A `rust/` whose sysroot carries a claim by `who`, written the way
    /// [`record_claimant`] writes one and as old as the ghost below was.
    fn claimed_by(name: &str, who: &Path) -> PathBuf {
        let rust_dir = std::env::temp_dir().join(format!("toyos-claim-{name}"));
        let _ = fs::remove_dir_all(&rust_dir);
        fs::create_dir_all(rust_dir.join("build")).unwrap();
        let when = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock later than 1970")
            .as_secs()
            .saturating_sub(4775 * 60);
        fs::write(claimant_path(&rust_dir), format!("{when} {} wt/ghost\n", who.display()))
            .unwrap();
        rust_dir
    }

    /// A `rust/` whose sysroot was built by somebody who left no record.
    fn unrecorded(name: &str) -> PathBuf {
        let rust_dir = std::env::temp_dir().join(format!("toyos-claim-{name}"));
        let _ = fs::remove_dir_all(&rust_dir);
        fs::create_dir_all(rust_dir.join("build")).unwrap();
        rust_dir
    }

    /// **The ghost claim of 2026-08-14, as a test.**
    ///
    /// The sysroot held `1786423818 /Users/jan/Dev/jan/toyos-logabi
    /// wt/toyos-logabi`. That worktree had been removed, its branch was on
    /// neither this machine nor origin, and what it held had landed on main
    /// three days earlier as `9d535d6`. Before this, [`standing`] was the whole
    /// answer and `MatchesMain` was always the refusal, so every main-matching
    /// checkout got "**Wait for it to land and merge main.** This refusal then
    /// ends by itself" about a change that had already landed — observed
    /// verbatim from `toolchain.rs:895` in a fresh worktree that day, with
    /// `--rebuild-toolchain` in the primary refused by the same record. Nothing
    /// could end it: a livelock.
    #[test]
    fn a_claim_whose_holder_is_gone_is_staleness_and_not_a_claim() {
        let root = scratch("ghost-claim");
        git(&root, &["checkout", "-qb", "wt/whatever"]);
        assert_eq!(standing(&root), Standing::MatchesMain);

        let ghost = std::env::temp_dir().join("toyos-a-worktree-that-was-removed");
        let _ = fs::remove_dir_all(&ghost);
        assert!(holder_gone(&ghost));
        assert_eq!(
            resolution(&root, &claimed_by("ghost", &ghost)),
            Resolution::Stale,
            "a checkout that matches main was left waiting for a checkout that is gone"
        );

        // **No record at all is not a dead claim.** `record_claimant` is
        // advisory and may fail, so an absent record can be a live holder that
        // could not write one — which is why only a holder that is *named* and
        // gone is dead.
        assert_eq!(resolution(&root, &unrecorded("none")), Resolution::Wait);
    }

    /// The edge that must not move: a holder that is still on disk is still
    /// holding it, and the checkout with nothing of its own still waits.
    ///
    /// Weakening this is the 2026-08-04 fight — a sysroot taken from the one
    /// worktree that could not merge its way out, six landing attempts, four
    /// witness rewrites in 38 minutes, one gate dead with 156 refusals.
    #[test]
    fn a_claim_whose_holder_is_there_is_still_a_claim() {
        let root = scratch("live-claim");
        git(&root, &["checkout", "-qb", "wt/whatever"]);
        let live = scratch("live-holder");
        assert!(!holder_gone(&live));
        assert_eq!(
            resolution(&root, &claimed_by("live", &live)),
            Resolution::Wait,
            "a live holder's claim was read as staleness"
        );
    }

    /// A dead claim makes a disagreement staleness. It does not make a real ABI
    /// change free: putting one in the shared sysroot refuses every other
    /// worktree whoever held it before, so it stays a deliberate
    /// `--claim-sysroot` — and git that cannot answer stays a refusal, because
    /// a dead holder is not the question that one is unsure about.
    #[test]
    fn a_dead_claim_does_not_hand_the_sysroot_to_a_checkout_that_differs() {
        let root = scratch("ghost-vs-abi");
        git(&root, &["checkout", "-qb", "wt/abi"]);
        fs::write(root.join("toyos-abi/src/lib.rs"), b"pub struct A(pub u64);\n").unwrap();

        let ghost = std::env::temp_dir().join("toyos-a-worktree-that-was-removed");
        let _ = fs::remove_dir_all(&ghost);
        assert_eq!(resolution(&root, &claimed_by("ghost-abi", &ghost)), Resolution::Claim);

        git(&root, &["branch", "-qD", "main"]);
        assert_eq!(resolution(&root, &claimed_by("ghost-unknown", &ghost)), Resolution::Unclear);
    }

    /// An unanswered question is not permission: a claim is destructive.
    #[test]
    fn git_that_cannot_answer_gives_no_standing() {
        let root = scratch("no-main");
        git(&root, &["checkout", "-qb", "only"]);
        git(&root, &["branch", "-qD", "main"]);
        assert_eq!(standing(&root), Standing::Unknown);
    }

    /// Verbatim from `rust/library/std/Cargo.toml`, so a fork edit that moves
    /// these lines fails here rather than in a sysroot nobody looks inside.
    const STD_DEPS: &str = "\n[target.'cfg(target_os = \"toyos\")'.dependencies]\n\
        toyos-abi = { path = \"../../../toyos-abi\", features = [\"rustc-dep-of-std\"], public = true }\n\
        toyos = { path = \"../../../toyos\", features = [\"rustc-dep-of-std\"], public = true }\n";

    #[test]
    fn the_two_std_deps_are_retargeted_and_nothing_else_is() {
        let mut out = STD_DEPS.to_string();
        for dep in STD_PATH_DEPS {
            out = retarget_path_dep(&out, dep, &format!("/checkouts/toyos-endow/{dep}"));
        }
        assert_eq!(
            out,
            "\n[target.'cfg(target_os = \"toyos\")'.dependencies]\n\
             toyos-abi = { path = \"/checkouts/toyos-endow/toyos-abi\", features = [\"rustc-dep-of-std\"], public = true }\n\
             toyos = { path = \"/checkouts/toyos-endow/toyos\", features = [\"rustc-dep-of-std\"], public = true }\n",
        );
    }

    #[test]
    fn the_build_leaves_the_fork_byte_identical_even_after_a_killed_one() {
        let rust_dir = std::env::temp_dir().join("toyos-source-override");
        let _ = fs::remove_dir_all(&rust_dir);
        fs::create_dir_all(rust_dir.join("library/std")).unwrap();
        let manifest = rust_dir.join(STD_MANIFEST);

        // What a build killed mid-flight left behind, naming a worktree that
        // has since been removed.
        let killed = STD_DEPS.replace("../../../", "/a-worktree-that-is-gone/");
        fs::write(&manifest, &killed).unwrap();

        {
            let _guard = SourceOverride::write(&rust_dir, Path::new("/checkouts/toyos-endow"));
            let held = fs::read_to_string(&manifest).unwrap();
            assert!(held.contains("\"/checkouts/toyos-endow/toyos-abi\""), "{held}");
            assert!(!held.contains("a-worktree-that-is-gone"), "{held}");
        }
        assert_eq!(fs::read_to_string(&manifest).unwrap(), STD_DEPS);
    }

    #[test]
    #[should_panic(expected = "The fork moved it")]
    fn a_dep_the_fork_no_longer_names_that_way_is_a_refusal() {
        retarget_path_dep("\ntoyos-abi = { workspace = true }\n", "toyos-abi", "/x");
    }

    /// The negative control is the defect itself: this is verbatim what cargo
    /// wrote for a worktree build before the override existed.
    #[test]
    fn dep_info_names_the_checkout_std_was_really_built_from() {
        let primary = "/Users/jan/Dev/jan/toyos/toyos-abi/src/lib.rs \
                       /Users/jan/Dev/jan/toyos/toyos/src/audio.rs \
                       /Users/jan/Dev/jan/toyos/rust/build/host/stage1-std/out/libcore.rmeta";
        assert_eq!(
            toyos_sources_in_dep_info(primary),
            [
                "/Users/jan/Dev/jan/toyos/toyos-abi/src/lib.rs",
                "/Users/jan/Dev/jan/toyos/toyos/src/audio.rs"
            ],
        );

        // A dep-info line ends in a colon when the file is its own target.
        let target = "/Users/jan/Dev/jan/toyos-endow/toyos-abi/src/lib.rs:";
        assert_eq!(
            toyos_sources_in_dep_info(target),
            ["/Users/jan/Dev/jan/toyos-endow/toyos-abi/src/lib.rs"],
        );

        // `rust/library/std/src/sys/pal/toyos/` is not one of these trees.
        assert!(
            toyos_sources_in_dep_info("/x/rust/library/std/src/sys/pal/toyos/mod.rs").is_empty()
        );
    }

    /// Verbatim from run `31370078581`, the run this check exists because of:
    /// the four warnings are real and were what the error had to be told apart
    /// from, and the allowance took it and reported a missing file 83 seconds
    /// later.
    fn run_31370078581() -> Vec<String> {
        [
            "   Compiling panic_unwind v0.0.0 (/home/runner/work/toyos/toyos/rust/library/panic_unwind)",
            "error[E0433]: cannot find `rtabort` in `crate`",
            "  --> library/std/src/sys/pal/toyos/tls.rs:35:26",
            "   |",
            "35 |         Err(_) => crate::rtabort!(\"no TLS block for a dlopen'd module\"),",
            "   |                          ^^^^^^^ could not find `rtabort` in the crate root",
            "warning: unused import: `IntoInner`",
            "  --> library/std/src/os/fd/owned.rs:24:38",
            "warning: unnecessary `unsafe` block",
            "warning: unused variable: `e`",
            "For more information about this error, try `rustc --explain E0433`.",
            "warning: `std` (lib) generated 4 warnings",
            "error: could not compile `std` (lib) due to 1 previous error; 4 warnings emitted",
            "Build completed unsuccessfully in 0:01:23",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn the_first_real_error_is_the_one_reported() {
        let log = run_31370078581();
        let at = compile_error_at(&log).expect("a compile error");
        assert_eq!(log[at], "error[E0433]: cannot find `rtabort` in `crate`");
        assert!(
            log[at + 1].contains("library/std/src/sys/pal/toyos/tls.rs:35"),
            "the file and line have to come with it, or the report is a name and a shrug"
        );
    }

    /// The negative control is the failure the allowance is *for*: a link
    /// failure has no `error[` and no `could not compile`, so it must not be
    /// mistaken for a compile error and stop a build that has its artifacts.
    #[test]
    fn a_link_failure_is_not_a_compile_error() {
        let log = [
            "error: linking with `toyos-ld` failed: exit status: 1",
            "  |",
            "  = note: rust-lld: error: undefined symbol: __rust_probestack",
            "Build completed unsuccessfully in 0:00:41",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(compile_error_at(&log), None);
    }

    /// **The three cycles this cost, as a test.** A `rust/library` edit was
    /// invisible to every checkout: the record did not exist, so a build
    /// compiled against the std already on disk.
    ///
    /// And the fourth state is the one only a runner is in — no `rust/` source
    /// at all, where the question cannot be asked and must not be answered
    /// `stale`, or every CI shard refuses the toolchain it just downloaded.
    #[test]
    fn a_std_fork_edit_is_seen_and_a_missing_fork_is_not_a_verdict() {
        let rust_dir = std::env::temp_dir().join("toyos-std-fork-witness");
        let _ = fs::remove_dir_all(&rust_dir);
        fs::create_dir_all(rust_dir.join("build")).unwrap();

        // No `rust/library`: Owner::Installed, and nothing to say.
        assert_eq!(std_fork_witness(&rust_dir), None);
        assert!(!std_fork_stale(&rust_dir), "a shard must not refuse what it cannot read");
        record_std_fork(&rust_dir);
        assert!(!std_fork_path(&rust_dir).exists(), "and it records nothing either");

        fs::create_dir_all(rust_dir.join("library/std/src/sys/pal/toyos")).unwrap();
        let tls = rust_dir.join("library/std/src/sys/pal/toyos/tls.rs");
        fs::write(&tls, b"pub fn block() {}\n").unwrap();

        assert!(std_fork_stale(&rust_dir), "no record is staleness, not ignorance");
        record_std_fork(&rust_dir);
        assert!(!std_fork_stale(&rust_dir));

        fs::write(&tls, b"pub fn block() { crate::rtabort!(\"no TLS block\") }\n").unwrap();
        assert!(std_fork_stale(&rust_dir), "the edit that reached CI uncompiled");
        record_std_fork(&rust_dir);
        assert!(!std_fork_stale(&rust_dir));
    }

    /// **A new line in the witness would refuse every worktree on the host**
    /// until each merged, because every checkout compares that file for
    /// equality and the ones built from an older commit compare the whole of
    /// it. That is why the std fork has a file of its own.
    #[test]
    fn the_witness_a_worktree_compares_is_unchanged_by_the_std_fork() {
        let root = scratch("witness-shape");
        assert!(
            witness(&root).lines().all(|l| SYSROOT_SOURCES.iter().any(|t| l.starts_with(t))),
            "witness: {}",
            witness(&root)
        );
    }
}
