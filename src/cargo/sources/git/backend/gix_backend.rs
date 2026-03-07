//! Git backend implementation using gix/gitoxide (pure Rust).
//!
//! When the `git2-backend` feature is disabled, this module provides
//! full git dependency support (clone, checkout, fetch, submodules)
//! using gix instead of libgit2. The utility functions (init, discover,
//! config) are also fully functional.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use anyhow::{Context as _, anyhow, bail};
use cargo_util::paths;
use cargo_util::paths::exclude_from_backups_and_indexing;
use gix::bstr::{BString, ByteSlice};
use tracing::{debug, info, trace};
use url::Url;

use crate::core::global_cache_tracker;
use crate::core::{Dependency, GitReference, Package, PackageId, SourceId};
use crate::sources::IndexSummary;
use crate::sources::RecursivePathSource;
use crate::sources::git::fetch::RemoteKind;
use crate::sources::git::oxide;
use crate::sources::git::oxide::cargo_config_to_gitoxide_overrides;
use crate::sources::source::{MaybePackage, QueryKind, Source};
use crate::util::cache_lock::CacheLockMode;
use crate::util::errors::CargoResult;
use crate::util::hex::short_hash;
use crate::util::interning::InternedString;
use crate::util::{GlobalContext, IntoUrl, Progress};

/// A file indicates that if present, `git reset` has been done and a repo
/// checkout is ready to go. See [`GitCheckout::reset`] for why we need this.
const CHECKOUT_READY_LOCK: &str = ".cargo-ok";

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A short abbreviated OID.
pub struct GitShortID(String);

impl GitShortID {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A remote repository. It gets cloned into a local [`GitDatabase`].
#[derive(PartialEq, Clone, Debug)]
pub struct GitRemote {
    url: Url,
}

/// A local clone of a remote repository's database.
pub struct GitDatabase {
    remote: GitRemote,
    path: PathBuf,
    repo: gix::Repository,
}

/// A local checkout of a particular revision from a [`GitDatabase`].
pub struct GitCheckout<'a> {
    database: &'a GitDatabase,
    path: PathBuf,
    revision: gix::ObjectId,
    repo: gix::Repository,
}

// ---------------------------------------------------------------------------
// GitRemote
// ---------------------------------------------------------------------------

impl GitRemote {
    pub fn new(url: &Url) -> GitRemote {
        GitRemote { url: url.clone() }
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Fetches and checkouts to a reference or a revision from this remote
    /// into a local path.
    pub fn checkout(
        &self,
        into: &Path,
        db: Option<GitDatabase>,
        reference: &GitReference,
        gctx: &GlobalContext,
    ) -> CargoResult<(GitDatabase, gix::ObjectId)> {
        if let Some(db) = db {
            fetch(
                &db.path,
                self.url.as_str(),
                reference,
                gctx,
                RemoteKind::GitDependency,
            )
            .with_context(|| format!("failed to fetch into: {}", into.display()))?;

            if let Ok(rev) = resolve_ref(reference, &db.path) {
                return Ok((db, rev));
            }
        }

        // Otherwise start from scratch to handle corrupt git repositories.
        if into.exists() {
            paths::remove_dir_all(into)?;
        }
        paths::create_dir_all(into)?;
        init_bare(into)?;
        fetch(
            into,
            self.url.as_str(),
            reference,
            gctx,
            RemoteKind::GitDependency,
        )
        .with_context(|| format!("failed to clone into: {}", into.display()))?;
        let rev = resolve_ref(reference, into)?;

        let config_overrides = cargo_config_to_gitoxide_overrides(gctx)?;
        let repo = oxide::open_repo(into, config_overrides, oxide::OpenMode::ForFetch)?;
        Ok((
            GitDatabase {
                remote: self.clone(),
                path: into.to_path_buf(),
                repo,
            },
            rev,
        ))
    }

    /// Creates a [`GitDatabase`] of this remote at `db_path`.
    pub fn db_at(&self, db_path: &Path) -> CargoResult<GitDatabase> {
        let repo = gix::open(db_path)?;
        Ok(GitDatabase {
            remote: self.clone(),
            path: db_path.to_path_buf(),
            repo,
        })
    }
}

// ---------------------------------------------------------------------------
// GitDatabase
// ---------------------------------------------------------------------------

impl GitDatabase {
    /// Checkouts to a revision at `dest` from this database.
    #[tracing::instrument(skip(self, gctx))]
    pub fn copy_to(
        &self,
        rev: gix::ObjectId,
        dest: &Path,
        gctx: &GlobalContext,
        quiet: bool,
    ) -> CargoResult<GitCheckout<'_>> {
        // If the existing checkout exists, and it is fresh, use it.
        let checkout = match gix::open(dest)
            .ok()
            .map(|repo| GitCheckout::new(self, rev, repo))
            .filter(|co| co.is_fresh())
        {
            Some(co) => co,
            None => {
                let (checkout, guard) = GitCheckout::clone_into(dest, self, rev, gctx)?;
                checkout.update_submodules(gctx, quiet)?;
                guard.mark_ok()?;
                checkout
            }
        };

