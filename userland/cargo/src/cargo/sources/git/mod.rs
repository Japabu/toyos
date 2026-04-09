//! Home of the [`GitSource`].
//!
//! Apparently, the most important type in this module is [`GitSource`].
//! [`utils`] provides libgit2 utilities like fetch and checkout, whereas
//! [`oxide`] is the counterpart for gitoxide integration. [`known_hosts`]
//! is the mitigation of [CVE-2022-46176].
//!
//! [CVE-2022-46176]: https://blog.rust-lang.org/2023/01/10/cve-2022-46176.html

// Backend-agnostic git utilities (init, discover, config, version, etc.)
// Callers use these instead of git2/gix directly.
pub mod backend;

// git2-based full implementation (clone, checkout, fetch, auth, submodules)
#[cfg(feature = "git2-backend")]
mod known_hosts;
#[cfg(feature = "git2-backend")]
mod source;
#[cfg(feature = "git2-backend")]
pub(crate) mod utils;

pub(crate) mod oxide;

// Re-export core types. With git2-backend, these come from the real
// implementation. Without it, they come from the backend module stubs.
#[cfg(feature = "git2-backend")]
pub use self::source::GitSource;
#[cfg(feature = "git2-backend")]
pub use self::utils::{GitCheckout, GitDatabase, GitRemote, GitShortID, fetch, resolve_ref};

#[cfg(not(feature = "git2-backend"))]
pub use self::backend::{GitSource, GitCheckout, GitDatabase, GitRemote, GitShortID, fetch, resolve_ref};

/// For `-Zgitoxide` integration.
pub mod fetch {
    use crate::GlobalContext;
    use crate::core::features::GitFeatures;

    /// The kind remote repository to fetch.
    #[derive(Debug, Copy, Clone)]
    pub enum RemoteKind {
        /// A repository belongs to a git dependency.
        GitDependency,
        /// A repository belongs to a Cargo registry.
        Registry,
    }

    impl RemoteKind {
        /// Obtain the kind of history we would want for a fetch from our remote
        /// knowing if the target repo is already shallow via `repo_is_shallow`.
        pub(crate) fn to_shallow_setting(
            &self,
            repo_is_shallow: bool,
            gctx: &GlobalContext,
        ) -> gix::remote::fetch::Shallow {
            let has_feature = |cb: &dyn Fn(GitFeatures) -> bool| {
                gctx.cli_unstable()
                    .git
                    .map_or(false, |features| cb(features))
            };

            if !repo_is_shallow {
                match self {
                    RemoteKind::GitDependency if has_feature(&|features| features.shallow_deps) => {
                    }
                    RemoteKind::Registry if has_feature(&|features| features.shallow_index) => {}
                    _ => return gix::remote::fetch::Shallow::NoChange,
                }
            };

            gix::remote::fetch::Shallow::DepthAtRemote(1.try_into().expect("non-zero"))
        }
    }

    pub type Error = gix::env::collate::fetch::Error<gix::refspec::parse::Error>;
}
