use std::cell::OnceCell;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cargo_util_schemas::manifest::{Hints, RustVersion};
use semver::Version;
use serde::Serialize;

use crate::core::compiler::{CompileKind, RustcTargetData};
use crate::core::dependency::DepKind;
use crate::core::package_downloads::{self, Downloads};
use crate::core::resolver::features::ForceAllTargets;
use crate::core::resolver::{HasDevUnits, Resolve};
use crate::core::{
    CliUnstable, Dependency, Features, Manifest, PackageId, PackageIdSpec, SerializedDependency,
    SourceId, Target,
};
use crate::core::{Summary, Workspace};
use crate::sources::source::SourceMap;
use crate::util::cache_lock::CacheLockMode;
use crate::util::errors::CargoResult;
use crate::util::interning::InternedString;
use crate::util::GlobalContext;

/// Information about a package that is available somewhere in the file system.
///
/// A package is a `Cargo.toml` file plus all the files that are part of it.
#[derive(Clone)]
pub struct Package {
    inner: Rc<PackageInner>,
}

#[derive(Clone)]
// TODO: is `manifest_path` a relic?
struct PackageInner {
    /// The package's manifest.
    manifest: Manifest,
    /// The root of the package.
    manifest_path: PathBuf,
}

impl Ord for Package {
    fn cmp(&self, other: &Package) -> Ordering {
        self.package_id().cmp(&other.package_id())
    }
}

impl PartialOrd for Package {
    fn partial_cmp(&self, other: &Package) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A Package in a form where `Serialize` can be derived.
#[derive(Serialize)]
pub struct SerializedPackage {
    name: InternedString,
    version: Version,
    id: PackageIdSpec,
    license: Option<String>,
    license_file: Option<String>,
    description: Option<String>,
    source: SourceId,
    dependencies: Vec<SerializedDependency>,
    targets: Vec<Target>,
    features: BTreeMap<InternedString, Vec<InternedString>>,
    manifest_path: PathBuf,
    metadata: Option<toml::Value>,
    publish: Option<Vec<String>>,
    authors: Vec<String>,
    categories: Vec<String>,
    keywords: Vec<String>,
    readme: Option<String>,
    repository: Option<String>,
    homepage: Option<String>,
    documentation: Option<String>,
    edition: String,
    links: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metabuild: Option<Vec<String>>,
    default_run: Option<String>,
    rust_version: Option<RustVersion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hints: Option<Hints>,
}

impl Package {
    /// Creates a package from a manifest and its location.
    pub fn new(manifest: Manifest, manifest_path: &Path) -> Package {
        Package {
            inner: Rc::new(PackageInner {
                manifest,
                manifest_path: manifest_path.to_path_buf(),
            }),
        }
    }

    /// Gets the manifest dependencies.
    pub fn dependencies(&self) -> &[Dependency] {
        self.manifest().dependencies()
    }
    /// Gets the manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.inner.manifest
    }
    /// Gets the manifest.
    pub fn manifest_mut(&mut self) -> &mut Manifest {
        &mut Rc::make_mut(&mut self.inner).manifest
    }
    /// Gets the path to the manifest.
    pub fn manifest_path(&self) -> &Path {
        &self.inner.manifest_path
    }
    /// Gets the name of the package.
    pub fn name(&self) -> InternedString {
        self.package_id().name()
    }
    /// Gets the `PackageId` object for the package (fully defines a package).
    pub fn package_id(&self) -> PackageId {
        self.manifest().package_id()
    }
    /// Gets the root folder of the package.
    pub fn root(&self) -> &Path {
        self.manifest_path().parent().unwrap()
    }
    /// Gets the summary for the package.
    pub fn summary(&self) -> &Summary {
        self.manifest().summary()
    }
    /// Gets the targets specified in the manifest.
    pub fn targets(&self) -> &[Target] {
        self.manifest().targets()
    }
    /// Gets the library crate for this package, if it exists.
    pub fn library(&self) -> Option<&Target> {
        self.targets().iter().find(|t| t.is_lib())
    }
    /// Gets the current package version.
    pub fn version(&self) -> &Version {
        self.package_id().version()
    }
    /// Gets the package authors.
    pub fn authors(&self) -> &Vec<String> {
        &self.manifest().metadata().authors
    }