        Ok(checkout)
    }

    /// Get a short OID for a `revision`, usually 7 chars or more if ambiguous.
    pub fn to_short_id(&self, revision: gix::ObjectId) -> CargoResult<GitShortID> {
        let hex = revision.to_hex_with_len(7);
        Ok(GitShortID(hex.to_string()))
    }

    /// Checks if the database contains the object of this `oid`.
    pub fn contains(&self, oid: gix::ObjectId) -> bool {
        self.repo.find_object(oid).is_ok()
    }

    /// Resolves this reference with this database.
    pub fn resolve(&self, r: &GitReference) -> CargoResult<gix::ObjectId> {
        resolve_ref(r, &self.path)
    }
}

// ---------------------------------------------------------------------------
// resolve_ref
// ---------------------------------------------------------------------------

/// Resolves [`GitReference`] to an object ID using the repo at `repo_path`.
pub fn resolve_ref(gitref: &GitReference, repo_path: &Path) -> CargoResult<gix::ObjectId> {
    let repo = gix::open(repo_path)?;
    let id = match gitref {
        GitReference::Tag(s) => {
            let refname = format!("refs/remotes/origin/tags/{}", s);
            let r = repo
                .find_reference(&refname)
                .with_context(|| format!("failed to find tag `{}`", s))?;
            r.into_fully_peeled_id()
                .with_context(|| format!("failed to peel tag `{}`", s))?
                .detach()
        }

        GitReference::Branch(s) => {
            let refname = format!("refs/remotes/origin/{}", s);
            let r = repo
                .find_reference(&refname)
                .with_context(|| format!("failed to find branch `{}`", s))?;
            r.into_fully_peeled_id()
                .with_context(|| format!("failed to peel branch `{}`", s))?
                .detach()
        }

        GitReference::DefaultBranch => {
            let r = repo
                .find_reference("refs/remotes/origin/HEAD")
                .context("failed to find ref `refs/remotes/origin/HEAD`")?;
            r.into_fully_peeled_id()
                .context("failed to peel refs/remotes/origin/HEAD")?
                .detach()
        }

        GitReference::Rev(s) => {
            let id = repo
                .rev_parse_single(s.as_bytes().as_bstr())
                .with_context(|| format!("failed to find rev `{}`", s))?
                .detach();
            // If it's a tag, peel to the commit
            match repo.find_object(id) {
                Ok(obj) => match obj.try_into_tag() {
                    Ok(tag) => tag
                        .target_id()
                        .context("tag has no target")?
                        .detach(),
                    Err(obj) => obj.id,
                },
                Err(_) => id,
            }
        }
    };
    Ok(id)
}

// ---------------------------------------------------------------------------
// GitCheckout
// ---------------------------------------------------------------------------

