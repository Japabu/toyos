//! Stub download backend when curl is not available.
//!
//! Handles locally-available packages (path deps, cached crates) but bails
//! if an actual network download is required.

use crate::core::package::PackageSet;
use crate::core::{Package, PackageId};
use crate::sources::source::MaybePackage;
use crate::util::cache_lock::{CacheLock, CacheLockMode};
use crate::util::errors::CargoResult;
use crate::util::{internal, GlobalContext};
use anyhow::Context as _;

/// Backend state for downloads. No-op without curl.
pub struct DownloadState;

impl DownloadState {
    pub fn new(_gctx: &GlobalContext) -> CargoResult<Self> {
        Ok(DownloadState)
    }
}

/// Download session. Without curl, only serves already-available packages.
pub struct Downloads<'a, 'gctx> {
    set: &'a PackageSet<'gctx>,
    pub success: bool,
    _lock: CacheLock<'gctx>,
}

impl<'a, 'gctx> Downloads<'a, 'gctx> {
    pub fn new(set: &'a PackageSet<'gctx>) -> CargoResult<Self> {
        Ok(Downloads {
            set,
            success: false,
            _lock: set
                .gctx
                .acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?,
        })
    }

    /// Starts to download the package for the `id` specified.
    ///
    /// Without curl, this only serves cached/local packages and bails if
    /// a network download would be required.
    pub fn start(&mut self, id: PackageId) -> CargoResult<Option<&'a Package>> {
        self.start_inner(id)
            .with_context(|| format!("failed to download `{}`", id))
    }

    fn start_inner(&mut self, id: PackageId) -> CargoResult<Option<&'a Package>> {
        let slot = self
            .set
            .packages
            .get(&id)
            .ok_or_else(|| internal(format!("couldn't find `{}` in package set", id)))?;
        if let Some(pkg) = slot.get() {
            return Ok(Some(pkg));
        }

        let mut sources = self.set.sources.borrow_mut();
        let source = sources
            .get_mut(id.source_id())
            .ok_or_else(|| internal(format!("couldn't find source for `{}`", id)))?;
        let pkg = source
            .download(id)
            .context("unable to get packages from source")?;
        match pkg {
            MaybePackage::Ready(pkg) => {
                assert!(slot.set(pkg).is_ok());
                Ok(Some(slot.get().unwrap()))
            }
            MaybePackage::Download { url, .. } => {
                anyhow::bail!(
                    "package `{}` requires downloading from `{}`, \
                     but the curl-backend feature is not enabled",
                    id,
                    url
                );
            }
        }
    }

    pub fn remaining(&self) -> usize {
        0
    }

    pub fn wait(&mut self) -> CargoResult<&'a Package> {
        anyhow::bail!("package downloading requires the curl-backend feature")
    }
}

impl Drop for Downloads<'_, '_> {
    fn drop(&mut self) {
        self.set.downloading.set(false);
    }
}
