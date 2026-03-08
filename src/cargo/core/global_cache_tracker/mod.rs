//! Support for tracking the last time files were used to assist with cleaning
//! up those files if they haven't been used in a while.
//!
//! Tracking of cache files is stored in a persistent database which contains a
//! timestamp of the last time the file was used, as well as the size of the
//! file. The storage backend is selected at compile time: with the `sqlite`
//! feature enabled, a SQLite database is used; without it, a JSON flat file
//! provides the same functionality without any C dependencies.
//!
//! While cargo is running, when it detects a use of a cache file, it adds a
//! timestamp to [`DeferredGlobalLastUse`]. This batches up a set of changes
//! that are then flushed to the database all at once (via
//! [`DeferredGlobalLastUse::save`]). Ideally saving would only be done once
//! for performance reasons, but that is not really possible due to the way
//! cargo works, since there are different ways cargo can be used (like `cargo
//! generate-lockfile`, `cargo fetch`, and `cargo build` are all very
//! different ways the code is used).
//!
//! All of the database interaction is done through the [`GlobalCacheTracker`]
//! type.
//!
//! There is a single global [`GlobalCacheTracker`] and
//! [`DeferredGlobalLastUse`] stored in [`GlobalContext`].
//!
//! The high-level interface for performing garbage collection is defined in
//! the [`crate::core::gc`] module. The functions there are responsible for
//! interacting with the [`GlobalCacheTracker`] to handle cleaning of global
//! cache data.
//!
//! ## Automatic gc
//!
//! Some commands (primarily the build commands) will trigger an automatic
//! deletion of files that haven't been used in a while. The high-level
//! interface for this is the [`crate::core::gc::auto_gc`] function.
//!
//! The [`GlobalCacheTracker`] database tracks the last time an automatic gc
//! was performed so that it is only done once per day for performance
//! reasons.
//!
//! ## Manual gc
//!
//! The user can perform a manual garbage collection with the `cargo clean`
//! command. That command has a variety of options to specify what to delete.
//! Manual gc supports deleting based on age or size or both. From a
//! high-level, this is done by the [`crate::core::gc::Gc::gc`] method, which
//! calls into [`GlobalCacheTracker`] to handle all the cleaning.
//!
//! ## Locking
//!
//! Usage of the database requires that the package cache is locked to prevent
//! concurrent access. See [`crate::util::cache_lock`] for more detail on
//! locking.
//!
//! When garbage collection is being performed, the package cache lock must be
//! in [`CacheLockMode::MutateExclusive`] to ensure no other cargo process is
//! running.
//!
//! When performing automatic gc, [`crate::core::gc::auto_gc`] will skip the
//! GC if the package cache lock is already held by anything else. Automatic
//! GC is intended to be opportunistic, and should impose as little disruption
//! to the user as possible.
//!
//! ## Compatibility
//!
//! The database must retain both forwards and backwards compatibility between
//! different versions of cargo.
//!
//! Since users may run older versions of cargo that do not do cache tracking,
//! the [`GlobalCacheTracker`] synchronizes with the filesystem during garbage
//! collection to handle entries created by older versions.
//!
//! ## Performance
//!
//! A lot of focus on the design of this system is to minimize the performance
//! impact. Every build command needs to save updates which we try to avoid
//! having a noticeable impact on build times.

#[cfg(feature = "sqlite")]
mod sqlite_backend;
#[cfg(not(feature = "sqlite"))]
mod flatfile_backend;

#[cfg(feature = "sqlite")]
use self::sqlite_backend::SqliteBackend as BackendImpl;
#[cfg(not(feature = "sqlite"))]
use self::flatfile_backend::FlatFileBackend as BackendImpl;

use crate::core::Verbosity;
use crate::core::gc::GcOpts;
use crate::ops::CleanContext;
use crate::util::cache_lock::CacheLockMode;
use crate::util::interning::InternedString;
use crate::util::{Filesystem, Progress, ProgressStyle};
use crate::{CargoResult, GlobalContext};
use anyhow::Context as _;
use cargo_util::paths;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tracing::{debug, trace};

/// How often timestamps will be updated.
///
/// As an optimization timestamps are not updated unless they are older than
/// the given number of seconds. This helps reduce the amount of disk I/O when
/// running cargo multiple times within a short window.
pub(crate) const UPDATE_RESOLUTION: u64 = 60 * 5;

/// Type for timestamps as stored in the database.
///
/// These are seconds since the Unix epoch.
pub(crate) type Timestamp = u64;

/// The key for a registry index entry stored in the database.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct RegistryIndex {
    /// A unique name of the registry source.
    pub encoded_registry_name: InternedString,
}

/// The key for a registry `.crate` entry stored in the database.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct RegistryCrate {
    /// A unique name of the registry source.
    pub encoded_registry_name: InternedString,
    /// The filename of the compressed crate, like `foo-1.2.3.crate`.
    pub crate_filename: InternedString,
    /// The size of the `.crate` file.
    pub size: u64,
}