    /// Returns `None` if the package is set to publish.
    /// Returns `Some(allowed_registries)` if publishing is limited to specified
    /// registries or if package is set to not publish.
    pub fn publish(&self) -> &Option<Vec<String>> {
        self.manifest().publish()
    }
    /// Returns `true` if this package is a proc-macro.
    pub fn proc_macro(&self) -> bool {
        self.targets().iter().any(|target| target.proc_macro())
    }
    /// Gets the package's minimum Rust version.
    pub fn rust_version(&self) -> Option<&RustVersion> {
        self.manifest().rust_version()
    }

    /// Gets the package's hints.
    pub fn hints(&self) -> Option<&Hints> {
        self.manifest().hints()
    }

    /// Returns `true` if the package uses a custom build script for any target.
    pub fn has_custom_build(&self) -> bool {
        self.targets().iter().any(|t| t.is_custom_build())
    }

    pub fn map_source(self, to_replace: SourceId, replace_with: SourceId) -> Package {
        Package {
            inner: Rc::new(PackageInner {
                manifest: self.manifest().clone().map_source(to_replace, replace_with),
                manifest_path: self.manifest_path().to_owned(),
            }),
        }
    }

    pub fn serialized(
        &self,
        unstable_flags: &CliUnstable,
        cargo_features: &Features,
    ) -> SerializedPackage {
        let summary = self.manifest().summary();
        let package_id = summary.package_id();
        let manmeta = self.manifest().metadata();
        // Filter out metabuild targets. They are an internal implementation
        // detail that is probably not relevant externally. There's also not a
        // real path to show in `src_path`, and this avoids changing the format.
        let targets: Vec<Target> = self
            .manifest()
            .targets()
            .iter()
            .filter(|t| t.src_path().is_path())
            .cloned()
            .collect();
        // Convert Vec<FeatureValue> to Vec<InternedString>
        let crate_features = summary
            .features()
            .iter()
            .map(|(k, v)| (*k, v.iter().map(|fv| fv.to_string().into()).collect()))
            .collect();

        SerializedPackage {
            name: package_id.name(),
            version: package_id.version().clone(),
            id: package_id.to_spec(),
            license: manmeta.license.clone(),
            license_file: manmeta.license_file.clone(),
            description: manmeta.description.clone(),
            source: summary.source_id(),
            dependencies: summary
                .dependencies()
                .iter()
                .map(|dep| dep.serialized(unstable_flags, cargo_features))
                .collect(),
            targets,
            features: crate_features,
            manifest_path: self.manifest_path().to_path_buf(),
            metadata: self.manifest().custom_metadata().cloned(),
            authors: manmeta.authors.clone(),
            categories: manmeta.categories.clone(),
            keywords: manmeta.keywords.clone(),
            readme: manmeta.readme.clone(),
            repository: manmeta.repository.clone(),
            homepage: manmeta.homepage.clone(),
            documentation: manmeta.documentation.clone(),
            edition: self.manifest().edition().to_string(),
            links: self.manifest().links().map(|s| s.to_owned()),
            metabuild: self.manifest().metabuild().cloned(),
            publish: self.publish().as_ref().cloned(),
            default_run: self.manifest().default_run().map(|s| s.to_owned()),
            rust_version: self.rust_version().cloned(),
            hints: self.hints().cloned(),
        }
    }
}

impl fmt::Display for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary().package_id())
    }
}

impl fmt::Debug for Package {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Package")
            .field("id", &self.summary().package_id())
            .field("..", &"..")
            .finish()
    }
}

impl PartialEq for Package {
    fn eq(&self, other: &Package) -> bool {
        self.package_id() == other.package_id()
    }
}

impl Eq for Package {}

impl hash::Hash for Package {
    fn hash<H: hash::Hasher>(&self, into: &mut H) {
        self.package_id().hash(into)
    }
}

/// A set of packages, with the intent to download.
///
/// This is primarily used to convert a set of `PackageId`s to `Package`s. It
/// will download as needed, or used the cached download if available.
pub struct PackageSet<'gctx> {
    pub(crate) packages: HashMap<PackageId, OnceCell<Package>>,
    pub(crate) sources: RefCell<SourceMap<'gctx>>,
    pub(crate) gctx: &'gctx GlobalContext,
    pub(crate) dl_state: package_downloads::DownloadState,
    /// Used to prevent reusing the `PackageSet` to download twice.
    pub(crate) downloading: Cell<bool>,
}