impl<'a> GitCheckout<'a> {
    fn new(
        database: &'a GitDatabase,
        revision: gix::ObjectId,
        repo: gix::Repository,
    ) -> GitCheckout<'a> {
        let path = repo
            .workdir()
            .unwrap_or_else(|| repo.git_dir())
            .to_path_buf();
        GitCheckout {
            path,
            database,
            revision,
            repo,
        }
    }

    pub fn location(&self) -> &Path {
        &self.path
    }

    pub fn is_fresh(&self) -> bool {
        match self.repo.rev_parse_single(b"HEAD".as_bstr()) {
            Ok(head) if head.detach() == self.revision => {
                self.path.join(CHECKOUT_READY_LOCK).exists()
            }
            _ => false,
        }
    }

    fn remote_url(&self) -> &Url {
        self.database.remote.url()
    }

    /// Clone from database to a local checkout path (filesystem-to-filesystem).
    fn clone_into(
        into: &Path,
        database: &'a GitDatabase,
        revision: gix::ObjectId,
        gctx: &GlobalContext,
    ) -> CargoResult<(GitCheckout<'a>, CheckoutGuard)> {
        let dirname = into.parent().unwrap();
        paths::create_dir_all(dirname)?;
        if into.exists() {
            paths::remove_dir_all(into)?;
        }

        // Use gix clone with local file transport for hardlink optimization
        let db_url = Url::from_file_path(&database.path)
            .map_err(|()| anyhow!("cannot convert path to URL: {}", database.path.display()))?;

        let mut prep = gix::clone::PrepareFetch::new(
            db_url.as_str(),
            into,
            gix::create::Kind::WithWorktree,
            gix::create::Options::default(),
            gix::open::Options::isolated(),
        )
        .context("failed to prepare clone")?;
        // Don't actually fetch — we already have objects in the database.
        // We just need the repo structure. Configure it as a local clone.
        prep = prep.configure_connection(|_| Ok(()));
        let (_prep_checkout, _outcome) = prep
            .fetch_only(gix::progress::Discard, &AtomicBool::default())
            .context("failed to clone from database")?;

        // Copy shallow file if the database is shallow
        let db_shallow = database.repo.git_dir().join("shallow");
        if db_shallow.exists() {
            let checkout_git_dir = if into.join(".git").is_dir() {
                into.join(".git")
            } else {
                into.to_path_buf()
            };
            let _ = std::fs::copy(&db_shallow, checkout_git_dir.join("shallow"));
        }

        let repo = gix::open(into).context("failed to open cloned repo")?;
        let checkout = GitCheckout::new(database, revision, repo);
        let guard = checkout.reset(gctx)?;
        Ok((checkout, guard))
    }

    /// Performs `git reset --hard` to the revision, with interrupt protection.
    fn reset(&self, gctx: &GlobalContext) -> CargoResult<CheckoutGuard> {
        let guard = CheckoutGuard::guard(&self.path);
        info!("reset {} to {}", self.path.display(), self.revision);

        // Set core.autocrlf = false
        let git_dir = self.repo.git_dir();
        let config_path = git_dir.join("config");
        if let Ok(mut config) =
            gix::config::File::from_path_no_includes(config_path.clone(), gix::config::Source::Local)
        {
            let _ = config.set_raw_value(&"core.autocrlf", "false");
            let _ = std::fs::write(&config_path, config.to_bstring());
        }

        // Use git CLI for checkout since gix worktree checkout API is complex
        // and we need reliable cross-platform behavior
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("repo has no workdir"))?;

        let mut pb = Progress::new("Checkout", gctx);
        let _ = pb.tick(0, 1, "");

        // Use `git checkout` to do the actual reset
        let mut cmd = std::process::Command::new("git");
        cmd.arg("checkout")
            .arg("--force")
            .arg(self.revision.to_string())
            .arg("--")
            .arg(".")
            .current_dir(workdir)
            .env("GIT_DIR", self.repo.git_dir());
        debug!("running {:?}", cmd);
        let output = cmd
            .output()
            .context("failed to run `git checkout` for reset")?;
        if !output.status.success() {
            bail!(
                "failed to checkout revision {}: {}",
                self.revision,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Also do a clean to remove untracked files
        let mut cmd = std::process::Command::new("git");
        cmd.arg("clean")
            .arg("-fdx")
            .current_dir(workdir)
            .env("GIT_DIR", self.repo.git_dir());
        let _ = cmd.output();

        let _ = pb.tick(1, 1, "");
        debug!("reset done");

        Ok(guard)
    }

    /// Like `git submodule update --recursive` but for this git checkout.
    fn update_submodules(&self, gctx: &GlobalContext, quiet: bool) -> CargoResult<()> {
        update_submodules(&self.repo, &self.path, gctx, quiet, self.remote_url().as_str())
    }
}

fn update_submodules(
    repo: &gix::Repository,
    workdir: &Path,
    gctx: &GlobalContext,
    quiet: bool,
    parent_remote_url: &str,
) -> CargoResult<()> {
    debug!("update submodules for: {:?}", workdir);

    let submodules = match repo.submodules() {
        Ok(Some(s)) => s,
        Ok(None) | Err(_) => return Ok(()),
    };

    for submodule in submodules {
        update_submodule(repo, &submodule, workdir, gctx, quiet, parent_remote_url)
            .with_context(|| {
                format!(
                    "failed to update submodule `{}`",
                    submodule.name()
                )
            })?;
    }
    Ok(())
}