/// The key for a registry src directory entry stored in the database.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct RegistrySrc {
    /// A unique name of the registry source.
    pub encoded_registry_name: InternedString,
    /// The directory name of the extracted source, like `foo-1.2.3`.
    pub package_dir: InternedString,
    /// Total size of the src directory in bytes.
    ///
    /// This can be None when the size is unknown.
    pub size: Option<u64>,
}

/// The key for a git db entry stored in the database.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct GitDb {
    /// A unique name of the git database.
    pub encoded_git_name: InternedString,
}

/// The key for a git checkout entry stored in the database.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct GitCheckout {
    /// A unique name of the git database.
    pub encoded_git_name: InternedString,
    /// A unique name of the checkout without the database.
    pub short_name: InternedString,
    /// Total size of the checkout directory.
    ///
    /// This can be None when the size is unknown.
    pub size: Option<u64>,
}

/// Filesystem paths in the global cache.
///
/// Accessing these assumes a lock has already been acquired.
pub(crate) struct BasePaths {
    /// Root path to the index caches.
    pub index: PathBuf,
    /// Root path to the git DBs.
    pub git_db: PathBuf,
    /// Root path to the git checkouts.
    pub git_co: PathBuf,
    /// Root path to the `.crate` files.
    pub crate_dir: PathBuf,
    /// Root path to the `src` directories.
    pub src: PathBuf,
}

/// Tracking for the global shared cache (registry files, etc.).
///
/// This is the interface to the global cache database, used for tracking and
/// cleaning. See the [`crate::core::global_cache_tracker`] module docs for
/// details.
#[derive(Debug)]
pub struct GlobalCacheTracker {
    /// The storage backend.
    backend: BackendImpl,
    /// This is an optimization used to make sure cargo only checks if gc
    /// needs to run once per session.
    auto_gc_checked_this_session: bool,
}

impl GlobalCacheTracker {
    /// Creates a new [`GlobalCacheTracker`].
    ///
    /// The caller is responsible for locking the package cache with
    /// [`CacheLockMode::DownloadExclusive`] before calling this.
    pub fn new(gctx: &GlobalContext) -> CargoResult<GlobalCacheTracker> {
        let db_path = Self::db_path(gctx);
        let _lock = gctx.assert_package_cache_locked(CacheLockMode::DownloadExclusive, &db_path);
        let backend = BackendImpl::open(db_path.as_path_unlocked())?;
        Ok(GlobalCacheTracker {
            backend,
            auto_gc_checked_this_session: false,
        })
    }

    /// The path to the database.
    pub fn db_path(gctx: &GlobalContext) -> Filesystem {
        gctx.home().join(BackendImpl::FILENAME)
    }

    /// Returns all index cache timestamps.
    pub fn registry_index_all(&self) -> CargoResult<Vec<(RegistryIndex, Timestamp)>> {
        self.backend.registry_index_all()
    }

    /// Returns all registry crate cache timestamps.
    pub fn registry_crate_all(&self) -> CargoResult<Vec<(RegistryCrate, Timestamp)>> {
        self.backend.registry_crate_all()
    }

    /// Returns all registry source cache timestamps.
    pub fn registry_src_all(&self) -> CargoResult<Vec<(RegistrySrc, Timestamp)>> {
        self.backend.registry_src_all()
    }

    /// Returns all git db timestamps.
    pub fn git_db_all(&self) -> CargoResult<Vec<(GitDb, Timestamp)>> {
        self.backend.git_db_all()
    }

    /// Returns all git checkout timestamps.
    pub fn git_checkout_all(&self) -> CargoResult<Vec<(GitCheckout, Timestamp)>> {
        self.backend.git_checkout_all()
    }

    /// Returns whether or not an auto GC should be performed.
    pub fn should_run_auto_gc(&mut self, frequency: Duration) -> CargoResult<bool> {
        trace!(target: "gc", "should_run_auto_gc");
        if self.auto_gc_checked_this_session {
            return Ok(false);
        }
        let last_auto_gc = self.backend.last_auto_gc()?;
        let should_run = last_auto_gc + frequency.as_secs() < now();
        trace!(target: "gc",
            "last auto gc was {}, {}",
            last_auto_gc,
            if should_run { "running" } else { "skipping" }
        );
        self.auto_gc_checked_this_session = true;
        Ok(should_run)
    }

    /// Writes to the database to indicate that an automatic GC has just been
    /// completed.
    pub fn set_last_auto_gc(&mut self) -> CargoResult<()> {
        self.backend.set_last_auto_gc(now())
    }

    /// Deletes files from the global cache based on the given options.
    pub fn clean(&mut self, clean_ctx: &mut CleanContext<'_>, gc_opts: &GcOpts) -> CargoResult<()> {
        self.clean_inner(clean_ctx, gc_opts)
            .context("failed to clean entries from the global cache")
    }

