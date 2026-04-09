//! JSON flat-file storage backend for the global cache tracker.
//!
//! This backend stores cache tracking data as a JSON file, providing the same
//! functionality as the SQLite backend without requiring any C dependencies.

use super::*;
use crate::CargoResult;
use crate::util::interning::InternedString;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const GLOBAL_CACHE_FILENAME: &str = ".global-cache-tracker";

/// On-disk format for the cache tracking data.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheData {
    /// Registry index entries: name → timestamp.
    registry_index: HashMap<String, Timestamp>,
    /// Registry crate entries: registry_name → { crate_name → (size, timestamp) }.
    registry_crate: HashMap<String, HashMap<String, (u64, Timestamp)>>,
    /// Registry src entries: registry_name → { src_name → (size_or_null, timestamp) }.
    registry_src: HashMap<String, HashMap<String, (Option<u64>, Timestamp)>>,
    /// Git db entries: name → timestamp.
    git_db: HashMap<String, Timestamp>,
    /// Git checkout entries: git_db_name → { checkout_name → (size_or_null, timestamp) }.
    git_checkout: HashMap<String, HashMap<String, (Option<u64>, Timestamp)>>,
    /// Last time automatic GC was run.
    last_auto_gc: Timestamp,
}

#[derive(Debug)]
pub(super) struct FlatFileBackend {
    data: CacheData,
    /// Snapshot for rollback support.
    snapshot: Option<Vec<u8>>,
    path: PathBuf,
}

impl FlatFileBackend {
    pub const FILENAME: &'static str = GLOBAL_CACHE_FILENAME;

    pub fn open(path: &Path) -> CargoResult<FlatFileBackend> {
        let data = if path.exists() {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read cache tracker at `{}`", path.display()))?;
            serde_json::from_str(&contents)
                .with_context(|| format!("failed to parse cache tracker at `{}`", path.display()))?
        } else {
            CacheData {
                last_auto_gc: now(),
                ..Default::default()
            }
        };
        Ok(FlatFileBackend {
            data,
            snapshot: None,
            path: path.to_path_buf(),
        })
    }