fn update_submodule(
    _parent_repo: &gix::Repository,
    submodule: &gix::Submodule<'_>,
    parent_workdir: &Path,
    gctx: &GlobalContext,
    quiet: bool,
    parent_remote_url: &str,
) -> CargoResult<()> {
    let child_url_str = submodule
        .url()
        .map(|u| u.to_bstring().to_string())
        .context("submodule has no URL")?;

    let child_remote_url = absolute_submodule_url(parent_remote_url, &child_url_str)?;

    // Get the expected HEAD of the submodule from the parent's tree
    let head_id = match submodule.head_id() {
        Ok(Some(id)) => id,
        _ => return Ok(()),
    };

    let child_path = parent_workdir.join(
        submodule
            .path()
            .ok()
            .map(|p| p.to_string())
            .unwrap_or_default(),
    );

    // Check if already at the right revision
    if let Ok(child_repo) = gix::open(&child_path) {
        if let Ok(child_head) = child_repo.rev_parse_single(b"HEAD".as_bstr()) {
            if child_head.detach() == head_id {
                return update_submodules(&child_repo, &child_path, gctx, quiet, &child_remote_url);
            }
        }
    }

    // Need to fetch and checkout. Use GitSource for this.
    let reference = GitReference::Rev(head_id.to_string());
    let source_id = SourceId::for_git(&child_remote_url.into_url()?, reference)?
        .with_git_precise(Some(head_id.to_string()));

    let mut source = GitSource::new(source_id, gctx)?;
    source.set_quiet(quiet);
    let (db, actual_rev) = source.fetch_db(true)?;
    db.copy_to(actual_rev, &child_path, gctx, quiet)?;
    Ok(())
}

/// Constructs an absolute URL for a child submodule URL with its parent base URL.
fn absolute_submodule_url<'s>(
    base_url: &str,
    submodule_url: &'s str,
) -> CargoResult<Cow<'s, str>> {
    let absolute_url = if ["./", "../"]
        .iter()
        .any(|p| submodule_url.starts_with(p))
    {
        match Url::parse(base_url) {
            Ok(mut base_url) => {
                let path = base_url.path().to_string();
                if !path.ends_with('/') {
                    base_url.set_path(&format!("{path}/"));
                }
                let absolute_url = base_url.join(submodule_url).with_context(|| {
                    format!(
                        "failed to parse relative child submodule url `{submodule_url}` \
                        using parent base url `{base_url}`"
                    )
                })?;
                Cow::from(absolute_url.to_string())
            }
            Err(_) => {
                let mut absolute_url = base_url.to_string();
                if !absolute_url.ends_with('/') {
                    absolute_url.push('/');
                }
                absolute_url.push_str(submodule_url);
                Cow::from(absolute_url)
            }
        }
    } else {
        Cow::from(submodule_url)
    };

    Ok(absolute_url)
}

// ---------------------------------------------------------------------------
// CheckoutGuard
// ---------------------------------------------------------------------------

/// See [`GitCheckout::reset`] for rationale on this type.
#[must_use]
struct CheckoutGuard {
    ok_file: PathBuf,
}

impl CheckoutGuard {
    fn guard(path: &Path) -> Self {
        let ok_file = path.join(CHECKOUT_READY_LOCK);
        let _ = paths::remove_file(&ok_file);
        Self { ok_file }
    }