    #[tracing::instrument(skip_all)]
    fn clean_inner(
        &mut self,
        clean_ctx: &mut CleanContext<'_>,
        gc_opts: &GcOpts,
    ) -> CargoResult<()> {
        let gctx = clean_ctx.gctx;
        let base = BasePaths {
            index: gctx.registry_index_path().into_path_unlocked(),
            git_db: gctx.git_db_path().into_path_unlocked(),
            git_co: gctx.git_checkouts_path().into_path_unlocked(),
            crate_dir: gctx.registry_cache_path().into_path_unlocked(),
            src: gctx.registry_source_path().into_path_unlocked(),
        };
        let now = now();
        trace!(target: "gc", "cleaning {gc_opts:?}");
        self.backend.begin()?;
        let mut delete_paths = Vec::new();

        if gc_opts.is_download_cache_opt_set() {
            self.sync_db_with_files(now, gctx, &base, gc_opts.is_download_cache_size_set(), &mut delete_paths)
                .context("failed to sync tracking database")?;
        }

        if let Some(max_age) = gc_opts.max_index_age {
            let max_ts = now - max_age.as_secs();
            self.clean_registry_index_by_age(max_ts, &base, &mut delete_paths)?;
        }
        if let Some(max_age) = gc_opts.max_src_age {
            let max_ts = now - max_age.as_secs();
            self.clean_registry_items_by_age_src(max_ts, &base, &mut delete_paths)?;
        }
        if let Some(max_age) = gc_opts.max_crate_age {
            let max_ts = now - max_age.as_secs();
            self.clean_registry_items_by_age_crate(max_ts, &base, &mut delete_paths)?;
        }
        if let Some(max_age) = gc_opts.max_git_db_age {
            let max_ts = now - max_age.as_secs();
            self.clean_git_db_by_age(max_ts, &base, &mut delete_paths)?;
        }
        if let Some(max_age) = gc_opts.max_git_co_age {
            let max_ts = now - max_age.as_secs();
            self.clean_git_co_by_age(max_ts, &base, &mut delete_paths)?;
        }
        if let Some(max_size) = gc_opts.max_crate_size {
            self.clean_registry_items_by_size_crate(max_size, &base, &mut delete_paths)?;
        }
        if let Some(max_size) = gc_opts.max_src_size {
            self.clean_registry_items_by_size_src(max_size, &base, &mut delete_paths)?;
        }
        if let Some(max_size) = gc_opts.max_git_size {
            self.clean_git_by_size(max_size, &base, &mut delete_paths)?;
        }
        if let Some(max_size) = gc_opts.max_download_size {
            self.clean_registry_items_by_size_both(max_size, &base, &mut delete_paths)?;
        }

        clean_ctx.remove_paths(&delete_paths)?;

        if clean_ctx.dry_run {
            self.backend.rollback()?;
        } else {
            self.backend.commit()?;
        }
        Ok(())
    }

    fn clean_registry_index_by_age(
        &mut self,
        max_ts: Timestamp,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning index since {max_ts:?}");
        let entries = self.backend.registry_index_all()?;
        for (index, ts) in entries {
            if ts < max_ts {
                let name = index.encoded_registry_name.as_str();
                self.backend.delete_registry_index(name)?;
                delete_paths.push(base.index.join(name));
                delete_paths.push(base.src.join(name));
                delete_paths.push(base.crate_dir.join(name));
            }
        }
        Ok(())
    }

    fn clean_registry_items_by_age_src(
        &mut self,
        max_ts: Timestamp,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning registry_src since {max_ts:?}");
        let entries = self.backend.registry_src_all()?;
        for (src, ts) in entries {
            if ts < max_ts {
                let registry = src.encoded_registry_name.as_str();
                let name = src.package_dir.as_str();
                self.backend.delete_registry_src(registry, name)?;
                delete_paths.push(base.src.join(registry).join(name));
            }
        }
        Ok(())
    }

    fn clean_registry_items_by_age_crate(
        &mut self,
        max_ts: Timestamp,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning registry_crate since {max_ts:?}");
        let entries = self.backend.registry_crate_all()?;
        for (krate, ts) in entries {
            if ts < max_ts {
                let registry = krate.encoded_registry_name.as_str();
                let name = krate.crate_filename.as_str();
                self.backend.delete_registry_crate(registry, name)?;
                delete_paths.push(base.crate_dir.join(registry).join(name));
            }
        }
        Ok(())
    }

    fn clean_git_db_by_age(
        &mut self,
        max_ts: Timestamp,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning git db since {max_ts:?}");
        let entries = self.backend.git_db_all()?;
        for (db, ts) in entries {
            if ts < max_ts {
                let name = db.encoded_git_name.as_str();
                self.backend.delete_git_db(name)?;
                delete_paths.push(base.git_db.join(name));
                delete_paths.push(base.git_co.join(name));
            }
        }
        Ok(())
    }