    fn flush(&self) -> CargoResult<()> {
        let contents = serde_json::to_string_pretty(&self.data)?;
        // Atomic write: write to temp file then rename.
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(".global-cache-tracker.tmp");
        std::fs::write(&tmp, contents.as_bytes())
            .with_context(|| format!("failed to write cache tracker to `{}`", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to rename cache tracker to `{}`", self.path.display()))?;
        Ok(())
    }

    // --- Query methods ---

    pub fn registry_index_all(&self) -> CargoResult<Vec<(RegistryIndex, Timestamp)>> {
        Ok(self
            .data
            .registry_index
            .iter()
            .map(|(name, &ts)| {
                (
                    RegistryIndex {
                        encoded_registry_name: InternedString::from(name.as_str()),
                    },
                    ts,
                )
            })
            .collect())
    }

    pub fn registry_crate_all(&self) -> CargoResult<Vec<(RegistryCrate, Timestamp)>> {
        let mut result = Vec::new();
        for (registry, crates) in &self.data.registry_crate {
            for (name, &(size, ts)) in crates {
                result.push((
                    RegistryCrate {
                        encoded_registry_name: InternedString::from(registry.as_str()),
                        crate_filename: InternedString::from(name.as_str()),
                        size,
                    },
                    ts,
                ));
            }
        }
        Ok(result)
    }

    pub fn registry_src_all(&self) -> CargoResult<Vec<(RegistrySrc, Timestamp)>> {
        let mut result = Vec::new();
        for (registry, srcs) in &self.data.registry_src {
            for (name, &(size, ts)) in srcs {
                result.push((
                    RegistrySrc {
                        encoded_registry_name: InternedString::from(registry.as_str()),
                        package_dir: InternedString::from(name.as_str()),
                        size,
                    },
                    ts,
                ));
            }
        }
        Ok(result)
    }

    pub fn git_db_all(&self) -> CargoResult<Vec<(GitDb, Timestamp)>> {
        Ok(self
            .data
            .git_db
            .iter()
            .map(|(name, &ts)| {
                (
                    GitDb {
                        encoded_git_name: InternedString::from(name.as_str()),
                    },
                    ts,
                )
            })
            .collect())
    }

    pub fn git_checkout_all(&self) -> CargoResult<Vec<(GitCheckout, Timestamp)>> {
        let mut result = Vec::new();
        for (db, checkouts) in &self.data.git_checkout {
            for (name, &(size, ts)) in checkouts {
                result.push((
                    GitCheckout {
                        encoded_git_name: InternedString::from(db.as_str()),
                        short_name: InternedString::from(name.as_str()),
                        size,
                    },
                    ts,
                ));
            }
        }
        Ok(result)
    }

    // --- Metadata ---

    pub fn last_auto_gc(&self) -> CargoResult<Timestamp> {
        Ok(self.data.last_auto_gc)
    }

    pub fn set_last_auto_gc(&mut self, timestamp: Timestamp) -> CargoResult<()> {
        self.data.last_auto_gc = timestamp;
        self.flush()
    }

    // --- Insert if missing ---

    pub fn insert_registry_index_if_missing(
        &mut self,
        name: &str,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.data
            .registry_index
            .entry(name.to_string())
            .or_insert(timestamp);
        Ok(())
    }

    pub fn insert_registry_crate_if_missing(
        &mut self,
        registry: &str,
        name: &str,
        size: u64,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.data
            .registry_crate
            .entry(registry.to_string())
            .or_default()
            .entry(name.to_string())
            .or_insert((size, timestamp));
        Ok(())
    }

    pub fn insert_registry_src_if_missing(
        &mut self,
        registry: &str,
        name: &str,
        size: Option<u64>,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.data
            .registry_src
            .entry(registry.to_string())
            .or_default()
            .entry(name.to_string())
            .or_insert((size, timestamp));
        Ok(())
    }

    pub fn insert_git_db_if_missing(
        &mut self,
        name: &str,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.data.git_db.entry(name.to_string()).or_insert(timestamp);
        Ok(())
    }

    pub fn insert_git_checkout_if_missing(
        &mut self,
        git_db: &str,
        name: &str,
        size: Option<u64>,
        timestamp: Timestamp,
    ) -> CargoResult<()> {
        self.data
            .git_checkout
            .entry(git_db.to_string())
            .or_default()
            .entry(name.to_string())
            .or_insert((size, timestamp));
        Ok(())
    }

    // --- Delete ---

    pub fn delete_registry_index(&mut self, name: &str) -> CargoResult<()> {
        self.data.registry_index.remove(name);
        // Cascade: remove child entries.
        self.data.registry_crate.remove(name);
        self.data.registry_src.remove(name);
        Ok(())
    }

    pub fn delete_registry_crate(&mut self, registry: &str, name: &str) -> CargoResult<()> {
        if let Some(crates) = self.data.registry_crate.get_mut(registry) {
            crates.remove(name);
        }
        Ok(())
    }

    pub fn delete_registry_src(&mut self, registry: &str, name: &str) -> CargoResult<()> {
        if let Some(srcs) = self.data.registry_src.get_mut(registry) {
            srcs.remove(name);
        }
        Ok(())
    }

    pub fn delete_git_db(&mut self, name: &str) -> CargoResult<()> {
        self.data.git_db.remove(name);
        // Cascade: remove child entries.
        self.data.git_checkout.remove(name);
        Ok(())
    }

    pub fn delete_git_checkout(&mut self, git_db: &str, name: &str) -> CargoResult<()> {
        if let Some(checkouts) = self.data.git_checkout.get_mut(git_db) {
            checkouts.remove(name);
        }
        Ok(())
    }

    // --- Existence checks ---

    pub fn registry_src_exists(&self, registry: &str, name: &str) -> CargoResult<bool> {
        Ok(self
            .data
            .registry_src
            .get(registry)
            .is_some_and(|srcs| srcs.contains_key(name)))
    }

    pub fn git_checkout_exists(&self, git_db: &str, name: &str) -> CargoResult<bool> {
        Ok(self
            .data
            .git_checkout
            .get(git_db)
            .is_some_and(|cos| cos.contains_key(name)))
    }

    // --- Size updates ---

    pub fn update_registry_src_size(
        &mut self,
        registry: &str,
        name: &str,
        size: u64,
    ) -> CargoResult<()> {
        if let Some(srcs) = self.data.registry_src.get_mut(registry) {
            if let Some(entry) = srcs.get_mut(name) {
                entry.0 = Some(size);
            }
        }
        Ok(())
    }

    pub fn update_git_checkout_size(
        &mut self,
        git_db: &str,
        name: &str,
        size: u64,
    ) -> CargoResult<()> {
        if let Some(cos) = self.data.git_checkout.get_mut(git_db) {
            if let Some(entry) = cos.get_mut(name) {
                entry.0 = Some(size);
            }
        }
        Ok(())
    }

    // --- Transaction control ---

    pub fn begin(&mut self) -> CargoResult<()> {
        // Take a snapshot for rollback.
        self.snapshot = Some(serde_json::to_vec(&self.data)?);
        Ok(())
    }

    pub fn commit(&mut self) -> CargoResult<()> {
        self.snapshot = None;
        self.flush()
    }

    pub fn rollback(&mut self) -> CargoResult<()> {
        if let Some(snapshot) = self.snapshot.take() {
            self.data = serde_json::from_slice(&snapshot)?;
        }
        Ok(())
    }

    // --- Batch save from DeferredGlobalLastUse ---

    pub fn save_deferred(&mut self, deferred: &mut DeferredGlobalLastUse) -> CargoResult<()> {
        // Registry index timestamps.
        for (index, new_ts) in std::mem::take(&mut deferred.registry_index_timestamps) {
            let name = index.encoded_registry_name.to_string();
            let entry = self.data.registry_index.entry(name).or_insert(0);
            if *entry < new_ts - UPDATE_RESOLUTION {
                *entry = new_ts;
            }
        }

        // Git db timestamps.
        for (db, new_ts) in std::mem::take(&mut deferred.git_db_timestamps) {
            let name = db.encoded_git_name.to_string();
            let entry = self.data.git_db.entry(name).or_insert(0);
            if *entry < new_ts - UPDATE_RESOLUTION {
                *entry = new_ts;
            }
        }

        // Registry crate timestamps.
        for (krate, new_ts) in std::mem::take(&mut deferred.registry_crate_timestamps) {
            let registry = krate.encoded_registry_name.to_string();
            let crate_name = krate.crate_filename.to_string();
            let crates = self.data.registry_crate.entry(registry).or_default();
            let entry = crates.entry(crate_name).or_insert((krate.size, 0));
            if entry.1 < new_ts - UPDATE_RESOLUTION {
                entry.1 = new_ts;
            }
        }

        // Registry src timestamps.
        for (src, new_ts) in std::mem::take(&mut deferred.registry_src_timestamps) {
            let registry = src.encoded_registry_name.to_string();
            let src_name = src.package_dir.to_string();
            let srcs = self.data.registry_src.entry(registry).or_default();
            let entry = srcs.entry(src_name).or_insert((src.size, 0));
            if entry.1 < new_ts - UPDATE_RESOLUTION {
                entry.1 = new_ts;
            }
        }

        // Git checkout timestamps.
        for (co, new_ts) in std::mem::take(&mut deferred.git_checkout_timestamps) {
            let db = co.encoded_git_name.to_string();
            let co_name = co.short_name.to_string();
            let checkouts = self.data.git_checkout.entry(db).or_default();
            let entry = checkouts.entry(co_name).or_insert((co.size, 0));
            if entry.1 < new_ts - UPDATE_RESOLUTION {
                entry.1 = new_ts;
            }
        }

        self.flush()
    }

    // --- Error classification ---

    pub fn is_silent_error(e: &anyhow::Error) -> bool {
        if let Some(e) = e.downcast_ref::<std::io::Error>() {
            return matches!(
                e.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            );
        }
        false
    }
}