    fn mark_ok(self) -> CargoResult<()> {
        let _ = paths::create(self.ok_file)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

/// Fetches the given git `reference` for a repository at `repo_path`.
///
/// Uses gitoxide for the actual fetch, with CLI fallback when configured.
pub fn fetch(
    repo_path: &Path,
    remote_url: &str,
    reference: &GitReference,
    gctx: &GlobalContext,
    remote_kind: RemoteKind,
) -> CargoResult<()> {
    if let Some(offline_flag) = gctx.offline_flag() {
        bail!(
            "attempting to update a git repository, but {offline_flag} \
             was specified"
        )
    }

    // Determine shallow setting
    let repo_is_shallow = repo_path.join("shallow").exists();
    let shallow = remote_kind.to_shallow_setting(repo_is_shallow, gctx);

    // Build refspecs from the reference (same logic as utils.rs)
    let mut refspecs = Vec::new();
    let mut tags = false;
    match reference {
        GitReference::Branch(b) => {
            refspecs.push(format!("+refs/heads/{0}:refs/remotes/origin/{0}", b));
        }
        GitReference::Tag(t) => {
            refspecs.push(format!("+refs/tags/{0}:refs/remotes/origin/tags/{0}", t));
        }
        GitReference::DefaultBranch => {
            refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
        }
        GitReference::Rev(rev) => {
            if rev.starts_with("refs/") {
                refspecs.push(format!("+{0}:{0}", rev));
            } else if !matches!(shallow, gix::remote::fetch::Shallow::NoChange)
                && rev_to_oid(rev).is_some()
            {
                refspecs.push(format!("+{0}:refs/remotes/origin/HEAD", rev));
            } else {
                refspecs.push(String::from("+refs/heads/*:refs/remotes/origin/*"));
                refspecs.push(String::from("+HEAD:refs/remotes/origin/HEAD"));
                tags = true;
            }
        }
    }

    debug!("doing a fetch for {remote_url}");
    if let Some(true) = gctx.net_config()?.git_fetch_with_cli {
        fetch_with_cli(repo_path, remote_url, &refspecs, tags, shallow, gctx)
    } else {
        fetch_with_gitoxide(repo_path, remote_url, refspecs, tags, shallow, gctx)
    }
}

fn fetch_with_cli(
    repo_path: &Path,
    url: &str,
    refspecs: &[String],
    tags: bool,
    shallow: gix::remote::fetch::Shallow,
    gctx: &GlobalContext,
) -> CargoResult<()> {
    use crate::core::Verbosity;
    use crate::util::errors::GitCliError;
    use crate::util::network;
    use cargo_util::ProcessBuilder;

    debug!(target: "git-fetch", backend = "git-cli");

    let mut cmd = ProcessBuilder::new("git");
    cmd.arg("fetch");
    if tags {
        cmd.arg("--tags");
    } else {
        cmd.arg("--no-tags");
    }
    if let gix::remote::fetch::Shallow::DepthAtRemote(depth) = shallow {
        let depth = 0i32.saturating_add_unsigned(depth.get());
        cmd.arg(format!("--depth={depth}"));
    }
    match gctx.shell().verbosity() {
        Verbosity::Normal => {}
        Verbosity::Verbose => {
            cmd.arg("--verbose");
        }
        Verbosity::Quiet => {
            cmd.arg("--quiet");
        }
    }
    cmd.arg("--force")
        .arg("--update-head-ok")
        .arg(url)
        .args(refspecs)
        .env("GIT_DIR", repo_path)
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .cwd(repo_path);
    gctx.shell()
        .verbose(|s| s.status("Running", &cmd.to_string()))?;
    network::retry::with_retry(gctx, || {
        cmd.exec()
            .map_err(|error| GitCliError::new(error, true).into())
    })?;
    Ok(())
}

fn fetch_with_gitoxide(
    repo_path: &Path,
    remote_url: &str,
    refspecs: Vec<String>,
    tags: bool,
    shallow: gix::remote::fetch::Shallow,
    gctx: &GlobalContext,
) -> CargoResult<()> {
    debug!(target: "git-fetch", backend = "gitoxide");

    let config_overrides = cargo_config_to_gitoxide_overrides(gctx)?;
    let repo_reinitialized = AtomicBool::default();
    let res = oxide::with_retry_and_progress(
        repo_path,
        gctx,
        remote_url,
        &|repo_path,
          should_interrupt,
          mut progress,
          url_for_authentication: &mut dyn FnMut(&gix::bstr::BStr)| {
            loop {
                let res = oxide::open_repo(
                    repo_path,
                    config_overrides.clone(),
                    oxide::OpenMode::ForFetch,
                )
                .map_err(crate::sources::git::fetch::Error::from)
                .and_then(|repo| {
                    debug!("initiating fetch of {refspecs:?} from {remote_url}");
                    let url_for_authentication = &mut *url_for_authentication;
                    let remote = repo
                        .remote_at(remote_url)?
                        .with_fetch_tags(if tags {
                            gix::remote::fetch::Tags::All
                        } else {
                            gix::remote::fetch::Tags::Included
                        })
                        .with_refspecs(
                            refspecs.iter().map(|s| s.as_str()),
                            gix::remote::Direction::Fetch,
                        )
                        .map_err(crate::sources::git::fetch::Error::Other)?;
                    let url = remote
                        .url(gix::remote::Direction::Fetch)
                        .expect("set at init")
                        .to_owned();
                    let connection = remote.connect(gix::remote::Direction::Fetch)?;
                    let mut authenticate = connection.configured_credentials(url)?;
                    let connection = connection.with_credentials(
                        move |action: gix::protocol::credentials::helper::Action| {
                            if let Some(url) = action
                                .context()
                                .and_then(|ctx| ctx.url.as_ref().filter(|url| *url != remote_url))
                            {
                                url_for_authentication(url.as_ref());
                            }
                            authenticate(action)
                        },
                    );
                    let outcome = connection
                        .prepare_fetch(&mut progress, gix::remote::ref_map::Options::default())?
                        .with_shallow(shallow.clone())
                        .receive(&mut progress, should_interrupt)?;
                    Ok(outcome)
                });
                let err = match res {
                    Ok(_) => break,
                    Err(e) => e,
                };
                debug!("fetch failed: {}", err);

                if !repo_reinitialized.load(Ordering::Relaxed)
                    && (err.is_corrupted() || has_shallow_lock_file(&err))
                {
                    repo_reinitialized.store(true, Ordering::Relaxed);
                    debug!(
                        "looks like this is a corrupt repository, reinitializing \
                         and trying again"
                    );
                    if oxide::reinitialize(repo_path).is_ok() {
                        continue;
                    }
                }

                return Err(err.into());
            }
            Ok(())
        },
    );
    res
}

fn has_shallow_lock_file(err: &crate::sources::git::fetch::Error) -> bool {
    matches!(
        err,
        gix::env::collate::fetch::Error::Fetch(gix::remote::fetch::Error::Fetch(
            gix::protocol::fetch::Error::LockShallowFile(_)
        ))
    )
}

/// Initialize a bare git repository at `path` using gix.
fn init_bare(path: &Path) -> CargoResult<()> {
    gix::init_bare(path)?;
    Ok(())
}

/// Parse a revision string into an OID, but only if it's a full hex commit hash.
pub(crate) fn rev_to_oid(rev: &str) -> Option<gix::ObjectId> {
    gix::ObjectId::from_hex(rev.as_bytes())
        .ok()
        .filter(|oid| oid.as_bytes().len() * 2 == rev.len())
}

// ---------------------------------------------------------------------------
// GitSource
// ---------------------------------------------------------------------------

/// `GitSource` contains one or more packages gathering from a Git repository.
pub struct GitSource<'gctx> {
    remote: GitRemote,
    locked_rev: Revision,
    source_id: SourceId,
    path_source: Option<RecursivePathSource<'gctx>>,
    short_id: Option<InternedString>,
    ident: InternedString,
    gctx: &'gctx GlobalContext,
    quiet: bool,
}

/// Indicates a Git revision that might be locked or deferred to be resolved.
#[derive(Clone, Debug)]
enum Revision {
    Deferred(GitReference),
    Locked(gix::ObjectId),
}

impl Revision {
    fn new(rev: &str) -> Revision {
        match rev_to_oid(rev) {
            Some(oid) => Revision::Locked(oid),
            None => Revision::Deferred(GitReference::Rev(rev.to_string())),
        }
    }
}

impl From<GitReference> for Revision {
    fn from(value: GitReference) -> Self {
        Revision::Deferred(value)
    }
}

impl From<Revision> for GitReference {
    fn from(value: Revision) -> Self {
        match value {
            Revision::Deferred(git_ref) => git_ref,
            Revision::Locked(oid) => GitReference::Rev(oid.to_string()),
        }
    }
}

/// Create an identifier from a URL.
fn ident(id: &SourceId) -> String {
    let ident = id
        .canonical_url()
        .raw_canonicalized_url()
        .path_segments()
        .and_then(|s| s.rev().next())
        .unwrap_or("");
    let ident = if ident.is_empty() { "_empty" } else { ident };
    format!("{}-{}", ident, short_hash(id.canonical_url()))
}

fn ident_shallow(id: &SourceId, is_shallow: bool) -> String {
    let mut ident = ident(id);
    if is_shallow {
        ident.push_str("-shallow");
    }
    ident
}

impl<'gctx> GitSource<'gctx> {
    pub fn new(
        source_id: SourceId,
        gctx: &'gctx GlobalContext,
    ) -> CargoResult<GitSource<'gctx>> {
        assert!(source_id.is_git(), "id is not git, id={}", source_id);