    fn clean_git_co_by_age(
        &mut self,
        max_ts: Timestamp,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning git co since {max_ts:?}");
        let entries = self.backend.git_checkout_all()?;
        for (co, ts) in entries {
            if ts < max_ts {
                let db_name = co.encoded_git_name.as_str();
                let name = co.short_name.as_str();
                self.backend.delete_git_checkout(db_name, name)?;
                delete_paths.push(base.git_co.join(db_name).join(name));
            }
        }
        Ok(())
    }

    fn clean_registry_items_by_size_crate(
        &mut self,
        max_size: u64,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning registry_crate till under {max_size:?}");
        let mut entries = self.backend.registry_crate_all()?;
        let total_size: u64 = entries.iter().map(|(c, _)| c.size).sum();
        if total_size <= max_size {
            return Ok(());
        }
        // Sort oldest first, name for determinism.
        entries.sort_by(|a, b| (a.1, &a.0.crate_filename).cmp(&(b.1, &b.0.crate_filename)));
        let mut remaining = total_size;
        for (krate, _ts) in &entries {
            if remaining <= max_size {
                break;
            }
            let registry = krate.encoded_registry_name.as_str();
            let name = krate.crate_filename.as_str();
            self.backend.delete_registry_crate(registry, name)?;
            delete_paths.push(base.crate_dir.join(registry).join(name));
            remaining -= krate.size;
        }
        Ok(())
    }

    fn clean_registry_items_by_size_src(
        &mut self,
        max_size: u64,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning registry_src till under {max_size:?}");
        let mut entries = self.backend.registry_src_all()?;
        let total_size: u64 = entries.iter().filter_map(|(s, _)| s.size).sum();
        if total_size <= max_size {
            return Ok(());
        }
        entries.sort_by(|a, b| (a.1, &a.0.package_dir).cmp(&(b.1, &b.0.package_dir)));
        let mut remaining = total_size;
        for (src, _ts) in &entries {
            if remaining <= max_size {
                break;
            }
            let Some(size) = src.size else { continue };
            let registry = src.encoded_registry_name.as_str();
            let name = src.package_dir.as_str();
            self.backend.delete_registry_src(registry, name)?;
            delete_paths.push(base.src.join(registry).join(name));
            remaining -= size;
        }
        Ok(())
    }

    fn clean_git_by_size(
        &mut self,
        max_size: u64,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning git till under {max_size:?}");

        // Collect git_db entries with their filesystem sizes.
        let db_entries = self.backend.git_db_all()?;
        let co_entries = self.backend.git_checkout_all()?;

        // (timestamp, is_db, db_name, co_name, size)
        let mut all_entries: Vec<(Timestamp, bool, String, String, u64)> = Vec::new();
        for (db, ts) in &db_entries {
            let size = cargo_util::du(&base.git_db.join(db.encoded_git_name.as_str()), &[]).unwrap_or(0);
            all_entries.push((*ts, true, db.encoded_git_name.to_string(), String::new(), size));
        }
        for (co, ts) in &co_entries {
            if let Some(size) = co.size {
                all_entries.push((*ts, false, co.encoded_git_name.to_string(), co.short_name.to_string(), size));
            }
        }

        // Sort oldest last so we can pop from the end.
        all_entries.sort_by(|a, b| (b.0, &b.3).cmp(&(a.0, &a.3)));

        let mut total_size: u64 = all_entries.iter().map(|e| e.4).sum();
        debug!(target: "gc", "total git cache size appears to be {total_size}");

        while let Some((_, is_db, db_name, co_name, size)) = all_entries.pop() {
            if total_size <= max_size {
                break;
            }
            if is_db {
                total_size -= size;
                delete_paths.push(base.git_db.join(&db_name));
                self.backend.delete_git_db(&db_name)?;
                // Also remove all checkouts for this db.
                let mut i = 0;
                while i < all_entries.len() {
                    if !all_entries[i].1 && all_entries[i].2 == db_name {
                        let removed = all_entries.remove(i);
                        delete_paths.push(base.git_co.join(&removed.2).join(&removed.3));
                        self.backend.delete_git_checkout(&removed.2, &removed.3)?;
                        total_size -= removed.4;
                    } else {
                        i += 1;
                    }
                }
            } else {
                delete_paths.push(base.git_co.join(&db_name).join(&co_name));
                self.backend.delete_git_checkout(&db_name, &co_name)?;
                total_size -= size;
            }
        }
        Ok(())
    }

    fn clean_registry_items_by_size_both(
        &mut self,
        max_size: u64,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "cleaning download till under {max_size:?}");

        // Combine crate and src entries, sorted by timestamp.
        // (is_src, registry_name, name, size, timestamp)
        let mut combined: Vec<(bool, String, String, u64, Timestamp)> = Vec::new();

        let src_entries = self.backend.registry_src_all()?;
        for (src, ts) in src_entries {
            if let Some(size) = src.size {
                combined.push((true, src.encoded_registry_name.to_string(), src.package_dir.to_string(), size, ts));
            }
        }
        let crate_entries = self.backend.registry_crate_all()?;
        for (krate, ts) in crate_entries {
            combined.push((false, krate.encoded_registry_name.to_string(), krate.crate_filename.to_string(), krate.size, ts));
        }

