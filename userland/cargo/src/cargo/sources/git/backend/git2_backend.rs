//! Git backend implementation using libgit2 (C library).

use std::path::Path;

use crate::util::errors::CargoResult;

/// Initialize a new git repository at the given path.
pub fn init_repo(path: &Path) -> CargoResult<()> {
    git2::Repository::init(path)?;
    Ok(())
}

/// Discover a git repository starting from the given path, searching upward.
/// Returns a handle that can be used with other backend functions.
pub fn discover_repo(path: &Path) -> Result<Repository, anyhow::Error> {
    Ok(Repository(git2::Repository::discover(path)?))
}

/// Opaque repository handle.
pub struct Repository(git2::Repository);

impl Repository {
    pub fn workdir(&self) -> Option<&Path> {
        self.0.workdir()
    }

    pub fn is_path_ignored(&self, path: &Path) -> bool {
        self.0.is_path_ignored(path).unwrap_or(false)
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
    if let Ok(repo) = git2::Repository::discover(path) {
        let mut opts = git2::StatusOptions::new();
        opts.include_ignored(false);
        opts.include_untracked(true);
        for status in repo.statuses(Some(&mut opts))?.iter() {
            if let Some(file_path) = status.path() {
                let s = match status.status() {
                    git2::Status::CURRENT => FileStatus::Current,
                    git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE => {
                        FileStatus::Staged(file_path.to_string())
                    }
                    _ => FileStatus::Dirty(file_path.to_string()),
                };
                result.push(s);
            }
        }
    }
    Ok(result)
}

/// Look up a string value from the global git configuration.
pub fn git_config_string(key: &str) -> Option<String> {
    git2::Config::open_default()
        .and_then(|cfg| cfg.get_string(key))
        .ok()
}

/// Reinitialize a git repository (used for recovery from corruption).
pub fn reinitialize_repo(path: &Path, bare: bool) -> CargoResult<()> {
    let mut opts = git2::RepositoryInitOptions::new();
    opts.external_template(false);
    opts.bare(bare);
    git2::Repository::init_opts(path, &opts)?;
    Ok(())
}

/// Check if an error from a git operation is spurious (transient/retryable).
/// Returns `Some(true)` if spurious, `Some(false)` if definitely not, `None` if not a git error.
pub fn is_spurious_git_error(err: &anyhow::Error) -> Option<bool> {
    let git_err = err.downcast_ref::<git2::Error>()?;
    match git_err.class() {
        git2::ErrorClass::Net
        | git2::ErrorClass::Os
        | git2::ErrorClass::Zlib
        | git2::ErrorClass::Http => Some(git_err.code() != git2::ErrorCode::Certificate),
        _ => Some(false),
    }
}

/// Run global git initialization (e.g., disabling owner validation for libgit2).
pub fn init_git_global() {
    unsafe {
        git2::opts::set_verify_owner_validation(false)
            .expect("set_verify_owner_validation should never fail");
    }
}

/// Get version information about the git backend for display.
pub fn version_info() -> String {
    let git2_v = git2::Version::get();
    let lib_v = git2_v.libgit2_version();
    let vendored = if git2_v.vendored() {
        "vendored"
    } else {
        "system"
    };
    format!(
        "libgit2: {}.{}.{} (sys:{} {})",
        lib_v.0,
        lib_v.1,
        lib_v.2,
        git2_v.crate_version(),
        vendored
    )
}