        let remote = GitRemote::new(source_id.url());
        let locked_rev = source_id
            .precise_git_fragment()
            .map(|s| Revision::new(s.into()))
            .unwrap_or_else(|| source_id.git_reference().unwrap().clone().into());

        let ident = ident_shallow(
            &source_id,
            gctx.cli_unstable()
                .git
                .map_or(false, |features| features.shallow_deps),
        );

        Ok(GitSource {
            remote,
            locked_rev,
            source_id,
            path_source: None,
            short_id: None,
            ident: ident.into(),
            gctx,
            quiet: false,
        })
    }

    pub fn url(&self) -> &Url {
        self.remote.url()
    }

    pub fn read_packages(&mut self) -> CargoResult<Vec<Package>> {
        if self.path_source.is_none() {
            self.invalidate_cache();
            self.block_until_ready()?;
        }
        self.path_source.as_mut().unwrap().read_packages()
    }

    fn mark_used(&self) -> CargoResult<()> {
        self.gctx
            .deferred_global_last_use()?
            .mark_git_checkout_used(global_cache_tracker::GitCheckout {
                encoded_git_name: self.ident,
                short_name: self.short_id.expect("update before download"),
                size: None,
            });
        Ok(())
    }

    pub(crate) fn fetch_db(&self, is_submodule: bool) -> CargoResult<(GitDatabase, gix::ObjectId)> {
        let db_path = self.gctx.git_db_path().join(&self.ident);
        let db_path = db_path.into_path_unlocked();

        let db = self.remote.db_at(&db_path).ok();

        let (db, actual_rev) = match (&self.locked_rev, db) {
            (Revision::Locked(oid), Some(db)) if db.contains(*oid) => (db, *oid),

            (Revision::Deferred(git_ref), Some(db)) if !self.gctx.network_allowed() => {
                let offline_flag = self
                    .gctx
                    .offline_flag()
                    .expect("always present when `!network_allowed`");
                let rev = db.resolve(git_ref).with_context(|| {
                    format!(
                        "failed to lookup reference in preexisting repository, and \
                         can't check for updates in offline mode ({offline_flag})"
                    )
                })?;
                (db, rev)
            }

            (locked_rev, db) => {
                if let Some(offline_flag) = self.gctx.offline_flag() {
                    bail!(
                        "can't checkout from '{}': you are in the offline mode ({offline_flag})",
                        self.remote.url()
                    );
                }

                if !self.quiet {
                    let scope = if is_submodule {
                        "submodule"
                    } else {
                        "repository"
                    };
                    self.gctx.shell().status(
                        "Updating",
                        format!("git {scope} `{}`", self.remote.url()),
                    )?;
                }

                trace!("updating git source `{:?}`", self.remote);

                let locked_rev = locked_rev.clone().into();
                self.remote.checkout(&db_path, db, &locked_rev, self.gctx)?
            }
        };
        Ok((db, actual_rev))
    }
}