        combined.sort_by(|a, b| (a.4, &a.2).cmp(&(b.4, &b.2)));

        let mut total_size: u64 = combined.iter().map(|e| e.3).sum();
        debug!(target: "gc", "total download cache size appears to be {total_size}");

        for (is_src, registry, name, size, _ts) in &combined {
            if total_size <= max_size {
                break;
            }
            if *is_src {
                delete_paths.push(base.src.join(registry).join(name));
                self.backend.delete_registry_src(registry, name)?;
            } else {
                delete_paths.push(base.crate_dir.join(registry).join(name));
                self.backend.delete_registry_crate(registry, name)?;
            }
            total_size -= size;
        }
        Ok(())
    }

    /// Synchronizes the database to match the files on disk.
    #[tracing::instrument(skip_all)]
    fn sync_db_with_files(
        &mut self,
        now: Timestamp,
        gctx: &GlobalContext,
        base: &BasePaths,
        sync_size: bool,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        debug!(target: "gc", "starting db sync");

        // Add any parent directories on disk that aren't tracked yet.
        self.sync_parent_entries(now, &base.index, true)?;
        self.sync_parent_entries(now, &base.git_db, false)?;

        // Remove child entries from db that aren't on disk.
        self.remove_stale_children_registry_crate(&base.crate_dir)?;
        self.remove_stale_children_registry_src(&base.src)?;
        self.remove_stale_children_git_checkout(&base.git_co)?;

        // Remove parent entries not on disk, and collect orphaned child paths.
        self.remove_stale_parents_registry(&base, delete_paths)?;
        self.remove_stale_parents_git(&base, delete_paths)?;

        // Add any child entries on disk that aren't tracked yet.
        self.populate_untracked_crate(now, &base.crate_dir)?;
        self.populate_untracked_registry_src(now, gctx, &base.src, sync_size)?;
        self.populate_untracked_git_checkout(now, gctx, &base.git_co, sync_size)?;

        // Fill in NULL sizes if needed.
        if sync_size {
            self.update_null_sizes_registry_src(gctx, &base.src)?;
            self.update_null_sizes_git_checkout(gctx, &base.git_co)?;
        }
        Ok(())
    }

    /// Adds any parent directories present on disk but not in the database.
    fn sync_parent_entries(
        &mut self,
        now: Timestamp,
        base_path: &Path,
        is_registry: bool,
    ) -> CargoResult<()> {
        let names = list_dir_names(base_path)?;
        for name in names {
            if is_registry {
                self.backend.insert_registry_index_if_missing(&name, now)?;
            } else {
                self.backend.insert_git_db_if_missing(&name, now)?;
            }
        }
        Ok(())
    }

    /// Removes child entries from the database that no longer exist on disk.
    fn remove_stale_children_registry_crate(&mut self, base_path: &Path) -> CargoResult<()> {
        trace!(target: "gc", "removing stale registry_crate entries");
        let entries = self.backend.registry_crate_all()?;
        for (krate, _ts) in entries {
            let registry = krate.encoded_registry_name.as_str();
            let name = krate.crate_filename.as_str();
            if !base_path.join(registry).join(name).exists() {
                self.backend.delete_registry_crate(registry, name)?;
            }
        }
        Ok(())
    }

    fn remove_stale_children_registry_src(&mut self, base_path: &Path) -> CargoResult<()> {
        trace!(target: "gc", "removing stale registry_src entries");
        let entries = self.backend.registry_src_all()?;
        for (src, _ts) in entries {
            let registry = src.encoded_registry_name.as_str();
            let name = src.package_dir.as_str();
            if !base_path.join(registry).join(name).exists() {
                self.backend.delete_registry_src(registry, name)?;
            }
        }
        Ok(())
    }

    fn remove_stale_children_git_checkout(&mut self, base_path: &Path) -> CargoResult<()> {
        trace!(target: "gc", "removing stale git_checkout entries");
        let entries = self.backend.git_checkout_all()?;
        for (co, _ts) in entries {
            let db = co.encoded_git_name.as_str();
            let name = co.short_name.as_str();
            if !base_path.join(db).join(name).exists() {
                self.backend.delete_git_checkout(db, name)?;
            }
        }
        Ok(())
    }

    /// Remove parent entries not on disk and collect orphaned child paths.
    fn remove_stale_parents_registry(
        &mut self,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        trace!(target: "gc", "removing stale registry_index entries");
        let entries = self.backend.registry_index_all()?;
        for (index, _ts) in entries {
            let name = index.encoded_registry_name.as_str();
            if !base.index.join(name).exists() {
                self.backend.delete_registry_index(name)?;
                for child_base in &[&base.crate_dir, &base.src] {
                    let child_path = child_base.join(name);
                    if child_path.exists() {
                        debug!(target: "gc", "removing orphaned path {child_path:?}");
                        delete_paths.push(child_path);
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_stale_parents_git(
        &mut self,
        base: &BasePaths,
        delete_paths: &mut Vec<PathBuf>,
    ) -> CargoResult<()> {
        trace!(target: "gc", "removing stale git_db entries");
        let entries = self.backend.git_db_all()?;
        for (db, _ts) in entries {
            let name = db.encoded_git_name.as_str();
            if !base.git_db.join(name).exists() {
                self.backend.delete_git_db(name)?;
                let child_path = base.git_co.join(name);
                if child_path.exists() {
                    debug!(target: "gc", "removing orphaned path {child_path:?}");
                    delete_paths.push(child_path);
                }
            }
        }
        Ok(())
    }

    /// Populates untracked `.crate` files.
    #[tracing::instrument(skip_all)]
    fn populate_untracked_crate(
        &mut self,
        now: Timestamp,
        base_path: &Path,
    ) -> CargoResult<()> {
        trace!(target: "gc", "populating untracked crate files");
        let index_names = list_dir_names(base_path)?;
        for index_name in index_names {
            let index_path = base_path.join(&index_name);
            let crates = read_dir_with_filter(&index_path, &|entry| {
                entry.file_type().map_or(false, |ty| ty.is_file())
                    && entry
                        .file_name()
                        .to_str()
                        .map_or(false, |name| name.ends_with(".crate"))
            })?;
            for crate_name in crates {
                let size = paths::metadata(index_path.join(&crate_name))?.len();
                self.backend.insert_registry_crate_if_missing(&index_name, &crate_name, size, now)?;
            }
        }
        Ok(())
    }

    /// Populates untracked registry src directories.
    #[tracing::instrument(skip_all)]
    fn populate_untracked_registry_src(
        &mut self,
        now: Timestamp,
        gctx: &GlobalContext,
        base_path: &Path,
        populate_size: bool,
    ) -> CargoResult<()> {
        trace!(target: "gc", "populating untracked registry src");
        let id_names = list_dir_names(base_path)?;
        let mut progress = Progress::with_style("Scanning", ProgressStyle::Ratio, gctx);
        for id_name in id_names {
            let index_path = base_path.join(&id_name);
            let names = list_dir_names(&index_path)?;
            let max = names.len();
            for (i, name) in names.iter().enumerate() {
                if self.backend.registry_src_exists(&id_name, name)? {
                    continue;
                }
                let dir_path = index_path.join(name);
                if !dir_path.is_dir() {
                    continue;
                }
                progress.tick(i, max, "")?;
                let size = if populate_size {
                    Some(du(&dir_path, "registry_src")?)
                } else {
                    None
                };
                self.backend.insert_registry_src_if_missing(&id_name, name, size, now)?;
            }
        }
        Ok(())
    }

    /// Populates untracked git checkout directories.
    #[tracing::instrument(skip_all)]
    fn populate_untracked_git_checkout(
        &mut self,
        now: Timestamp,
        gctx: &GlobalContext,
        base_path: &Path,
        populate_size: bool,
    ) -> CargoResult<()> {
        trace!(target: "gc", "populating untracked git checkouts");
        let id_names = list_dir_names(base_path)?;
        let mut progress = Progress::with_style("Scanning", ProgressStyle::Ratio, gctx);
        for id_name in id_names {
            let index_path = base_path.join(&id_name);
            let names = list_dir_names(&index_path)?;
            let max = names.len();
            for (i, name) in names.iter().enumerate() {
                if self.backend.git_checkout_exists(&id_name, name)? {
                    continue;
                }
                let dir_path = index_path.join(name);
                if !dir_path.is_dir() {
                    continue;
                }
                progress.tick(i, max, "")?;
                let size = if populate_size {
                    Some(du(&dir_path, "git_checkout")?)
                } else {
                    None
                };
                self.backend.insert_git_checkout_if_missing(&id_name, name, size, now)?;
            }
        }
        Ok(())
    }

    /// Fills in NULL sizes for registry src entries.
    fn update_null_sizes_registry_src(
        &mut self,
        gctx: &GlobalContext,
        base_path: &Path,
    ) -> CargoResult<()> {
        trace!(target: "gc", "updating NULL size information in registry_src");
        let entries = self.backend.registry_src_all()?;
        let null_entries: Vec<_> = entries.into_iter().filter(|(s, _)| s.size.is_none()).collect();
        let mut progress = Progress::with_style("Scanning", ProgressStyle::Ratio, gctx);
        let max = null_entries.len();
        for (i, (src, _ts)) in null_entries.iter().enumerate() {
            let registry = src.encoded_registry_name.as_str();
            let name = src.package_dir.as_str();
            let path = base_path.join(registry).join(name);
            progress.tick(i, max, "")?;
            let size = du(&path, "registry_src")?;
            self.backend.update_registry_src_size(registry, name, size)?;
        }
        Ok(())
    }

    /// Fills in NULL sizes for git checkout entries.
    fn update_null_sizes_git_checkout(
        &mut self,
        gctx: &GlobalContext,
        base_path: &Path,
    ) -> CargoResult<()> {
        trace!(target: "gc", "updating NULL size information in git_checkout");
        let entries = self.backend.git_checkout_all()?;
        let null_entries: Vec<_> = entries.into_iter().filter(|(c, _)| c.size.is_none()).collect();
        let mut progress = Progress::with_style("Scanning", ProgressStyle::Ratio, gctx);
        let max = null_entries.len();
        for (i, (co, _ts)) in null_entries.iter().enumerate() {
            let db = co.encoded_git_name.as_str();
            let name = co.short_name.as_str();
            let path = base_path.join(db).join(name);
            progress.tick(i, max, "")?;
            let size = du(&path, "git_checkout")?;
            self.backend.update_git_checkout_size(db, name, size)?;
        }
        Ok(())
    }
}

/// This is a cache of modifications that will be saved to disk all at once
/// via the [`DeferredGlobalLastUse::save`] method.
///
/// This is here to improve performance.
#[derive(Debug)]
pub struct DeferredGlobalLastUse {
    /// New registry index entries to insert.
    pub(crate) registry_index_timestamps: HashMap<RegistryIndex, Timestamp>,
    /// New registry `.crate` entries to insert.
    pub(crate) registry_crate_timestamps: HashMap<RegistryCrate, Timestamp>,
    /// New registry src directory entries to insert.
    pub(crate) registry_src_timestamps: HashMap<RegistrySrc, Timestamp>,
    /// New git db entries to insert.
    pub(crate) git_db_timestamps: HashMap<GitDb, Timestamp>,
    /// New git checkout entries to insert.
    pub(crate) git_checkout_timestamps: HashMap<GitCheckout, Timestamp>,
    /// This is used so that a warning about failing to update the database is
    /// only displayed once.
    save_err_has_warned: bool,
    /// The current time, used to improve performance to avoid accessing the
    /// clock hundreds of times.
    now: Timestamp,
}

impl DeferredGlobalLastUse {
    pub fn new() -> DeferredGlobalLastUse {
        DeferredGlobalLastUse {
            registry_index_timestamps: HashMap::new(),
            registry_crate_timestamps: HashMap::new(),
            registry_src_timestamps: HashMap::new(),
            git_db_timestamps: HashMap::new(),
            git_checkout_timestamps: HashMap::new(),
            save_err_has_warned: false,
            now: now(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.registry_index_timestamps.is_empty()
            && self.registry_crate_timestamps.is_empty()
            && self.registry_src_timestamps.is_empty()
            && self.git_db_timestamps.is_empty()
            && self.git_checkout_timestamps.is_empty()
    }

    fn clear(&mut self) {
        self.registry_index_timestamps.clear();
        self.registry_crate_timestamps.clear();
        self.registry_src_timestamps.clear();
        self.git_db_timestamps.clear();
        self.git_checkout_timestamps.clear();
    }

    /// Indicates the given [`RegistryIndex`] has been used right now.
    pub fn mark_registry_index_used(&mut self, registry_index: RegistryIndex) {
        self.mark_registry_index_used_stamp(registry_index, None);
    }

    /// Indicates the given [`RegistryCrate`] has been used right now.
    ///
    /// Also implicitly marks the index used, too.
    pub fn mark_registry_crate_used(&mut self, registry_crate: RegistryCrate) {
        self.mark_registry_crate_used_stamp(registry_crate, None);
    }

    /// Indicates the given [`RegistrySrc`] has been used right now.
    ///
    /// Also implicitly marks the index used, too.
    pub fn mark_registry_src_used(&mut self, registry_src: RegistrySrc) {
        self.mark_registry_src_used_stamp(registry_src, None);
    }

    /// Indicates the given [`GitCheckout`] has been used right now.
    ///
    /// Also implicitly marks the git db used, too.
    pub fn mark_git_checkout_used(&mut self, git_checkout: GitCheckout) {
        self.mark_git_checkout_used_stamp(git_checkout, None);
    }

    /// Indicates the given [`RegistryIndex`] has been used with the given
    /// time (or "now" if `None`).
    pub fn mark_registry_index_used_stamp(
        &mut self,
        registry_index: RegistryIndex,
        timestamp: Option<&SystemTime>,
    ) {
        let timestamp = timestamp.map_or(self.now, to_timestamp);
        self.registry_index_timestamps
            .insert(registry_index, timestamp);
    }

    /// Indicates the given [`RegistryCrate`] has been used with the given
    /// time (or "now" if `None`).
    ///
    /// Also implicitly marks the index used, too.
    pub fn mark_registry_crate_used_stamp(
        &mut self,
        registry_crate: RegistryCrate,
        timestamp: Option<&SystemTime>,
    ) {
        let timestamp = timestamp.map_or(self.now, to_timestamp);
        let index = RegistryIndex {
            encoded_registry_name: registry_crate.encoded_registry_name,
        };
        self.registry_index_timestamps.insert(index, timestamp);
        self.registry_crate_timestamps
            .insert(registry_crate, timestamp);
    }

    /// Indicates the given [`RegistrySrc`] has been used with the given
    /// time (or "now" if `None`).
    ///
    /// Also implicitly marks the index used, too.
    pub fn mark_registry_src_used_stamp(
        &mut self,
        registry_src: RegistrySrc,
        timestamp: Option<&SystemTime>,
    ) {
        let timestamp = timestamp.map_or(self.now, to_timestamp);
        let index = RegistryIndex {
            encoded_registry_name: registry_src.encoded_registry_name,
        };
        self.registry_index_timestamps.insert(index, timestamp);
        self.registry_src_timestamps.insert(registry_src, timestamp);
    }

    /// Indicates the given [`GitCheckout`] has been used with the given
    /// time (or "now" if `None`).
    ///
    /// Also implicitly marks the git db used, too.
    pub fn mark_git_checkout_used_stamp(
        &mut self,
        git_checkout: GitCheckout,
        timestamp: Option<&SystemTime>,
    ) {
        let timestamp = timestamp.map_or(self.now, to_timestamp);
        let db = GitDb {
            encoded_git_name: git_checkout.encoded_git_name,
        };
        self.git_db_timestamps.insert(db, timestamp);
        self.git_checkout_timestamps.insert(git_checkout, timestamp);
    }

    /// Saves all of the deferred information to the database.
    ///
    /// This will also clear the state of `self`.
    #[tracing::instrument(skip_all)]
    pub fn save(&mut self, tracker: &mut GlobalCacheTracker) -> CargoResult<()> {
        trace!(target: "gc", "saving last-use data");
        if self.is_empty() {
            return Ok(());
        }
        tracker.backend.save_deferred(self)?;
        trace!(target: "gc", "last-use save complete");
        Ok(())
    }

    /// Variant of [`DeferredGlobalLastUse::save`] that does not return an
    /// error.
    ///
    /// This will log or display a warning to the user.
    pub fn save_no_error(&mut self, gctx: &GlobalContext) {
        if let Err(e) = self.save_with_gctx(gctx) {
            self.clear();
            if !self.save_err_has_warned {
                if is_silent_error(&e) && gctx.shell().verbosity() != Verbosity::Verbose {
                    tracing::warn!("failed to save last-use data: {e:?}");
                } else {
                    crate::display_warning_with_error(
                        "failed to save last-use data\n\
                        This may prevent cargo from accurately tracking what is being \
                        used in its global cache. This information is used for \
                        automatically removing unused data in the cache.",
                        &e,
                        &mut gctx.shell(),
                    );
                    self.save_err_has_warned = true;
                }
            }
        }
    }

    fn save_with_gctx(&mut self, gctx: &GlobalContext) -> CargoResult<()> {
        let mut tracker = gctx.global_cache_tracker()?;
        self.save(&mut tracker)
    }
}

/// Converts a [`SystemTime`] to a [`Timestamp`].
fn to_timestamp(t: &SystemTime) -> Timestamp {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .expect("invalid clock")
        .as_secs()
}

/// Returns the current time.
#[expect(
    clippy::disallowed_methods,
    reason = "testing only, no reason for config support"
)]
pub(crate) fn now() -> Timestamp {
    match std::env::var("__CARGO_TEST_LAST_USE_NOW") {
        Ok(now) => now.parse().unwrap(),
        Err(_) => to_timestamp(&SystemTime::now()),
    }
}

/// Returns whether or not the given error should cause a warning to be
/// displayed to the user.
pub fn is_silent_error(e: &anyhow::Error) -> bool {
    BackendImpl::is_silent_error(e)
}

/// Returns a list of directory entries that are themselves directories.
fn list_dir_names(path: &Path) -> CargoResult<Vec<String>> {
    read_dir_with_filter(path, &|entry| {
        entry.file_type().map_or(false, |ty| ty.is_dir())
    })
}

/// Returns a list of names in a directory, filtered by the given callback.
fn read_dir_with_filter(
    path: &Path,
    filter: &dyn Fn(&std::fs::DirEntry) -> bool,
) -> CargoResult<Vec<String>> {
    let entries = match path.read_dir() {
        Ok(e) => e,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(Vec::new());
            } else {
                return Err(
                    anyhow::Error::new(e).context(format!("failed to read path `{path:?}`"))
                );
            }
        }
    };
    let names = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| filter(entry))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    Ok(names)
}

/// Returns the disk usage for a git checkout directory.
#[tracing::instrument]
fn du_git_checkout(path: &Path) -> CargoResult<u64> {
    cargo_util::du(&path, &["!.git"])
}

fn du(path: &Path, table_name: &str) -> CargoResult<u64> {
    if table_name == "git_checkout" {
        du_git_checkout(path)
    } else {
        cargo_util::du(&path, &[])
    }
}