impl<'gctx> PackageSet<'gctx> {
    pub fn new(
        package_ids: &[PackageId],
        sources: SourceMap<'gctx>,
        gctx: &'gctx GlobalContext,
    ) -> CargoResult<PackageSet<'gctx>> {
        Ok(PackageSet {
            packages: package_ids
                .iter()
                .map(|&id| (id, OnceCell::new()))
                .collect(),
            sources: RefCell::new(sources),
            gctx,
            dl_state: package_downloads::DownloadState::new(gctx)?,
            downloading: Cell::new(false),
        })
    }

    pub fn package_ids(&self) -> impl Iterator<Item = PackageId> + '_ {
        self.packages.keys().cloned()
    }

    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.values().filter_map(|p| p.get())
    }

    pub fn enable_download<'a>(&'a self) -> CargoResult<Downloads<'a, 'gctx>> {
        assert!(!self.downloading.replace(true));
        Downloads::new(self)
    }

    pub fn get_one(&self, id: PackageId) -> CargoResult<&Package> {
        if let Some(pkg) = self.packages.get(&id).and_then(|slot| slot.get()) {
            return Ok(pkg);
        }
        Ok(self.get_many(Some(id))?.remove(0))
    }

    pub fn get_many(&self, ids: impl IntoIterator<Item = PackageId>) -> CargoResult<Vec<&Package>> {
        let mut pkgs = Vec::new();
        let _lock = self
            .gctx
            .acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
        let mut downloads = self.enable_download()?;
        for id in ids {
            pkgs.extend(downloads.start(id)?);
        }
        while downloads.remaining() > 0 {
            pkgs.push(downloads.wait()?);
        }
        downloads.success = true;
        drop(downloads);

        let mut deferred = self.gctx.deferred_global_last_use()?;
        deferred.save_no_error(self.gctx);
        Ok(pkgs)
    }

    /// Downloads any packages accessible from the give root ids.
    #[tracing::instrument(skip_all)]
    pub fn download_accessible(
        &self,
        resolve: &Resolve,
        root_ids: &[PackageId],
        has_dev_units: HasDevUnits,
        requested_kinds: &[CompileKind],
        target_data: &RustcTargetData<'gctx>,
        force_all_targets: ForceAllTargets,
    ) -> CargoResult<()> {
        fn collect_used_deps(
            used: &mut BTreeSet<(PackageId, CompileKind)>,
            resolve: &Resolve,
            pkg_id: PackageId,
            has_dev_units: HasDevUnits,
            requested_kind: CompileKind,
            target_data: &RustcTargetData<'_>,
            force_all_targets: ForceAllTargets,
        ) -> CargoResult<()> {
            if !used.insert((pkg_id, requested_kind)) {
                return Ok(());
            }
            let requested_kinds = &[requested_kind];
            let filtered_deps = PackageSet::filter_deps(
                pkg_id,
                resolve,
                has_dev_units,
                requested_kinds,
                target_data,
                force_all_targets,
            );
            for (pkg_id, deps) in filtered_deps {
                collect_used_deps(
                    used,
                    resolve,
                    pkg_id,
                    has_dev_units,
                    requested_kind,
                    target_data,
                    force_all_targets,
                )?;
                let artifact_kinds = deps.iter().filter_map(|dep| {
                    Some(
                        dep.artifact()?
                            .target()?
                            .to_resolved_compile_kind(*requested_kinds.iter().next().unwrap()),
                    )
                });
                for artifact_kind in artifact_kinds {
                    collect_used_deps(
                        used,
                        resolve,
                        pkg_id,
                        has_dev_units,
                        artifact_kind,
                        target_data,
                        force_all_targets,
                    )?;
                }
            }
            Ok(())
        }

        // This is sorted by PackageId to get consistent behavior and error
        // messages for Cargo's testsuite. Perhaps there is a better ordering
        // that optimizes download time?
        let mut to_download = BTreeSet::new();

        for id in root_ids {
            for requested_kind in requested_kinds {
                collect_used_deps(
                    &mut to_download,
                    resolve,
                    *id,
                    has_dev_units,
                    *requested_kind,
                    target_data,
                    force_all_targets,
                )?;
            }
        }
        let to_download = to_download
            .into_iter()
            .map(|(p, _)| p)
            .collect::<BTreeSet<_>>();
        self.get_many(to_download.into_iter())?;
        Ok(())
    }

    /// Check if there are any dependency packages that violate artifact constraints
    /// to instantly abort, or that do not have any libs which results in warnings.
    pub(crate) fn warn_no_lib_packages_and_artifact_libs_overlapping_deps(
        &self,
        ws: &Workspace<'gctx>,
        resolve: &Resolve,
        root_ids: &[PackageId],
        has_dev_units: HasDevUnits,
        requested_kinds: &[CompileKind],
        target_data: &RustcTargetData<'_>,
        force_all_targets: ForceAllTargets,
    ) -> CargoResult<()> {
        let no_lib_pkgs: BTreeMap<PackageId, Vec<(&Package, &HashSet<Dependency>)>> = root_ids
            .iter()
            .map(|&root_id| {
                let dep_pkgs_to_deps: Vec<_> = PackageSet::filter_deps(
                    root_id,
                    resolve,
                    has_dev_units,
                    requested_kinds,
                    target_data,
                    force_all_targets,
                )
                .collect();

                let dep_pkgs_and_deps = dep_pkgs_to_deps
                    .into_iter()
                    .filter(|(_id, deps)| deps.iter().any(|dep| dep.maybe_lib()))
                    .filter_map(|(dep_package_id, deps)| {
                        self.get_one(dep_package_id).ok().and_then(|dep_pkg| {
                            (!dep_pkg.targets().iter().any(|t| t.is_lib())).then(|| (dep_pkg, deps))
                        })
                    })
                    .collect();
                (root_id, dep_pkgs_and_deps)
            })
            .collect();

        for (pkg_id, dep_pkgs) in no_lib_pkgs {
            for (_dep_pkg_without_lib_target, deps) in dep_pkgs {
                for dep in deps.iter().filter(|dep| {
                    dep.artifact()
                        .map(|artifact| artifact.is_lib())
                        .unwrap_or(true)
                }) {
                    ws.gctx().shell().warn(&format!(
                        "{} ignoring invalid dependency `{}` which is missing a lib target",
                        pkg_id,
                        dep.name_in_toml(),
                    ))?;
                }
            }
        }
        Ok(())
    }

    pub fn filter_deps<'a>(
        pkg_id: PackageId,
        resolve: &'a Resolve,
        has_dev_units: HasDevUnits,
        requested_kinds: &'a [CompileKind],
        target_data: &'a RustcTargetData<'_>,
        force_all_targets: ForceAllTargets,
    ) -> impl Iterator<Item = (PackageId, &'a HashSet<Dependency>)> + 'a {
        resolve
            .deps(pkg_id)
            .filter(move |&(_id, deps)| {
                deps.iter().any(|dep| {
                    if dep.kind() == DepKind::Development && has_dev_units == HasDevUnits::No {
                        return false;
                    }
                    if force_all_targets == ForceAllTargets::No {
                        let activated = requested_kinds
                            .iter()
                            .chain(Some(&CompileKind::Host))
                            .any(|kind| target_data.dep_platform_activated(dep, *kind));
                        if !activated {
                            return false;
                        }
                    }
                    true
                })
            })
            .into_iter()
    }

    pub fn sources(&self) -> Ref<'_, SourceMap<'gctx>> {
        self.sources.borrow()
    }

    pub fn sources_mut(&self) -> RefMut<'_, SourceMap<'gctx>> {
        self.sources.borrow_mut()
    }

    /// Merge the given set into self.
    pub fn add_set(&mut self, set: PackageSet<'gctx>) {
        assert!(!self.downloading.get());
        assert!(!set.downloading.get());
        for (pkg_id, p_cell) in set.packages {
            self.packages.entry(pkg_id).or_insert(p_cell);
        }
        let mut sources = self.sources.borrow_mut();
        let other_sources = set.sources.into_inner();
        sources.add_source_map(other_sources);
    }
}