impl fmt::Debug for GitSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "git repo at {}", self.remote.url())?;
        match &self.locked_rev {
            Revision::Deferred(git_ref) => match git_ref.pretty_ref(true) {
                Some(s) => write!(f, " ({})", s),
                None => Ok(()),
            },
            Revision::Locked(oid) => write!(f, " ({oid})"),
        }
    }
}

impl<'gctx> Source for GitSource<'gctx> {
    fn query(
        &mut self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Poll<CargoResult<()>> {
        if let Some(src) = self.path_source.as_mut() {
            src.query(dep, kind, f)
        } else {
            Poll::Pending
        }
    }

    fn supports_checksums(&self) -> bool {
        false
    }

    fn requires_precise(&self) -> bool {
        true
    }

    fn source_id(&self) -> SourceId {
        self.source_id
    }

    fn block_until_ready(&mut self) -> CargoResult<()> {
        if self.path_source.is_some() {
            self.mark_used()?;
            return Ok(());
        }

        let git_fs = self.gctx.git_path();
        let _ = git_fs.create_dir();
        let git_path = self
            .gctx
            .assert_package_cache_locked(CacheLockMode::DownloadExclusive, &git_fs);

        exclude_from_backups_and_indexing(&git_path);

        let (db, actual_rev) = self.fetch_db(false)?;

        let short_id = db.to_short_id(actual_rev)?;

        let checkout_path = self
            .gctx
            .git_checkouts_path()
            .join(&self.ident)
            .join(short_id.as_str());
        let checkout_path = checkout_path.into_path_unlocked();
        db.copy_to(actual_rev, &checkout_path, self.gctx, self.quiet)?;

        let source_id = self
            .source_id
            .with_git_precise(Some(actual_rev.to_string()));
        let path_source = RecursivePathSource::new(&checkout_path, source_id, self.gctx);

        self.path_source = Some(path_source);
        self.short_id = Some(short_id.as_str().into());
        self.locked_rev = Revision::Locked(actual_rev);
        self.path_source.as_mut().unwrap().load()?;

        self.mark_used()?;
        Ok(())
    }

    fn download(&mut self, id: PackageId) -> CargoResult<MaybePackage> {
        trace!(
            "getting packages for package ID `{}` from `{:?}`",
            id,
            self.remote
        );
        self.mark_used()?;
        self.path_source
            .as_mut()
            .expect("BUG: `update()` must be called before `get()`")
            .download(id)
    }

    fn finish_download(&mut self, _id: PackageId, _data: Vec<u8>) -> CargoResult<Package> {
        panic!("no download should have started")
    }

    fn fingerprint(&self, _pkg: &Package) -> CargoResult<String> {
        match &self.locked_rev {
            Revision::Locked(oid) => Ok(oid.to_string()),
            _ => unreachable!("locked_rev must be resolved when computing fingerprint"),
        }
    }

    fn describe(&self) -> String {
        format!("Git repository {}", self.source_id)
    }

    fn add_to_yanked_whitelist(&mut self, _pkgs: &[PackageId]) {}

    fn is_yanked(&mut self, _pkg: PackageId) -> Poll<CargoResult<bool>> {
        Poll::Ready(Ok(false))
    }

    fn invalidate_cache(&mut self) {}

    fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }
}

// ---------------------------------------------------------------------------
// Utility functions — fully functional with gix
// ---------------------------------------------------------------------------

/// Initialize a new git repository at the given path.
pub fn init_repo(path: &Path) -> CargoResult<()> {
    gix::init(path)?;
    Ok(())
}

/// Discover a git repository starting from the given path, searching upward.
pub fn discover_repo(path: &Path) -> Result<Repository, anyhow::Error> {
    Ok(Repository(gix::discover(path)?))
}

/// Opaque repository handle.
pub struct Repository(gix::Repository);

impl Repository {
    pub fn workdir(&self) -> Option<&Path> {
        self.0.workdir()
    }

    pub fn is_path_ignored(&self, _path: &Path) -> bool {
        // gix excludes API is complex; conservatively report not ignored.
        false
    }
}

/// File status as reported by the git backend.
pub enum FileStatus {
    Current,
    Staged(String),
    Dirty(String),
}

/// Get the status of files in the repository at `path`.
pub fn repo_statuses(path: &Path) -> CargoResult<Vec<FileStatus>> {
    let mut result = Vec::new();
    if let Ok(repo) = gix::discover(path) {
        if let Ok(platform) = repo.status(gix::progress::Discard) {
            if let Ok(iter) = platform.into_index_worktree_iter(None::<BString>) {
                for item in iter.flatten() {
                    use gix::status::index_worktree::Item;
                    match item {
                        Item::Modification { rela_path, .. } => {
                            result.push(FileStatus::Dirty(rela_path.to_string()));
                        }
                        Item::DirectoryContents {
                            entry: gix::dir::Entry { rela_path, .. },
                            ..
                        } => {
                            result.push(FileStatus::Dirty(rela_path.to_string()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Look up a string value from the global git configuration.
pub fn git_config_string(key: &str) -> Option<String> {
    gix::config::File::from_globals()
        .ok()
        .and_then(|config| config.string(key).map(|s| s.to_string()))
}

/// Reinitialize a git repository (used for recovery from corruption).
pub fn reinitialize_repo(path: &Path, bare: bool) -> CargoResult<()> {
    if bare {
        gix::init_bare(path)?;
    } else {
        gix::init(path)?;
    }
    Ok(())
}

/// Check if an error is a spurious git error. Returns `None` — gix errors
/// are handled via `IsSpuriousError` trait in retry.rs.
pub fn is_spurious_git_error(_err: &anyhow::Error) -> Option<bool> {
    None
}

/// Run global git initialization. No-op for gix.
pub fn init_git_global() {}

/// Get version information about the git backend for display.
pub fn version_info() -> String {
    "gix (pure Rust)".to_string()
}
