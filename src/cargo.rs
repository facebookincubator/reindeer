/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Interface with Cargo
//!
//! This module defines invocations of `cargo` to either perform actions (vendor, update) or
//! get metadata about a crate. It also defines all the types for deserializing from Cargo's
//! JSON output.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::hash_map;
use std::env;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fs;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::iter;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::thread;

use anyhow::Context;
use anyhow::bail;
use cargo::core::GitReference;
use cargo::core::Package;
use cargo::core::PackageId;
use cargo::core::SourceId;
use cargo::sources::CRATES_IO_REGISTRY;
use cargo::util::cache_lock::CacheLockMode;
use cargo::util::interning::InternedString;
use cargo_toml::OptionalFile;
use foldhash::HashMap;
use foldhash::HashSet;
use globset::GlobSet;
use ignore::gitignore::Gitignore;
use indoc::indoc;
use semver::VersionReq;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::Unexpected;
use serde::de::Visitor;
use serde_with::As;
use serde_with::DeserializeAs;
use sha2::Digest;
use sha2::Sha256;
use walkdir::WalkDir;

use crate::Args;
use crate::Paths;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::lockfile::Lockfile;
use crate::platform::PlatformExpr;
use crate::remap::RemapConfig;
use crate::remap::RemapSource;

static STAGING_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const SYNTHESIZED_BUILD_RS: &[u8] = b"fn main() {}\n";

pub fn cargo_get_lockfile_and_metadata(
    config: &Config,
    args: &Args,
    paths: &Paths,
    fast: bool,
) -> anyhow::Result<(Lockfile, Metadata)> {
    if let VendorConfig::Source(_) = config.vendor {
        let lockfile = Lockfile::load(paths)?;
        let metadata = if fast {
            fast_metadata(config, args, paths)?
        } else {
            slow_metadata(config, args, paths)?
        };
        Ok((lockfile, metadata))
    } else if fast {
        bail!("`--fast` currently only works with `vendor = true` in reindeer.toml");
    } else {
        let metadata = slow_metadata(config, args, paths)?;
        // In non-vendored mode, we allow `cargo metadata` to make changes to
        // the lockfile, so load it second.
        let lockfile = Lockfile::load(paths)?;
        Ok((lockfile, metadata))
    }
}

fn slow_metadata(config: &Config, args: &Args, paths: &Paths) -> anyhow::Result<Metadata> {
    let mut cargo_flags = vec![
        "metadata",
        "--format-version",
        "1",
        "--manifest-path",
        paths.manifest_path.to_str().unwrap(),
        "--all-features",
    ];

    let cargo_home = if let VendorConfig::Source(_) = config.vendor {
        // The Cargo.lock should already have been updated by the vendor step.
        // We must not change it during buckify or else we'd be generating Buck
        // targets for not the same crate versions that were put in the vendor
        // directory.
        cargo_flags.extend(["--frozen", "--locked", "--offline"]);

        Some(paths.cargo_home.as_path())
    } else {
        None
    };

    run_cargo_json(config, cargo_home, None, args, &cargo_flags).context("parsing metadata")
}

/// Named flags for [`make_gctx`] so call sites are self-documenting.
struct GctxProperties {
    frozen: bool,
    locked: bool,
    offline: bool,
    quiet: bool,
    git_fetch_with_cli: bool,
}

/// Build a cargo `GlobalContext` configured for the third-party directory.
///
/// Shared by `fast_metadata` and `fast_vendor` so the cargo-as-a-library setup
/// is consistent across both code paths.
fn make_gctx(
    config: &Config,
    args: &Args,
    paths: &Paths,
    props: GctxProperties,
) -> anyhow::Result<cargo::GlobalContext> {
    let shell = cargo::core::Shell::new();
    let cwd = paths.third_party_dir.clone();
    let cargo_home = paths.cargo_home.clone();
    let mut gctx = cargo::GlobalContext::new(shell, cwd, cargo_home);

    let mut unstable_flags = Vec::new();
    if config.cargo.bindeps {
        unstable_flags.push("bindeps".to_owned());
    }

    let mut cli_config = Vec::new();
    if props.git_fetch_with_cli {
        cli_config.push("net.git-fetch-with-cli=true".to_owned());
    }
    if let Some(rustc_path) = get_rustc(config, args) {
        let rustc_path = rustc_path
            .to_str()
            .context("Failed to set cargo's build.rustc config")?;
        cli_config.push(format!(
            "build.rustc={}",
            toml::Value::String(rustc_path.to_owned()),
        ));
    }

    let verbose = 0;
    let color = None;
    let target_dir = None;
    gctx.configure(
        verbose,
        props.quiet,
        color,
        props.frozen,
        props.locked,
        props.offline,
        &target_dir,
        &unstable_flags,
        &cli_config,
    )?;

    Ok(gctx)
}

fn fast_metadata(config: &Config, args: &Args, paths: &Paths) -> anyhow::Result<Metadata> {
    let gctx = make_gctx(
        config,
        args,
        paths,
        GctxProperties {
            frozen: true,
            locked: true,
            offline: true,
            quiet: false,
            git_fetch_with_cli: false,
        },
    )?;

    // Load .cargo/config.toml (source replacements).
    let source_config = cargo::sources::SourceConfigMap::new(&gctx)?;

    // Load workspace Cargo.toml.
    let workspace = cargo::core::Workspace::new(&paths.manifest_path, &gctx)?;

    // Load workspace Cargo.lock.
    let Some(resolve) = cargo::ops::load_pkg_lockfile(&workspace)? else {
        bail!(indoc! {"
            When using `vendor = true` in reindeer.toml, `reindeer buckify` requires that a
            Cargo.lock already exists. Otherwise buckify might be generating Buck targets
            for not the same crate versions that have been vendored. Run `reindeer vendor`
            first.
        "});
    };

    // Instantiate package sources.
    let mut source_map = HashMap::default();
    let mut shared_sources = HashMap::default();
    let yanked_whitelist = std::collections::HashSet::new();
    for pkg_id in resolve.iter() {
        let source_id = pkg_id.source_id();
        let hash_map::Entry::Vacant(entry) = source_map.entry(source_id) else {
            continue;
        };
        let source = source_config.load(source_id, &yanked_whitelist)?;
        assert_eq!(source.source_id(), source_id);
        let replaced_source_id = source.replaced_source_id();
        let delegate = match shared_sources.entry(replaced_source_id) {
            hash_map::Entry::Vacant(entry) => {
                let source = source_config.load(replaced_source_id, &yanked_whitelist)?;
                assert_eq!(source.source_id(), replaced_source_id);
                let rc = Rc::new(RefCell::new(source));
                Rc::clone(entry.insert(rc))
            }
            hash_map::Entry::Occupied(entry) => Rc::clone(entry.get()),
        };
        entry.insert(cargo::sources::ReplacedSource::new(
            source_id,
            replaced_source_id,
            Box::new(SharedSource { delegate }),
        ));
    }

    // Load packages from each source into package registry.
    let mut registry =
        cargo::core::registry::PackageRegistry::new_with_source_config(&gctx, source_config)?;
    for mut source in source_map.into_values() {
        cargo::sources::source::Source::block_until_ready(&mut source)?;
        registry.add_preloaded(Box::new(source));
    }

    // Resolve dependency graph.
    let keep_previous = None;
    let specs = [];
    let register_patches = true;
    let resolve = cargo::ops::resolve_with_previous(
        &mut registry,
        &workspace,
        &cargo::core::resolver::CliFeatures::new_all(true),
        cargo::core::resolver::HasDevUnits::Yes,
        Some(&resolve),
        keep_previous,
        &specs,
        register_patches,
    )?;

    // Load vendored Cargo.tomls.
    let packages = Vec::from_iter(resolve.iter());
    let package_set = registry.get(&packages)?;
    let package_vec = package_set.get_many(packages.iter().copied())?;

    let mut metadata = Metadata {
        packages: Vec::new(),
        workspace_members: Vec::new(),
        resolve: Resolve {
            root: None,
            nodes: Vec::new(),
        },
    };

    // Convert Cargo data structures to Reindeer data structures.
    let mut package_map = BTreeMap::new();
    for &pkg in &package_vec {
        let pkgid = pkg.package_id();
        package_map.insert(pkgid, pkg);

        // Serialize cargo::core::Package and deserialize to our own Manifest struct.
        let serialized = pkg.serialized(gctx.cli_unstable(), workspace.unstable_features());
        let json = serde_json::to_value(serialized).unwrap();
        let manifest = serde_json::from_value(json)
            .with_context(|| format!("failed to deserialize manifest of package {pkgid}"))?;
        metadata.packages.push(manifest);

        if workspace.is_member_id(pkgid) {
            metadata.workspace_members.push(pkgid);
        }
    }

    let mut node_map = BTreeMap::new();
    for member_pkg in workspace.members() {
        build_resolve_graph(
            &mut node_map,
            member_pkg.package_id(),
            &resolve,
            &package_map,
        )?;
    }

    metadata.resolve.nodes.extend(node_map.into_values());
    metadata.resolve.root = workspace
        .current_opt()
        .map(cargo::core::Package::package_id);

    Ok(metadata)
}

pub struct SharedSource<'gctx> {
    delegate: Rc<RefCell<Box<dyn cargo::sources::source::Source + 'gctx>>>,
}

impl<'gctx> cargo::sources::source::Source for SharedSource<'gctx> {
    fn source_id(&self) -> cargo::core::SourceId {
        self.delegate.borrow().source_id()
    }

    fn supports_checksums(&self) -> bool {
        self.delegate.borrow().supports_checksums()
    }

    fn requires_precise(&self) -> bool {
        self.delegate.borrow().requires_precise()
    }

    fn query(
        &mut self,
        dep: &cargo::core::Dependency,
        kind: cargo::sources::source::QueryKind,
        f: &mut dyn FnMut(cargo::sources::IndexSummary),
    ) -> Poll<anyhow::Result<()>> {
        self.delegate.borrow_mut().query(dep, kind, f)
    }

    fn invalidate_cache(&mut self) {
        self.delegate.borrow_mut().invalidate_cache();
    }

    fn set_quiet(&mut self, quiet: bool) {
        self.delegate.borrow_mut().set_quiet(quiet);
    }

    fn download(
        &mut self,
        pkg_id: PackageId,
    ) -> anyhow::Result<cargo::sources::source::MaybePackage> {
        self.delegate.borrow_mut().download(pkg_id)
    }

    fn finish_download(
        &mut self,
        pkg_id: PackageId,
        contents: Vec<u8>,
    ) -> anyhow::Result<cargo::core::Package> {
        self.delegate.borrow_mut().finish_download(pkg_id, contents)
    }

    fn fingerprint(&self, pkg: &cargo::core::Package) -> anyhow::Result<String> {
        self.delegate.borrow().fingerprint(pkg)
    }

    fn describe(&self) -> String {
        self.delegate.borrow().describe()
    }

    fn add_to_yanked_whitelist(&mut self, pkgs: &[PackageId]) {
        self.delegate.borrow_mut().add_to_yanked_whitelist(pkgs);
    }

    fn is_yanked(&mut self, pkg: PackageId) -> Poll<anyhow::Result<bool>> {
        self.delegate.borrow_mut().is_yanked(pkg)
    }

    fn block_until_ready(&mut self) -> anyhow::Result<()> {
        self.delegate.borrow_mut().block_until_ready()
    }

    fn verify(&self, pkg: PackageId) -> anyhow::Result<()> {
        self.delegate.borrow().verify(pkg)
    }
}

// From cargo-0.91.0/src/cargo/ops/cargo_output_metadata.rs
fn build_resolve_graph(
    node_map: &mut BTreeMap<PackageId, Node>,
    pkg_id: PackageId,
    resolve: &cargo::core::resolver::Resolve,
    package_map: &BTreeMap<PackageId, &cargo::core::Package>,
) -> anyhow::Result<()> {
    if node_map.contains_key(&pkg_id) {
        return Ok(());
    }

    let normalize_id = |id| -> PackageId { *package_map.get_key_value(&id).unwrap().0 };

    let deps = {
        let mut dep_metadatas = Vec::new();
        let iter = resolve.deps(pkg_id);
        for (dep_id, deps) in iter {
            let mut dep_kinds = Vec::new();

            let targets = package_map[&dep_id].targets();

            // Try to get the extern name for lib, or crate name for bins.
            let extern_name = |target| {
                resolve
                    .extern_crate_name_and_dep_name(pkg_id, dep_id, target)
                    .map(|(ext_crate_name, _)| ext_crate_name)
            };

            let lib_target = targets.iter().find(|t| t.is_lib());

            for dep in deps.iter() {
                if let Some(target) = lib_target {
                    // When we do have a library target, include them in deps if...
                    let included = match dep.artifact() {
                        // it is not an artifact dep at all
                        None => true,
                        // it is also an artifact dep with `{ …, lib = true }`
                        Some(a) if a.is_lib() => true,
                        _ => false,
                    };
                    // TODO(bindeps): Cargo shouldn't have `extern_name` field
                    // if the user is not using -Zbindeps.
                    // Remove this condition ` after -Zbindeps gets stabilized.
                    let extern_name = if dep.artifact().is_some() {
                        Some(extern_name(target)?)
                    } else {
                        None
                    };
                    if included {
                        dep_kinds.push(NodeDepKind {
                            kind: match dep.kind() {
                                cargo::core::dependency::DepKind::Normal => DepKind::Normal,
                                cargo::core::dependency::DepKind::Development => DepKind::Dev,
                                cargo::core::dependency::DepKind::Build => DepKind::Build,
                            },
                            target: dep
                                .platform()
                                .map(|plat| PlatformExpr::parse(&plat.to_string()).unwrap()),
                            extern_name: extern_name.map(|intern| intern.as_str().to_owned()),
                            artifact: None,
                            compile_target: None,
                            bin_name: None,
                        });
                    }
                }

                // No need to proceed if there is no artifact dependency.
                if dep.artifact().is_none() {
                    continue;
                }

                let target_set =
                    match_artifacts_kind_with_targets(dep, targets, pkg_id.name().as_str())?;

                for (kind, target) in target_set {
                    dep_kinds.push(NodeDepKind {
                        kind: match dep.kind() {
                            cargo::core::dependency::DepKind::Normal => DepKind::Normal,
                            cargo::core::dependency::DepKind::Development => DepKind::Dev,
                            cargo::core::dependency::DepKind::Build => DepKind::Build,
                        },
                        target: dep
                            .platform()
                            .map(|plat| PlatformExpr::parse(&plat.to_string()).unwrap()),
                        extern_name: extern_name(target)
                            .ok()
                            .map(|intern| intern.as_str().to_owned()),
                        artifact: Some(match kind.crate_type() {
                            "bin" => ArtifactKind::EveryBin,
                            "staticlib" => ArtifactKind::Staticlib,
                            "cdylib" => ArtifactKind::Cdylib,
                            _ => unimplemented!(),
                        }),
                        compile_target: None,
                        bin_name: target.is_bin().then(|| target.name().to_string()),
                    })
                }
            }

            let pkg_id = normalize_id(dep_id);

            let dep = match (lib_target, dep_kinds.len()) {
                (Some(target), _) => NodeDep {
                    name: Some(extern_name(target)?.as_str().to_owned()),
                    pkg: pkg_id,
                    dep_kinds,
                },
                // No lib target exists but contains artifact deps.
                (None, 1..) => NodeDep {
                    name: None,
                    pkg: pkg_id,
                    dep_kinds,
                },
                // No lib or artifact dep exists.
                // Usually this mean parent depending on non-lib bin crate.
                (None, _) => continue,
            };

            dep_metadatas.push(dep)
        }
        dep_metadatas
    };

    let to_visit: Vec<PackageId> = deps.iter().map(|dep| dep.pkg).collect();
    let node = Node {
        id: normalize_id(pkg_id),
        deps,
    };
    node_map.insert(pkg_id, node);
    for dep_id in to_visit {
        build_resolve_graph(node_map, dep_id, resolve, package_map)?;
    }

    Ok(())
}

// From cargo-0.91.0/src/cargo/core/compiler/artifact.rs
fn match_artifacts_kind_with_targets<'a>(
    artifact_dep: &'a cargo::core::Dependency,
    targets: &'a [cargo::core::Target],
    parent_package: &str,
) -> anyhow::Result<
    HashSet<(
        &'a cargo::core::dependency::ArtifactKind,
        &'a cargo::core::Target,
    )>,
> {
    let mut out = HashSet::default();
    let artifact_requirements = artifact_dep.artifact().expect("artifact present");
    for artifact_kind in artifact_requirements.kinds() {
        let mut extend = |kind, filter: &dyn Fn(&&cargo::core::Target) -> bool| {
            let mut iter = targets.iter().filter(filter).peekable();
            let found = iter.peek().is_some();
            out.extend(iter::repeat(kind).zip(iter));
            found
        };
        let found = match artifact_kind {
            cargo::core::dependency::ArtifactKind::Cdylib => {
                extend(artifact_kind, &|t| t.is_cdylib())
            }
            cargo::core::dependency::ArtifactKind::Staticlib => {
                extend(artifact_kind, &|t| t.is_staticlib())
            }
            cargo::core::dependency::ArtifactKind::AllBinaries => {
                extend(artifact_kind, &|t| t.is_bin())
            }
            cargo::core::dependency::ArtifactKind::SelectedBinary(bin_name) => {
                extend(artifact_kind, &|t| {
                    t.is_bin() && t.name() == bin_name.as_str()
                })
            }
        };
        if !found {
            bail!(
                "dependency `{}` in package `{}` requires a `{}` artifact to be present.",
                artifact_dep.name_in_toml(),
                parent_package,
                artifact_kind
            );
        }
    }
    Ok(out)
}

/// Glob and gitignore rules for files to omit from `.cargo-checksum.json`.
///
/// Both `GlobSet` and `Gitignore` are `Sync`, so this struct can be shared
/// across threads via a shared reference.
pub(crate) struct ChecksumFilter {
    pub(crate) remove_globs: GlobSet,
    pub(crate) gitignore: Gitignore,
}

/// Filtering parameters passed into `fast_vendor`.
///
/// Controls which files are excluded from the vendor directory and checksums.
pub(crate) struct VendorFilters {
    /// Name of the BUCK file to exclude from extraction and checksums (e.g. `"BUCK"`).
    /// `None` means no exclusion (split mode is disabled).
    pub(crate) buck_file_name: Option<String>,
    /// Glob/gitignore rules for files to omit from `.cargo-checksum.json`.
    pub(crate) checksum_filter: Option<ChecksumFilter>,
}

#[derive(Debug, Deserialize)]
struct CargoChecksumJson {
    #[serde(default)]
    package: Option<String>,
}

struct ExpectedCrate {
    dst_name: String,
    dst: PathBuf,
    pkgdir: PathBuf,
    pkg_cksum: Option<String>,
    materialization: Materialization,
}

struct PendingCrate {
    pkg_id: PackageId,
    dst_name: String,
    dst: PathBuf,
    pkgdir: PathBuf,
    pkg_cksum: Option<String>,
    materialization: PendingMaterialization,
}

enum PendingMaterialization {
    RegistryArchive { archive: PathBuf },
    LoadFromPackageSet,
}

enum Materialization {
    RegistryArchive {
        archive: PathBuf,
    },
    CopyFiles {
        src_root: PathBuf,
        file_paths: Vec<PathBuf>,
        normalized_cargo_toml: Option<String>,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum TreeEntryFingerprint {
    File(String),
    Symlink(String),
}

/// Vendor crates using cargo-as-a-library, with parallel archive extraction.
///
/// This replaces the `cargo vendor` subprocess call with direct API usage,
/// enabling parallel extraction of `.crate` archives via `thread::scope`.
/// The result is a populated `vendor/` directory and a `.cargo/config.toml`
/// with source replacement entries.
pub(crate) fn fast_vendor(
    config: &Config,
    no_delete: bool,
    args: &Args,
    paths: &Paths,
    filters: VendorFilters,
) -> anyhow::Result<()> {
    let vendor_dir = paths.third_party_dir.join("vendor");
    let cargo_config_path = paths.cargo_home.join("config.toml");
    let gctx = make_gctx(
        config,
        args,
        paths,
        GctxProperties {
            frozen: false,
            locked: false,
            offline: false,
            quiet: true,
            git_fetch_with_cli: true,
        },
    )?;

    let manifest_path = paths.manifest_path.clone();
    let ws = cargo::core::Workspace::new(&manifest_path, &gctx)?;

    eprintln!("Resolving workspace...");
    let resolve = resolve_ws_with_original_sources(&ws, &gctx, false)
        .context("failed to resolve workspace")?;

    let original_package_ids = resolve
        .iter()
        .filter(|pkg_id| !pkg_id.source_id().is_path())
        .collect::<Vec<_>>();

    fs::create_dir_all(&vendor_dir)?;

    // Collect existing top-level entries for cleanup (unless --no-delete).
    let mut to_remove: BTreeSet<PathBuf> = if no_delete {
        BTreeSet::new()
    } else {
        collect_vendor_cleanup_entries(&vendor_dir)?
    };

    let mut pending_crates = Vec::new();
    let mut package_load_ids = Vec::new();
    let mut destinations = BTreeMap::new();
    let mut sources: BTreeSet<SourceId> = BTreeSet::new();
    let mut prepared_crates = 0usize;

    eprintln!(
        "Preparing expected contents for {} vendored crates...",
        original_package_ids.len(),
    );
    for pkg_id in resolve.iter() {
        // Skip path dependencies -- they're already in the source tree.
        // Any preexisting vendor/<name>-<version> directory for this crate
        // stays in `to_remove` so it will be deleted as stale.
        if pkg_id.source_id().is_path() {
            continue;
        }

        if pkg_id.source_id().is_git() {
            eprintln!("Preparing git crate {pkg_id}...");
        }
        let dst_name = format!("{}-{}", pkg_id.name(), pkg_id.version());
        if let Some(previous) = destinations.insert(dst_name.clone(), pkg_id) {
            bail!(
                "multiple packages resolve to vendored directory `{}`: {} and {}",
                dst_name,
                previous,
                pkg_id,
            );
        }
        let dst = vendor_dir.join(&dst_name);
        let pkgdir = dst
            .strip_prefix(&paths.third_party_dir)
            .expect("dst is always under third_party_dir")
            .to_path_buf();
        remove_expected_vendor_entries_from_cleanup(
            &mut to_remove,
            &vendor_dir,
            &dst,
            pkg_id.name().as_str(),
            filters.buck_file_name.as_deref(),
        );
        sources.insert(pkg_id.source_id());

        let pkg_cksum = resolve.checksums().get(&pkg_id).and_then(|c| c.clone());

        let materialization = if pkg_id.source_id().is_registry() {
            let pkg_cksum = pkg_cksum.as_deref().with_context(|| {
                format!("missing lockfile checksum for registry package {pkg_id}")
            })?;
            if let Some(archive) =
                find_cached_registry_archive(&paths.cargo_home, pkg_id, pkg_cksum)?
            {
                PendingMaterialization::RegistryArchive { archive }
            } else {
                package_load_ids.push(pkg_id);
                PendingMaterialization::LoadFromPackageSet
            }
        } else {
            if pkg_id.source_id().is_git() && pkg_id.source_id().precise_git_fragment().is_none() {
                bail!("git package {pkg_id} is not locked to a precise revision");
            }
            package_load_ids.push(pkg_id);
            PendingMaterialization::LoadFromPackageSet
        };

        pending_crates.push(PendingCrate {
            pkg_id,
            dst_name,
            dst,
            pkgdir,
            pkg_cksum,
            materialization,
        });
        prepared_crates += 1;
        if prepared_crates == original_package_ids.len() || prepared_crates.is_multiple_of(250) {
            eprintln!(
                "Prepared {prepared_crates}/{} expected crate contents...",
                original_package_ids.len(),
            );
        }
    }

    let original_package_set = if package_load_ids.is_empty() {
        None
    } else {
        let source_ids_to_load = package_load_ids
            .iter()
            .map(|pkg_id| pkg_id.source_id())
            .collect::<BTreeSet<_>>();
        eprintln!(
            "Preparing {} external source(s) for {} git or missing-cache crate(s)...",
            source_ids_to_load.len(),
            package_load_ids.len(),
        );

        let original_source_config = cargo::sources::SourceConfigMap::empty(&gctx)?;
        let mut original_source_map = cargo::sources::source::SourceMap::new();
        let yanked_whitelist = std::collections::HashSet::new();
        {
            let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
            for (index, source_id) in source_ids_to_load.iter().copied().enumerate() {
                if source_id.is_git() {
                    eprintln!("Fetching git source {source_id}...");
                }
                let mut source = original_source_config
                    .load(source_id, &yanked_whitelist)
                    .with_context(|| format!("failed to load original source {source_id}"))?;
                cargo::sources::source::Source::block_until_ready(&mut source)?;
                original_source_map.insert(source);
                let completed = index + 1;
                if completed == source_ids_to_load.len() || completed.is_multiple_of(10) {
                    eprintln!(
                        "Prepared {completed}/{} external sources...",
                        source_ids_to_load.len(),
                    );
                }
            }
        }

        let package_set =
            cargo::core::PackageSet::new(&package_load_ids, original_source_map, &gctx)?;
        eprintln!(
            "Downloading/materializing {} git or missing-cache crate(s)...",
            package_load_ids.len(),
        );
        package_set
            .get_many(package_load_ids.iter().copied())
            .context("failed to download original packages")?;
        Some(package_set)
    };

    let mut expected_crates = Vec::new();
    for pending in pending_crates {
        let materialization = match pending.materialization {
            PendingMaterialization::RegistryArchive { archive } => {
                Materialization::RegistryArchive { archive }
            }
            PendingMaterialization::LoadFromPackageSet => {
                let package_set = original_package_set
                    .as_ref()
                    .expect("package set should be loaded when crates need it");
                let original_pkg = package_set
                    .get_one(pending.pkg_id)
                    .context("failed to fetch original package")?;
                if pending.pkg_id.source_id().is_registry() {
                    let pkg_cksum = pending.pkg_cksum.as_deref().with_context(|| {
                        format!(
                            "missing lockfile checksum for registry package {}",
                            pending.pkg_id
                        )
                    })?;
                    let archive = find_cached_registry_archive(
                        &paths.cargo_home,
                        pending.pkg_id,
                        pkg_cksum,
                    )?
                    .with_context(|| {
                        format!(
                            "missing cached registry archive for {} after Cargo package load",
                            pending.pkg_id,
                        )
                    })?;
                    Materialization::RegistryArchive { archive }
                } else {
                    let src_root = original_pkg.root().to_path_buf();
                    let file_paths = cargo::sources::PathSource::new(
                        original_pkg.root(),
                        pending.pkg_id.source_id(),
                        &gctx,
                    )
                    .list_files(original_pkg)?
                    .into_iter()
                    .map(|entry| entry.into_path_buf())
                    .collect::<Vec<_>>();
                    let normalized_cargo_toml = if pending.pkg_id.source_id().is_git() {
                        Some(prepare_git_cargo_toml_for_vendor(
                            original_pkg,
                            &file_paths,
                            &src_root,
                            &gctx,
                        )?)
                    } else {
                        None
                    };

                    Materialization::CopyFiles {
                        src_root,
                        file_paths,
                        normalized_cargo_toml,
                    }
                }
            }
        };
        expected_crates.push(ExpectedCrate {
            dst_name: pending.dst_name,
            dst: pending.dst,
            pkgdir: pending.pkgdir,
            pkg_cksum: pending.pkg_cksum,
            materialization,
        });
    }

    // Generate .cargo/config.toml with source replacements before mutating the
    // vendor tree so serialization failures are caught early.
    let vendor_config = generate_vendor_config(&sources, &vendor_dir)?;

    // Compare expected crates in parallel and replace only mismatches.
    // Filters are shared by reference across threads; ChecksumFilter is Sync.
    let num_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let chunk_size = expected_crates.len().div_ceil(num_threads.max(1));
    let filters = &filters;
    let total = expected_crates.len();
    let progress = AtomicUsize::new(0);

    if total > 0 {
        eprintln!("Checking {total} vendored crates...");
    }

    let progress = &progress;
    let process_result = thread::scope(|s| {
        let handles: Vec<_> = expected_crates
            .chunks(chunk_size.max(1))
            .map(|chunk| {
                s.spawn(move || {
                    for expected in chunk {
                        process_expected_crate(expected, filters)
                            .with_context(|| format!("failed to vendor {}", expected.dst_name))?;
                        let completed = progress.fetch_add(1, Ordering::Relaxed) + 1;
                        if completed == total || completed.is_multiple_of(250) {
                            eprintln!("Checked {completed}/{total} vendored crates...");
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("vendor thread panicked")?;
        }
        Ok::<_, anyhow::Error>(())
    });
    process_result.context(
        "fast vendoring failed; if the vendor tree was partially updated, run `sl revert third-party/rust/vendor third-party/rust/.cargo/config.toml`, then `sl purge --cwd \"$FBSOURCE\" --all third-party/rust/vendor`, then retry `fbcode/common/rust/tools/reindeer/vendor --fast`",
    )?;

    // Remove stale vendor entries left behind by prior runs or by drift in the
    // top-level vendor directory.
    for stale in &to_remove {
        remove_existing_path(stale)?;
    }

    fs::create_dir_all(&paths.cargo_home)?;
    write_regular_file_if_changed(&cargo_config_path, vendor_config.as_bytes())
        .with_context(|| format!("failed to write {}", cargo_config_path.display()))?;

    Ok(())
}

fn resolve_ws_with_original_sources<'gctx>(
    ws: &cargo::core::Workspace<'gctx>,
    gctx: &'gctx cargo::GlobalContext,
    dry_run: bool,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    let source_config = cargo::sources::SourceConfigMap::empty(gctx)?;
    let mut registry =
        cargo::core::registry::PackageRegistry::new_with_source_config(gctx, source_config)?;
    let previous_resolve = cargo::ops::load_pkg_lockfile(ws)?;
    let mut resolve = cargo::ops::resolve_with_previous(
        &mut registry,
        ws,
        &cargo::core::resolver::CliFeatures::new_all(true),
        cargo::core::resolver::HasDevUnits::Yes,
        previous_resolve.as_ref(),
        None,
        &[],
        true,
    )?;

    let print_changes = if !ws.is_ephemeral() && ws.require_optional_deps() {
        if dry_run {
            true
        } else {
            cargo::ops::write_pkg_lockfile(ws, &mut resolve)?
        }
    } else {
        false
    };
    if print_changes {
        cargo::ops::print_lockfile_changes(ws, previous_resolve.as_ref(), &resolve, &mut registry)?;
    }

    Ok(resolve)
}

fn path_file_type_no_follow(path: &Path) -> anyhow::Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let len = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if len == 0 {
            break;
        }
        hasher.update(&buffer[..len]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn bytes_sha256(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn read_regular_file_to_string(path: &Path) -> Option<String> {
    let Ok(file_type) = path_file_type_no_follow(path) else {
        return None;
    };
    let file_type = file_type?;
    if !file_type.is_file() {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn remove_existing_path(path: &Path) -> anyhow::Result<()> {
    let Some(file_type) = path_file_type_no_follow(path)? else {
        return Ok(());
    };
    if file_type.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove stale vendor dir {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale vendor file {}", path.display()))?;
    }
    Ok(())
}

fn prepare_regular_file_target(path: &Path) -> anyhow::Result<()> {
    let Some(file_type) = path_file_type_no_follow(path)? else {
        return Ok(());
    };
    if file_type.is_file() {
        return Ok(());
    }
    remove_existing_path(path)
}

fn write_regular_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    prepare_regular_file_target(path)?;
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn write_regular_file_if_changed(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    if path_file_type_no_follow(path)?.is_some_and(|file_type| file_type.is_file())
        && fs::read(path).with_context(|| format!("failed to read {}", path.display()))? == contents
    {
        return Ok(());
    }
    write_regular_file(path, contents)
}

fn collect_vendor_cleanup_entries(vendor_dir: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
    if !vendor_dir.exists() {
        return Ok(BTreeSet::new());
    }

    fs::read_dir(vendor_dir)?
        .map(|entry| {
            let entry = entry?;
            Ok(entry.path())
        })
        .collect::<Result<BTreeSet<_>, io::Error>>()
        .map_err(anyhow::Error::from)
        .with_context(|| format!("failed to read {}", vendor_dir.display()))
}

fn remove_expected_vendor_entries_from_cleanup(
    to_remove: &mut BTreeSet<PathBuf>,
    vendor_dir: &Path,
    source_dir: &Path,
    package_name: &str,
    buck_file_name: Option<&str>,
) {
    to_remove.remove(source_dir);
    if buck_file_name.is_some() {
        to_remove.remove(&vendor_dir.join(package_name));
    }
}

pub(crate) fn read_existing_package_checksum(crate_dir: &Path) -> anyhow::Result<Option<String>> {
    Ok(read_existing_checksum_json(crate_dir)?.package)
}

fn read_existing_checksum_json(crate_dir: &Path) -> anyhow::Result<CargoChecksumJson> {
    let cksum_path = crate_dir.join(".cargo-checksum.json");
    let content = read_regular_file_to_string(&cksum_path)
        .with_context(|| format!("missing or non-regular {}", cksum_path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", cksum_path.display()))
}

fn find_cached_registry_archive(
    cargo_home: &Path,
    pkg_id: PackageId,
    expected_checksum: &str,
) -> anyhow::Result<Option<PathBuf>> {
    find_cached_registry_archive_by_tarball(cargo_home, &pkg_id.tarball_name(), expected_checksum)
}

fn find_cached_registry_archive_by_tarball(
    cargo_home: &Path,
    tarball_name: &str,
    expected_checksum: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let cache_dir = cargo_home.join("registry").join("cache");
    let entries = match fs::read_dir(&cache_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", cache_dir.display()));
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let archive = entry.path().join(tarball_name);
        let Some(file_type) = path_file_type_no_follow(&archive)? else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        match file_sha256(&archive) {
            Ok(actual) if actual == expected_checksum => return Ok(Some(archive)),
            Ok(_) => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(None)
}

fn is_split_buck_file(relative: &Path, buck_file_name: Option<&str>) -> bool {
    buck_file_name.is_some_and(|name| relative == Path::new(name))
}

fn process_expected_crate(expected: &ExpectedCrate, filters: &VendorFilters) -> anyhow::Result<()> {
    if vendor_dir_matches_expected_source(expected, filters)? {
        return Ok(());
    }

    let (staging_root, staging_dst) = make_staging_destination(&expected.dst)?;
    let result = (|| -> anyhow::Result<()> {
        materialize_expected_crate(expected, &staging_dst, filters)?;
        replace_vendor_dir(&staging_dst, &expected.dst)?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging_root);
    result
}

fn vendor_dir_matches_expected_source(
    expected: &ExpectedCrate,
    filters: &VendorFilters,
) -> anyhow::Result<bool> {
    let Some(actual_type) = path_file_type_no_follow(&expected.dst)? else {
        return Ok(false);
    };
    if !actual_type.is_dir() {
        return Ok(false);
    }

    Ok(expected_tree_fingerprint(expected, filters)?
        == tree_fingerprint(&expected.dst, &expected.pkgdir, filters)?)
}

fn expected_tree_fingerprint(
    expected: &ExpectedCrate,
    filters: &VendorFilters,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    match &expected.materialization {
        Materialization::RegistryArchive { archive } => expected_registry_archive_fingerprint(
            expected,
            archive,
            filters,
            expected.pkg_cksum.as_deref(),
        ),
        Materialization::CopyFiles {
            src_root,
            file_paths,
            normalized_cargo_toml,
        } => expected_copy_source_fingerprint(
            src_root,
            file_paths,
            normalized_cargo_toml.as_deref(),
            &expected.pkgdir,
            filters,
            expected.pkg_cksum.as_deref(),
        ),
    }
}

fn expected_registry_archive_fingerprint(
    expected: &ExpectedCrate,
    archive: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let tarball =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let archive_size = tarball.metadata()?.len();
    let size_limit = u64::max(512 * 1024 * 1024, archive_size * 20);
    let gz = flate2::read::GzDecoder::new(LimitReader::new(tarball, size_limit));
    let mut tar = tar::Archive::new(gz);
    let prefix = Path::new(&expected.dst_name);

    let mut fingerprint = BTreeMap::new();
    let mut file_cksums = BTreeMap::new();
    let mut cargo_toml = None;

    for entry in tar.entries().context("failed to read archive entries")? {
        let mut entry = entry.context("failed to read archive entry")?;
        let entry_path = entry
            .path()
            .context("failed to read entry path")?
            .into_owned();
        let relative = entry_path.strip_prefix(prefix).with_context(|| {
            format!("invalid tarball: entry at {entry_path:?} is not under {prefix:?}")
        })?;

        if source_excluded(&expected.pkgdir, relative, filters)
            || relative == Path::new(".cargo-checksum.json")
        {
            continue;
        }

        if relative.as_os_str().is_empty() {
            continue;
        }

        let key = path_key(relative)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        } else if entry_type.is_file() {
            let mut sha = Sha256::new();
            let mut buf = [0u8; 8 * 1024];
            let mut cargo_toml_contents = (key == "Cargo.toml").then(Vec::new);
            let hash = loop {
                let n = entry.read(&mut buf).with_context(|| {
                    format!("failed to read archive entry {}", entry_path.display())
                })?;
                if n == 0 {
                    break format!("{:x}", sha.finalize());
                }
                sha.update(&buf[..n]);
                if let Some(contents) = cargo_toml_contents.as_mut() {
                    contents.extend_from_slice(&buf[..n]);
                }
            };
            if let Some(contents) = cargo_toml_contents {
                if let Ok(content) = String::from_utf8(contents) {
                    cargo_toml = Some(content);
                }
            }
            fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
            maybe_insert_checksum(
                &mut file_cksums,
                &expected.pkgdir,
                relative,
                &key,
                &hash,
                filters,
            );
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .with_context(|| {
                    format!("failed to read symlink target for {}", entry_path.display())
                })?
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_default();
            fingerprint.insert(key, TreeEntryFingerprint::Symlink(target));
        } else {
            bail!("unsupported archive entry type at {}", entry_path.display());
        }
    }

    finish_expected_fingerprint(
        fingerprint,
        file_cksums,
        &expected.pkgdir,
        filters,
        pkg_cksum,
        cargo_toml.as_deref(),
    )
}

fn expected_copy_source_fingerprint(
    src_root: &Path,
    file_paths: &[PathBuf],
    normalized_cargo_toml: Option<&str>,
    pkgdir: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let mut fingerprint = BTreeMap::new();
    let mut file_cksums = BTreeMap::new();
    let mut cargo_toml = None;

    for src_path in file_paths {
        let relative = src_path.strip_prefix(src_root).with_context(|| {
            format!("{} is not under {}", src_path.display(), src_root.display(),)
        })?;

        if source_excluded(pkgdir, relative, filters)
            || relative == Path::new(".cargo-checksum.json")
        {
            continue;
        }

        let key = path_key(relative)?;
        let contents = if key == "Cargo.toml" {
            match normalized_cargo_toml {
                Some(contents) => contents.as_bytes().to_vec(),
                None => fs::read(src_path)
                    .with_context(|| format!("failed to read {}", src_path.display()))?,
            }
        } else {
            fs::read(src_path).with_context(|| format!("failed to read {}", src_path.display()))?
        };
        let hash = bytes_sha256(&contents);
        if key == "Cargo.toml" {
            if let Ok(content) = String::from_utf8(contents.clone()) {
                cargo_toml = Some(content);
            }
        }
        fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
        maybe_insert_checksum(&mut file_cksums, pkgdir, relative, &key, &hash, filters);
    }

    finish_expected_fingerprint(
        fingerprint,
        file_cksums,
        pkgdir,
        filters,
        pkg_cksum,
        cargo_toml.as_deref(),
    )
}

fn finish_expected_fingerprint(
    mut fingerprint: BTreeMap<String, TreeEntryFingerprint>,
    mut file_cksums: BTreeMap<String, String>,
    pkgdir: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
    cargo_toml: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    synthesize_missing_build_rs_fingerprint(
        &mut fingerprint,
        &mut file_cksums,
        pkgdir,
        filters,
        cargo_toml,
    )?;
    let checksum_json = checksum_json_bytes(pkg_cksum, &file_cksums)?;
    fingerprint.insert(
        ".cargo-checksum.json".to_owned(),
        TreeEntryFingerprint::File(bytes_sha256(&checksum_json)),
    );
    Ok(fingerprint)
}

fn synthesize_missing_build_rs_fingerprint(
    fingerprint: &mut BTreeMap<String, TreeEntryFingerprint>,
    file_cksums: &mut BTreeMap<String, String>,
    pkgdir: &Path,
    filters: &VendorFilters,
    cargo_toml: Option<&str>,
) -> anyhow::Result<()> {
    type TomlManifest = cargo_toml::Manifest<serde::de::IgnoredAny>;

    let Some(cargo_toml) = cargo_toml else {
        return Ok(());
    };
    let Ok(manifest) = toml::from_str::<TomlManifest>(cargo_toml) else {
        return Ok(());
    };
    let Some(package) = &manifest.package else {
        return Ok(());
    };
    let Some(OptionalFile::Path(build_script_path)) = &package.build else {
        return Ok(());
    };
    let build_script_path = normalize_manifest_path(build_script_path);
    let key = path_key(&build_script_path)?;
    if fingerprint.contains_key(&key) {
        return Ok(());
    }

    let hash = bytes_sha256(SYNTHESIZED_BUILD_RS);
    fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
    maybe_insert_checksum(
        file_cksums,
        pkgdir,
        &build_script_path,
        &key,
        &hash,
        filters,
    );
    Ok(())
}

fn maybe_insert_checksum(
    file_cksums: &mut BTreeMap<String, String>,
    pkgdir: &Path,
    relative: &Path,
    key: &str,
    hash: &str,
    filters: &VendorFilters,
) {
    if checksum_excluded(pkgdir, relative, key, filters.checksum_filter.as_ref()) {
        return;
    }
    file_cksums.insert(key.to_owned(), hash.to_owned());
}

fn checksum_excluded(
    pkgdir: &Path,
    relative: &Path,
    key: &str,
    filter: Option<&ChecksumFilter>,
) -> bool {
    filter.is_some_and(|filter| {
        filter.remove_globs.is_match(key) || gitignore_excluded(pkgdir, relative, Some(filter))
    })
}

fn source_excluded(pkgdir: &Path, relative: &Path, filters: &VendorFilters) -> bool {
    materialization_excluded(relative, filters)
        || gitignore_excluded(pkgdir, relative, filters.checksum_filter.as_ref())
}

fn materialization_excluded(relative: &Path, filters: &VendorFilters) -> bool {
    !vendor_this(relative) || is_split_buck_file(relative, filters.buck_file_name.as_deref())
}

fn gitignore_excluded(pkgdir: &Path, relative: &Path, filter: Option<&ChecksumFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter
            .gitignore
            .matched_path_or_any_parents(pkgdir.join(relative), false)
            .is_ignore()
    })
}

fn path_key(path: &Path) -> anyhow::Result<String> {
    let Some(path) = path.to_str() else {
        bail!("non-UTF8 vendor path {}", path.display());
    };
    Ok(path.replace('\\', "/"))
}

fn materialize_expected_crate(
    expected: &ExpectedCrate,
    staging_dst: &Path,
    filters: &VendorFilters,
) -> anyhow::Result<()> {
    match &expected.materialization {
        Materialization::RegistryArchive { archive } => {
            let include = |rel: &Path| !materialization_excluded(rel, filters);
            unpack_package_archive(archive, staging_dst, &include).with_context(|| {
                format!(
                    "failed to unpack {} into {}",
                    archive.display(),
                    staging_dst.display(),
                )
            })?;
        }
        Materialization::CopyFiles {
            src_root,
            file_paths,
            normalized_cargo_toml,
        } => {
            copy_vendor_sources(
                src_root,
                file_paths,
                normalized_cargo_toml.as_deref(),
                staging_dst,
                filters,
            )?;
        }
    }
    postprocess_vendored_crate_dir(
        staging_dst,
        &expected.pkgdir,
        filters,
        expected.pkg_cksum.as_deref(),
    )
}

fn tree_fingerprint(
    root: &Path,
    pkgdir: &Path,
    filters: &VendorFilters,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let mut entries = BTreeMap::new();
    for entry in WalkDir::new(root).into_iter() {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("walkdir entry must be under root");
        if source_excluded(pkgdir, relative, filters) {
            continue;
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let fingerprint = if file_type.is_file() {
            TreeEntryFingerprint::File(file_sha256(path)?)
        } else if file_type.is_symlink() {
            let target = fs::read_link(path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            TreeEntryFingerprint::Symlink(target.to_string_lossy().into_owned())
        } else {
            bail!("unsupported vendor tree entry type at {}", path.display());
        };
        entries.insert(relative, fingerprint);
    }
    Ok(entries)
}

/// Returns `true` if this relative path should be included in the vendor dir.
///
/// Excludes VCS bookkeeping files anywhere in the package. Cargo's own helper
/// only checks the package root, but fbsource does not track these files in
/// vendored third-party trees, so treating them as source would make clean
/// checkouts fail the fast no-op comparison.
fn vendor_this(relative: &Path) -> bool {
    !relative.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".gitattributes" | ".gitignore" | ".git" | ".cargo-ok")
        )
    })
}

fn prepare_git_cargo_toml_for_vendor(
    pkg: &Package,
    file_paths: &[PathBuf],
    src_root: &Path,
    gctx: &cargo::GlobalContext,
) -> anyhow::Result<String> {
    let packaged_files = file_paths
        .iter()
        .map(|path| {
            path.strip_prefix(src_root)
                .with_context(|| format!("{} is not under {}", path.display(), src_root.display()))
                .map(Path::to_path_buf)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let vendored_pkg = prepare_package_for_vendor(pkg, &packaged_files, gctx)?;
    vendored_pkg
        .manifest()
        .to_normalized_contents()
        .context("failed to render prepared Cargo.toml")
}

fn prepare_package_for_vendor(
    pkg: &Package,
    packaged_files: &[PathBuf],
    gctx: &cargo::GlobalContext,
) -> cargo::CargoResult<Package> {
    let contents = pkg.manifest().contents();
    let document = pkg.manifest().document();
    let original_toml = {
        let mut manifest = pkg.manifest().normalized_toml().clone();
        {
            let package = manifest
                .package
                .as_mut()
                .expect("vendored manifests must have packages");
            if let Some(custom_build_scripts) =
                package.normalized_build().expect("previously normalized")
            {
                let mut included_scripts = Vec::new();
                for script in custom_build_scripts {
                    let path = normalize_manifest_path(Path::new(script));
                    if packaged_files.contains(&path) {
                        let path = path
                            .into_os_string()
                            .into_string()
                            .map_err(|_err| anyhow::format_err!("non-UTF8 `package.build`"))?;
                        included_scripts.push(cargo::util::toml::normalize_path_string_sep(path));
                    } else {
                        gctx.shell().warn(format!(
                            "ignoring `package.build` entry `{}` as it is not included in the published package",
                            path.display()
                        ))?;
                    }
                }
                package.build = Some(match included_scripts.len() {
                    0 => toml::Value::Boolean(false).try_into()?,
                    1 => toml::Value::String(included_scripts[0].clone()).try_into()?,
                    _ => toml::Value::Array(
                        included_scripts
                            .into_iter()
                            .map(toml::Value::String)
                            .collect(),
                    )
                    .try_into()?,
                });
            }
        }

        manifest.lib = if let Some(target) = &manifest.lib {
            cargo::util::toml::prepare_target_for_publish(
                target,
                Some(packaged_files),
                "library",
                gctx,
            )?
        } else {
            None
        };
        manifest.bin = cargo::util::toml::prepare_targets_for_publish(
            manifest.bin.as_ref(),
            Some(packaged_files),
            "binary",
            gctx,
        )?;
        manifest.example = cargo::util::toml::prepare_targets_for_publish(
            manifest.example.as_ref(),
            Some(packaged_files),
            "example",
            gctx,
        )?;
        manifest.test = cargo::util::toml::prepare_targets_for_publish(
            manifest.test.as_ref(),
            Some(packaged_files),
            "test",
            gctx,
        )?;
        manifest.bench = cargo::util::toml::prepare_targets_for_publish(
            manifest.bench.as_ref(),
            Some(packaged_files),
            "benchmark",
            gctx,
        )?;

        manifest
    };
    let normalized_toml = original_toml.clone();
    let features = pkg.manifest().unstable_features().clone();
    let workspace_config = pkg.manifest().workspace_config().clone();
    let source_id = pkg.package_id().source_id();
    let mut warnings = Default::default();
    let mut errors = Default::default();
    let manifest = cargo::util::toml::to_real_manifest(
        contents.to_owned(),
        document.clone(),
        original_toml,
        normalized_toml,
        features,
        workspace_config,
        source_id,
        pkg.manifest_path(),
        pkg.manifest().is_embedded(),
        gctx,
        &mut warnings,
        &mut errors,
    )?;
    Ok(Package::new(manifest, pkg.manifest_path()))
}

fn normalize_manifest_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut normalized = if let Some(component @ Component::Prefix(..)) = components.peek().cloned()
    {
        components.next();
        PathBuf::from(component.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => normalized.push(Component::RootDir),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.ends_with(Component::ParentDir) {
                    normalized.push(Component::ParentDir);
                } else {
                    let popped = normalized.pop();
                    if !popped && !normalized.has_root() {
                        normalized.push(Component::ParentDir);
                    }
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }

    normalized
}

fn make_staging_destination(dst: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let parent = dst
        .parent()
        .context("vendor destination must have a parent")?;
    let leaf = dst
        .file_name()
        .context("vendor destination must have a file name")?;
    let suffix = STAGING_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging_root = parent.join(format!(
        ".reindeer-staging-{}-{}",
        std::process::id(),
        suffix,
    ));
    let _ = fs::remove_dir_all(&staging_root);
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("failed to create {}", staging_root.display()))?;
    Ok((staging_root.clone(), staging_root.join(leaf)))
}

fn replace_vendor_dir(staged_dst: &Path, dst: &Path) -> anyhow::Result<()> {
    remove_existing_path(dst)?;
    fs::rename(staged_dst, dst).with_context(|| {
        format!(
            "failed to move staged vendor dir {} into {}",
            staged_dst.display(),
            dst.display(),
        )
    })?;
    Ok(())
}

/// Walk a directory and compute SHA256 checksums for all regular files.
///
/// Files matched by checksum globs are left on disk but omitted from the
/// returned map. VCS bookkeeping files and configured gitignore matches are
/// treated as absent from the vendored source tree.
fn compute_dir_checksums_filtered(
    root: &Path,
    pkgdir: &Path,
    filter: Option<&ChecksumFilter>,
) -> anyhow::Result<BTreeMap<String, String>> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry)),
            Ok(_) => None,
            Err(err) => Some(Err(anyhow::Error::from(err))),
        })
        .map(|entry| {
            let e = entry?;
            let path = e.path();
            let relative = path
                .strip_prefix(root)
                .expect("walkdir entry must be under root");
            let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");
            if key == ".cargo-checksum.json" {
                return Ok(None);
            }
            if !vendor_this(relative) || gitignore_excluded(pkgdir, relative, filter) {
                log::trace!("checksum: skipping source-excluded file {}", key);
                return Ok(None);
            }
            if checksum_excluded(pkgdir, relative, &key, filter) {
                log::trace!("checksum: skipping excluded file {}", key);
                return Ok(None);
            }
            let contents = fs::read(path)?;
            let hash = bytes_sha256(&contents);
            Ok(Some((key, hash)))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

/// Copy source files from a non-registry package into the vendor directory.
///
/// BUCK files (when `filters.buck_file_name` is set) and VCS bookkeeping files
/// are excluded from the copy entirely. Files matched by checksum globs or
/// configured gitignore rules are copied to disk and handled later when
/// `.cargo-checksum.json` is rewritten.
fn copy_vendor_sources(
    src_root: &Path,
    file_paths: &[PathBuf],
    normalized_cargo_toml: Option<&str>,
    dst: &Path,
    filters: &VendorFilters,
) -> anyhow::Result<()> {
    for src_path in file_paths {
        let relative = src_path.strip_prefix(src_root).with_context(|| {
            format!("{} is not under {}", src_path.display(), src_root.display(),)
        })?;

        if materialization_excluded(relative, filters) {
            continue;
        }

        // Build destination preserving directory structure.
        let dst_path = relative
            .iter()
            .fold(dst.to_path_buf(), |acc, component| acc.join(component));

        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");
        let contents = if key == "Cargo.toml" {
            match normalized_cargo_toml {
                Some(contents) => contents.as_bytes().to_vec(),
                None => fs::read(src_path)
                    .with_context(|| format!("failed to read {}", src_path.display()))?,
            }
        } else {
            fs::read(src_path).with_context(|| format!("failed to read {}", src_path.display()))?
        };
        write_vendored_source_file(
            #[cfg(unix)]
            src_path,
            &dst_path,
            &contents,
            #[cfg(unix)]
            {
                key == "Cargo.toml" && normalized_cargo_toml.is_some()
            },
        )?;
    }

    Ok(())
}

fn write_vendored_source_file(
    #[cfg(unix)] src_path: &Path,
    dst_path: &Path,
    contents: &[u8],
    #[cfg(unix)] generated: bool,
) -> anyhow::Result<()> {
    prepare_regular_file_target(dst_path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if !generated {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;

        let mode = fs::metadata(src_path)
            .with_context(|| format!("failed to stat {}", src_path.display()))?
            .mode();
        options.mode(mode);
    }

    let mut file = options
        .open(dst_path)
        .with_context(|| format!("failed to write {}", dst_path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", dst_path.display()))
}

fn remove_split_buck_file(crate_dir: &Path, buck_file_name: Option<&str>) -> anyhow::Result<()> {
    let Some(buck_file_name) = buck_file_name else {
        return Ok(());
    };
    let buck_path = crate_dir.join(buck_file_name);
    remove_existing_path(&buck_path)
}

/// Synthesize a stub `build.rs` if the manifest declares a build script path
/// that does not exist on disk.
///
/// Workaround for [cargo#14348]: some crates declare a non-standard build
/// script path in `Cargo.toml` but omit the file from the published tarball.
/// Without the stub, cargo refuses to compile the crate.
fn synthesize_missing_build_rs(crate_dir: &Path) -> anyhow::Result<()> {
    type TomlManifest = cargo_toml::Manifest<serde::de::IgnoredAny>;

    let cargo_toml_path = crate_dir.join("Cargo.toml");
    let content = match fs::read_to_string(&cargo_toml_path) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("Failed to read {}: {}", cargo_toml_path.display(), err);
            return Ok(());
        }
    };

    let manifest: TomlManifest = match toml::from_str(&content) {
        Ok(m) => m,
        Err(err) => {
            log::warn!(
                "Failed to deserialize {}: {}",
                cargo_toml_path.display(),
                err
            );
            return Ok(());
        }
    };

    if let Some(package) = &manifest.package {
        if let Some(OptionalFile::Path(build_script_path)) = &package.build {
            let expected = crate_dir.join(normalize_manifest_path(build_script_path));
            if !expected.try_exists()? {
                log::trace!("Synthesizing build script {}", expected.display());
                if let Some(parent) = expected.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(expected, SYNTHESIZED_BUILD_RS)?;
            }
        }
    }

    Ok(())
}

pub(crate) fn postprocess_vendored_crate_dir(
    crate_dir: &Path,
    pkgdir: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<()> {
    remove_split_buck_file(crate_dir, filters.buck_file_name.as_deref())?;
    remove_existing_path(&crate_dir.join(".cargo-checksum.json"))?;
    synthesize_missing_build_rs(crate_dir)?;
    let file_cksums =
        compute_dir_checksums_filtered(crate_dir, pkgdir, filters.checksum_filter.as_ref())?;
    write_checksum_json(crate_dir, pkg_cksum, &file_cksums)
}

pub(crate) fn postprocess_vendored_directory(
    third_party_dir: &Path,
    filters: &VendorFilters,
) -> anyhow::Result<()> {
    let vendor_dir = third_party_dir.join("vendor");
    if !vendor_dir.try_exists()? {
        return Ok(());
    }

    for entry in fs::read_dir(&vendor_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let crate_dir = entry.path();
        let pkgdir = crate_dir.strip_prefix(third_party_dir).with_context(|| {
            format!(
                "vendored crate {} is not under {}",
                crate_dir.display(),
                third_party_dir.display(),
            )
        })?;
        let package_checksum = read_existing_package_checksum(&crate_dir)
            .with_context(|| format!("failed to read {}", crate_dir.display()))?;

        postprocess_vendored_crate_dir(&crate_dir, pkgdir, filters, package_checksum.as_deref())
            .with_context(|| format!("failed to post-process {}", crate_dir.display()))?;
    }

    Ok(())
}

/// Write `.cargo-checksum.json` into a vendored crate directory.
fn write_checksum_json(
    dst: &Path,
    pkg_cksum: Option<&str>,
    file_cksums: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let cksum_path = dst.join(".cargo-checksum.json");
    write_regular_file(&cksum_path, &checksum_json_bytes(pkg_cksum, file_cksums)?)
        .with_context(|| format!("failed to write {}", cksum_path.display()))
}

fn checksum_json_bytes(
    pkg_cksum: Option<&str>,
    file_cksums: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::json!({
        "package": pkg_cksum,
        "files": file_cksums,
    });
    Ok(json.to_string().into_bytes())
}

/// Generate a `.cargo/config.toml` string with source replacement entries
/// that point all resolved sources to the `vendored-sources` directory.
fn generate_vendor_config(
    sources: &BTreeSet<SourceId>,
    _vendor_dir: &Path,
) -> anyhow::Result<String> {
    let mut remap = RemapConfig::default();
    let merged = "vendored-sources";

    for sid in sources {
        let name = if sid.is_crates_io() {
            CRATES_IO_REGISTRY.to_string()
        } else {
            sid.without_precise().as_url().to_string()
        };

        let source = if sid.is_crates_io() {
            RemapSource {
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_remote_registry() {
            RemapSource {
                registry: Some(sid.url().to_string()),
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_git() {
            let mut branch = None;
            let mut tag = None;
            let mut rev = None;
            if let Some(reference) = sid.git_reference() {
                match reference {
                    GitReference::Branch(b) => branch = Some(b.clone()),
                    GitReference::Tag(t) => tag = Some(t.clone()),
                    GitReference::Rev(r) => rev = Some(r.clone()),
                    GitReference::DefaultBranch => {}
                }
            }
            RemapSource {
                git: Some(sid.url().to_string()),
                branch,
                tag,
                rev,
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else {
            anyhow::bail!("unsupported source type: {}", sid);
        };

        remap.sources.insert(name, source);
    }

    // Always write [source.vendored-sources] so that is_vendored() returns true
    // even for workspaces with only path dependencies (where sources is empty).
    remap.sources.insert(
        merged.to_owned(),
        RemapSource {
            directory: Some(PathBuf::from("vendor")),
            ..RemapSource::default()
        },
    );

    toml::to_string(&remap).context("failed to serialize vendor config")
}

fn get_rustc(config: &Config, args: &Args) -> Option<PathBuf> {
    // Priority:
    // 1. `--rustc-path` arg.
    // 2. `RUSTC` env.
    // 3. `reindeer.toml` config value.

    if let Some(rustc_path) = args.rustc_path.as_ref() {
        return Some(rustc_path.clone());
    }

    if let Some(rustc_path) = env::var_os("RUSTC") {
        // Any relative RUSTC path in Reindeer's env is meant to be interpreted
        // relative to the directory Reindeer is running in, which is different
        // from the one we are about to invoke Cargo in. Patch up the RUSTC in
        // the env.
        if Path::new(&rustc_path).components().nth(1).is_some() {
            if let Ok(current_dir) = env::current_dir() {
                let rustc_path = current_dir.join(rustc_path);
                return Some(rustc_path);
            }
        }
    }

    if let Some(bin) = config.cargo.rustc.as_ref() {
        return Some(config.config_dir.join(bin));
    }

    None
}

// Run a cargo command
pub(crate) fn run_cargo(
    config: &Config,
    cargo_home: Option<&Path>,
    current_dir: Option<&Path>,
    args: &Args,
    opts: &[&str],
) -> anyhow::Result<String> {
    let mut cmdline: Vec<_> = args
        .cargo_options
        .iter()
        .map(String::as_str)
        .chain(opts.iter().cloned())
        .collect();
    let mut envs = Vec::new();
    if config.cargo.bindeps {
        cmdline.push("-Zbindeps");
        envs.push(("RUSTC_BOOTSTRAP", "1"));
    }

    log::debug!(
        "Running Cargo command {:?} in dir {:?}",
        cmdline,
        current_dir
    );

    let mut cargo_command = if let Some(cargo_path) = args.cargo_path.as_ref() {
        Command::new(cargo_path)
    } else if let Some(bin) = config.cargo.cargo.as_ref() {
        Command::new(config.config_dir.join(bin))
    } else {
        Command::new("cargo")
    };

    if let Some(rustc_path) = get_rustc(config, args) {
        cargo_command.env("RUSTC", rustc_path);
    }

    if let Some(cargo_home) = cargo_home {
        cargo_command.env("CARGO_HOME", cargo_home);
    }

    if let Some(current_dir) = current_dir {
        cargo_command.current_dir(current_dir);
    }

    cargo_command
        .args(&cmdline)
        .envs(envs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cargo_command
        .spawn()
        .with_context(|| format!("Failed to execute `{:?}`", cargo_command))?;

    let stdout_thr = thread::spawn({
        let stdout = BufReader::new(child.stdout.take().unwrap());
        move || {
            let mut buf = String::new();
            for line in stdout.lines() {
                let line = line.expect("malformed stdout from cargo");
                log::trace!("STDOUT: {}", line);
                buf += &line;
                buf += "\n";
            }
            buf
        }
    });
    let stderr_thr = thread::spawn({
        let stderr = BufReader::new(child.stderr.take().unwrap());
        move || {
            let mut buf = String::new();
            for line in stderr.lines() {
                let line = line.expect("malformed stderr from cargo");
                log::trace!("cargo: {}", line);
                buf += &line;
                buf += "\n";
            }
            buf
        }
    });

    let stdout = stdout_thr.join().expect("stdout thread join failed");
    let stderr = stderr_thr.join().expect("stderr thread join failed");

    if !child.wait()?.success() {
        anyhow::bail!("`{:?}` failed:\n{}", cargo_command, stderr);
    }

    Ok(stdout)
}

// Run a cargo command, assuming it returns a json output of some form.
pub(crate) fn run_cargo_json<T: DeserializeOwned>(
    config: &Config,
    cargo_home: Option<&Path>,
    current_dir: Option<&Path>,
    args: &Args,
    opts: &[&str],
) -> anyhow::Result<T> {
    let json = run_cargo(config, cargo_home, current_dir, args, opts).context("running cargo")?;

    let res = serde_json::from_str::<T>(&json).context("deserializing json")?;

    Ok(res)
}

fn deserialize_default_from_null<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

/// Top-level structure from `cargo metadata`
#[derive(Debug, Deserialize)]
pub struct Metadata {
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub packages: Vec<Manifest>,
    #[serde(with = "As::<Vec<PackageIdFromSpec>>")]
    pub workspace_members: Vec<PackageId>,
    /// Resolved dependency graph
    pub resolve: Resolve,
}

/// Package manifest
// https://doc.rust-lang.org/cargo/reference/manifest.html#the-package-section
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Package name
    pub name: String,
    /// Package version
    pub version: semver::Version,
    /// Canonical ID for package
    #[serde(with = "As::<PackageIdFromSpec>")]
    pub id: PackageId,
    /// SPDX license expression
    pub license: Option<String>,
    /// Path to license
    pub license_file: Option<PathBuf>,
    /// Package description
    pub description: Option<String>,
    /// Source registry for package
    pub source: Source,
    /// Package dependencies (unresolved)
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub dependencies: Vec<ManifestDep>,
    /// Targets in package
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub targets: Vec<ManifestTarget>,
    /// The `[features]` section
    pub features: BTreeMap<String, Vec<String>>,
    /// Path to Cargo.toml
    pub manifest_path: PathBuf,
    /// List of authors
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub authors: Vec<String>,
    /// Path to README file
    pub readme: Option<String>,
    /// Source repository
    pub repository: Option<String>,
    /// Crate's dedicated website that is not just the source repository or docs
    pub homepage: Option<String>,
    /// Default edition for the package (if targets don't have it)
    pub edition: Edition,
    /// Name of a native library that the build script links
    pub links: Option<String>,
    /// Minimum supported rustc version
    pub rust_version: Option<String>,
}

impl Manifest {
    /// Find the target that other packages can depend on. There's supposed to be at most one.
    pub fn dependency_target(&self) -> Option<&ManifestTarget> {
        self.targets
            .iter()
            .find(|tgt| tgt.kind_lib() || tgt.kind_proc_macro())
    }

    /// Return full path to manifest dir (ie, top of package)
    pub fn manifest_dir(&self) -> &Path {
        self.manifest_path.parent().unwrap()
    }
}

impl Eq for Manifest {}
impl PartialEq for Manifest {
    fn eq(&self, other: &Manifest) -> bool {
        self.id.eq(&other.id)
    }
}

impl Ord for Manifest {
    fn cmp(&self, other: &Manifest) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for Manifest {
    fn partial_cmp(&self, other: &Manifest) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Display for Manifest {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}-{}", self.name, self.version)
    }
}

/// Package dependency (unresolved)
#[derive(Debug, Deserialize)]
pub struct ManifestDep {
    /// Dependency name
    pub name: String,
    /// Dependency version requirement
    pub req: VersionReq,
    /// If renamed, local name for dependency
    pub rename: Option<String>,
    /// Dependency kind ("dev", "build" or "normal")
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: DepKind,
    /// Whether dependency is optional or not
    pub optional: bool,
    /// Whether or not to use default features
    pub uses_default_features: bool,
    /// Set of (additional) features
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub features: BTreeSet<String>,
    /// An "artifact dependency" for depending on a binary instead of (or in
    /// addition to) a package's library crate
    pub artifact: Option<ArtifactDep>,
    /// Target platform for target-specific dependencies
    pub target: Option<PlatformExpr>,
}

/// Kind of dependency
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DepKind {
    /// Normal dependency
    #[default]
    Normal,
    /// Dev dependency
    Dev,
    /// Build dependency
    Build,
}

#[derive(Debug, Deserialize)]
pub struct ArtifactDep {
    pub kinds: Vec<ArtifactKind>,
    pub lib: bool,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub enum ArtifactKind {
    Bin(String),
    EveryBin,
    Staticlib,
    Cdylib,
}

impl<'de> Deserialize<'de> for ArtifactKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ArtifactKindVisitor;

        impl<'de> Visitor<'de> for ArtifactKindVisitor {
            type Value = ArtifactKind;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("artifact kind")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "bin" => return Ok(ArtifactKind::EveryBin),
                    "staticlib" => return Ok(ArtifactKind::Staticlib),
                    "cdylib" => return Ok(ArtifactKind::Cdylib),
                    _ => {}
                }
                if let Some(specific_bin) = value.strip_prefix("bin:") {
                    return Ok(ArtifactKind::Bin(specific_bin.to_owned()));
                }
                Err(serde::de::Error::invalid_value(
                    Unexpected::Str(value),
                    &self,
                ))
            }
        }

        deserializer.deserialize_str(ArtifactKindVisitor)
    }
}

/// Package build target
#[derive(Debug, Deserialize, PartialEq)]
pub struct ManifestTarget {
    /// Name of target (crate name)
    pub name: String,
    /// Kind of build(s?) target is for
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: BTreeSet<TargetKind>,
    /// Crate types generate by target
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub crate_types: BTreeSet<CrateType>,
    /// Absolute path to root source file of target
    pub src_path: PathBuf,
    /// Edition, defaulting to package edition
    pub edition: Option<Edition>,
    /// Required features
    #[serde(deserialize_with = "deserialize_default_from_null")]
    #[serde(rename = "required-features")]
    #[serde(default)]
    pub required_features: BTreeSet<String>,
    /// Target has doctests that `cargo test` should extract and build/run
    pub doctest: bool,
}

impl ManifestTarget {
    pub fn kind(&self) -> TargetKind {
        *self.kind.iter().next().expect("need at least one kind")
    }

    pub fn kind_bench(&self) -> bool {
        self.kind.contains(&TargetKind::Bench)
    }

    pub fn kind_bin(&self) -> bool {
        self.kind.contains(&TargetKind::Bin)
    }

    pub fn kind_rlib(&self) -> bool {
        self.kind.contains(&TargetKind::Rlib)
    }

    pub fn kind_lib(&self) -> bool {
        self.kind_rlib() || self.kind.contains(&TargetKind::Lib)
    }

    pub fn kind_proc_macro(&self) -> bool {
        self.kind.contains(&TargetKind::ProcMacro)
    }

    pub fn kind_custom_build(&self) -> bool {
        self.kind.contains(&TargetKind::CustomBuild)
    }

    pub fn kind_test(&self) -> bool {
        self.kind.contains(&TargetKind::Test)
    }

    pub fn kind_example(&self) -> bool {
        self.kind.contains(&TargetKind::Example)
    }

    pub fn kind_staticlib(&self) -> bool {
        self.kind.contains(&TargetKind::Staticlib)
    }

    pub fn kind_cdylib(&self) -> bool {
        self.kind.contains(&TargetKind::Cdylib)
    }

    pub fn crate_rlib(&self) -> bool {
        self.crate_types.contains(&CrateType::Rlib)
    }

    pub fn crate_dylib(&self) -> bool {
        self.crate_types.contains(&CrateType::Dylib)
    }

    pub fn crate_staticlib(&self) -> bool {
        self.crate_types.contains(&CrateType::Staticlib)
    }

    pub fn crate_cdylib(&self) -> bool {
        self.crate_types.contains(&CrateType::Cdylib)
    }

    pub fn crate_lib(&self) -> bool {
        self.crate_rlib() || self.crate_dylib() || self.crate_types.contains(&CrateType::Lib)
    }

    pub fn crate_bin(&self) -> bool {
        self.crate_types.contains(&CrateType::Bin)
    }

    pub fn crate_proc_macro(&self) -> bool {
        self.crate_types.contains(&CrateType::ProcMacro)
    }
}

/// Resolved dependencies
#[derive(Debug, Deserialize)]
pub struct Resolve {
    /// Root package of the workspace
    #[serde(with = "As::<Option<PackageIdFromSpec>>")]
    pub root: Option<PackageId>,
    pub nodes: Vec<Node>,
}

/// Resolved dependencies for a particular package
#[derive(Debug, Deserialize)]
pub struct Node {
    /// Package
    #[serde(with = "As::<PackageIdFromSpec>")]
    pub id: PackageId,
    /// Dependencies with rename information
    pub deps: Vec<NodeDep>,
}

/// Resolved dependencies with rename information
#[derive(Debug, Deserialize)]
pub struct NodeDep {
    /// Package id for dependency
    #[serde(with = "As::<PackageIdFromSpec>")]
    pub pkg: PackageId,
    /// Local manifest's name for the dependency. Deprecated -- if using `-Z
    /// bindeps`, use the `extern_name` from `NodeDepKind` instead of this.
    #[serde(deserialize_with = "deserialize_empty_string_as_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub dep_kinds: Vec<NodeDepKind>,
}

fn deserialize_empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;

    // Cargo metadata contains "name":"" for artifact dependencies.
    if string.is_empty() {
        Ok(None)
    } else {
        Ok(Some(string))
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Hash)]
pub struct NodeDepKind {
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: DepKind,
    /// Platform config
    pub target: Option<PlatformExpr>,

    // vvvvv The fields below are introduced for `-Z bindeps`.
    /// Artifact's crate type, e.g. staticlib, cdylib, bin...
    pub artifact: Option<ArtifactKind>,
    /// What the manifest calls the crate.
    pub extern_name: Option<String>,
    /// Equivalent to `{ target = "..." }` in an artifact dependency requirement.
    pub compile_target: Option<String>,
    /// Executable name for an artifact binary dependency.
    pub bin_name: Option<String>,
    // ^^^^^ The fields above are introduced for `-Z bindeps`.
}

impl NodeDepKind {
    // The dep_kind assumed for `extra_deps` fixups.
    pub const ORDINARY: Self = NodeDepKind {
        kind: DepKind::Normal,
        target: None,
        artifact: None,
        extern_name: None,
        compile_target: None,
        bin_name: None,
    };

    pub fn target_req(&self) -> TargetReq<'_> {
        match self.artifact {
            None => TargetReq::Lib,
            Some(ArtifactKind::Bin(_) | ArtifactKind::EveryBin) => {
                if let Some(bin_name) = &self.bin_name {
                    TargetReq::Bin(bin_name)
                } else {
                    panic!("missing bin_name for artifact dependency {self:?}");
                }
            }
            Some(ArtifactKind::Staticlib) => TargetReq::Staticlib,
            Some(ArtifactKind::Cdylib) => TargetReq::Cdylib,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TargetReq<'a> {
    Lib,
    Bin(&'a str),
    EveryBin,
    BuildScript,
    Staticlib,
    Cdylib,
    Sources,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[expect(dead_code)]
pub enum CompileMode {
    Build,
    RunCustomBuild,
    Test,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum CrateType {
    Bin,
    Dylib,
    Lib,
    ProcMacro,
    Rlib,
    Staticlib,
    Cdylib,
}

#[derive(
    Copy,
    Clone,
    Debug,
    Serialize,
    Deserialize,
    Eq,
    PartialEq,
    Ord,
    PartialOrd
)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Bench,
    Bin,
    CustomBuild,
    Example,
    Dylib,
    Lib,
    Rlib,
    ProcMacro,
    Test,
    Staticlib,
    Cdylib,
}

#[derive(Debug, Deserialize)]
#[expect(dead_code)]
pub enum BuildKind {
    Host,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, PartialOrd)]
pub enum Edition {
    #[serde(rename = "2015")]
    Rust2015,
    #[serde(rename = "2018")]
    Rust2018,
    #[serde(rename = "2021")]
    Rust2021,
    #[serde(rename = "2024")]
    Rust2024,
}

impl Display for Edition {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let edition = match self {
            Edition::Rust2015 => "2015",
            Edition::Rust2018 => "2018",
            Edition::Rust2021 => "2021",
            Edition::Rust2024 => "2024",
        };
        fmt.write_str(edition)
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Source {
    Local,
    CratesIo,
    Git { repo: String, commit_hash: String },
    Unrecognized(String),
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let source: Option<String> = Deserialize::deserialize(deserializer)?;
        Ok(match source {
            None => Source::Local,
            Some(source) => parse_source(&source).unwrap_or(Source::Unrecognized(source)),
        })
    }
}

fn parse_source(source: &str) -> Option<Source> {
    if source == "registry+https://github.com/rust-lang/crates.io-index" {
        Some(Source::CratesIo)
    } else if let Some(rest) = source.strip_prefix("git+") {
        // Git sources take one of the following formats:
        //
        //   git+https://github.com/owner/repo.git?branch=patchv1#9f8e7d6c5b4a3210
        //   git+https://github.com/owner/repo.git?rev=9f8e7d6c5b4a3210#9f8e7d6c5b4a3210
        //   git+https://github.com/owner/repo.git?rev=9f8e7d6c5b4a3210
        //   git+https://github.com/owner/repo.git#9f8e7d6c5b4a3210
        //
        // At least one git commit hash is always present, but it may either be
        // in the "?rev=" or in the "#" or both.
        if let Some((address, commit_hash)) = rest.split_once('#') {
            if let Some((repo, _reference)) = address.split_once('?') {
                Some(Source::Git {
                    repo: repo.to_owned(),
                    commit_hash: commit_hash.to_owned(),
                })
            } else {
                Some(Source::Git {
                    repo: address.to_owned(),
                    commit_hash: commit_hash.to_owned(),
                })
            }
        } else if let Some((repo, commit_hash)) = rest.split_once("?rev=") {
            Some(Source::Git {
                repo: repo.to_owned(),
                commit_hash: commit_hash.to_owned(),
            })
        } else {
            None
        }
    } else {
        None
    }
}

struct PackageIdFromSpec;

impl<'de> Visitor<'de> for PackageIdFromSpec {
    type Value = PackageId;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("package id string")
    }

    fn visit_str<E>(self, string: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let spec = cargo::core::PackageIdSpec::parse(string).map_err(serde::de::Error::custom)?;

        let name = InternedString::new(spec.name());

        let Some(version) = spec.version() else {
            return Err(E::custom(format!("missing version for pkgid: {spec}")));
        };

        let mut url = url::Url::parse(string)
            .map_err(|e| E::custom(format!("bad package spec URL {string}: {e}")))?;
        url.set_fragment(None);
        let source_id = cargo::core::SourceId::from_url(url.as_str())
            .map_err(|e| E::custom(format!("bad source URL for pkgid {spec}: {e}")))?;

        Ok(PackageId::new(name, version, source_id))
    }
}

impl<'de> DeserializeAs<'de, PackageId> for PackageIdFromSpec {
    fn deserialize_as<D>(deserializer: D) -> Result<PackageId, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(PackageIdFromSpec)
    }
}

/// Unpacks a `.crate` archive into `dst`, applying `include` to each entry
/// relative to the crate root. Replicates cargo's zip-bomb and path-traversal
/// protections. Size limit: 512 MiB minimum, or 20x the compressed archive
/// size (matching cargo's defaults).
pub(crate) fn unpack_package_archive(
    archive: &Path,
    dst: &Path,
    include: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    let tarball =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let archive_size = tarball.metadata()?.len();
    let size_limit = u64::max(512 * 1024 * 1024, archive_size * 20);

    let gz = flate2::read::GzDecoder::new(LimitReader::new(tarball, size_limit));
    let mut tar = tar::Archive::new(gz);

    let prefix = dst.file_name().expect("dst must have a file name");
    let parent = dst.parent().expect("dst must have a parent directory");

    for entry in tar.entries().context("failed to read archive entries")? {
        let mut entry = entry.context("failed to read archive entry")?;
        let entry_path = entry
            .path()
            .context("failed to read entry path")?
            .into_owned();

        let relative = match entry_path.strip_prefix(prefix) {
            Ok(rel) => rel.to_owned(),
            Err(_) => {
                anyhow::bail!("invalid tarball: entry at {entry_path:?} is not under {prefix:?}",)
            }
        };

        if !include(&relative) {
            continue;
        }

        // Skip `.cargo-ok` -- cargo's unpack-success marker file.
        if entry_path.file_name().is_some_and(|n| n == ".cargo-ok") {
            continue;
        }

        entry
            .unpack_in(parent)
            .with_context(|| format!("failed to unpack `{}`", entry_path.display()))?;
    }

    Ok(())
}

/// A [`Read`] wrapper that enforces a byte-count limit and returns an error
/// when exceeded. Guards against zip-bomb attacks in compressed archives.
/// Matches the behavior of cargo's internal `LimitErrorReader`.
struct LimitReader<R> {
    inner: io::Take<R>,
}

impl<R: Read> LimitReader<R> {
    fn new(r: R, limit: u64) -> Self {
        LimitReader {
            inner: r.take(limit),
        }
    }
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(0) if self.inner.limit() == 0 => {
                Err(io::Error::other("maximum limit reached when reading"))
            }
            e => e,
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::fs;
    use std::io::Cursor;
    use std::io::Read;
    use std::path::Path;
    use std::path::PathBuf;

    use globset::GlobBuilder;
    use globset::GlobSetBuilder;
    use ignore::gitignore::GitignoreBuilder;
    use sha2::Digest;

    use super::ArtifactKind;
    use super::ChecksumFilter;
    use super::CrateType;
    use super::DepKind;
    use super::ExpectedCrate;
    use super::LimitReader;
    use super::ManifestTarget;
    use super::Materialization;
    use super::NodeDepKind;
    use super::Source;
    use super::TargetKind;
    use super::VendorFilters;
    use super::collect_vendor_cleanup_entries;
    use super::compute_dir_checksums_filtered;
    use super::copy_vendor_sources;
    use super::file_sha256;
    use super::find_cached_registry_archive_by_tarball;
    use super::generate_vendor_config;
    use super::is_split_buck_file;
    use super::make_staging_destination;
    use super::parse_source;
    use super::postprocess_vendored_crate_dir;
    use super::read_existing_package_checksum;
    use super::remove_expected_vendor_entries_from_cleanup;
    use super::synthesize_missing_build_rs;
    use super::tree_fingerprint;
    use super::vendor_dir_matches_expected_source;
    use super::vendor_this;
    use super::write_checksum_json;

    // Build a ChecksumFilter that matches a single glob pattern.
    fn glob_filter(pattern: &str) -> ChecksumFilter {
        let mut builder = GlobSetBuilder::new();
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .unwrap(),
        );
        let gitignore = GitignoreBuilder::new("/").build().unwrap();
        ChecksumFilter {
            remove_globs: builder.build().unwrap(),
            gitignore,
        }
    }

    fn gitignore_filter(pattern: &str) -> ChecksumFilter {
        let remove_globs = GlobSetBuilder::new().build().unwrap();
        let mut builder = GitignoreBuilder::new("/");
        builder.add_line(None, pattern).unwrap();
        let gitignore = builder.build().unwrap();
        ChecksumFilter {
            remove_globs,
            gitignore,
        }
    }

    #[test]
    fn test_checksum_excludes_buck_entry() {
        // A BUCK file present in the extracted directory should be excluded
        // from the checksum map (it was skipped at extraction time by the
        // include filter, so it won't be on disk here -- but even if it were,
        // a glob on "BUCK" would exclude it).
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("Cargo.toml"), b"[package]").unwrap();

        // Use a glob filter that matches the BUCK file name.
        let filter = glob_filter("BUCK");
        let pkgdir = std::path::Path::new("vendor/sourdough-starter-1.0.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        // lib.rs and Cargo.toml should be present; BUCK should not be.
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should be in checksum map"
        );
        assert!(
            !cksums.contains_key("BUCK"),
            "BUCK should be excluded from checksum map"
        );
    }

    #[test]
    fn test_checksum_computation_skips_checksum_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join(".cargo-checksum.json"), b"not source").unwrap();

        let cksums =
            compute_dir_checksums_filtered(root, Path::new("vendor/example-0.1.0"), None).unwrap();

        assert!(cksums.contains_key("lib.rs"));
        assert!(
            !cksums.contains_key(".cargo-checksum.json"),
            "checksum metadata is generated, not source content"
        );
    }

    #[test]
    fn test_vendor_dir_matches_expected_source_does_not_trust_checksum_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();
        fs::write(
            src.join("Cargo.toml"),
            r#"[package]
name = "example"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(
            src.join(".cargo-checksum.json"),
            r#"{"package":"package-checksum","files":{"lib.rs":"wrong"}}"#,
        )
        .unwrap();

        fs::write(
            actual.join("Cargo.toml"),
            r#"[package]
name = "example"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: None,
        };
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(&actual, &pkgdir, &filters, Some("package-checksum"))
            .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual.clone(),
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join(".cargo-checksum.json"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&expected, &filters).unwrap(),
            "matching source contents should take the fast no-op path"
        );

        fs::write(
            actual.join(".cargo-checksum.json"),
            r#"{"package":"package-checksum","files":{"lib.rs":"wrong"}}"#,
        )
        .unwrap();

        assert!(
            !vendor_dir_matches_expected_source(&expected, &filters).unwrap(),
            "editing checksum metadata must invalidate the fast no-op path"
        );
    }

    #[test]
    fn test_vendor_dir_matches_expected_source_ignores_gitignore_filtered_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();

        let cargo_toml = r#"[package]
name = "example"
version = "0.1.0"
"#;
        fs::write(src.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(src.join("Cargo.lock"), b"source lockfile\n").unwrap();

        fs::write(actual.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: Some(gitignore_filter("vendor/*/Cargo.lock")),
        };
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(&actual, &pkgdir, &filters, Some("package-checksum"))
            .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual.clone(),
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join("Cargo.lock"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&expected, &filters).unwrap(),
            "gitignore-filtered source files missing from actual should not invalidate no-op"
        );

        fs::write(actual.join("Cargo.lock"), b"actual lockfile\n").unwrap();
        assert!(
            vendor_dir_matches_expected_source(&expected, &filters).unwrap(),
            "gitignore-filtered actual files should not invalidate no-op"
        );
    }

    #[test]
    fn test_vendor_dir_matches_expected_source_normalizes_build_script_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();

        let cargo_toml = r#"[package]
name = "example"
version = "0.1.0"
build = "./build.rs"
"#;
        fs::write(src.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(src.join("build.rs"), b"fn main() {}\n").unwrap();

        fs::write(actual.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(actual.join("build.rs"), b"fn main() {}\n").unwrap();

        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: None,
        };
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(&actual, &pkgdir, &filters, Some("package-checksum"))
            .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual,
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join("build.rs"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&expected, &filters).unwrap(),
            "`./build.rs` in Cargo.toml should match `build.rs` in the vendor tree"
        );
    }

    #[test]
    fn test_tree_fingerprint_ignores_empty_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("empty")).unwrap();

        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: None,
        };
        let fingerprint =
            tree_fingerprint(root, Path::new("vendor/example-0.1.0"), &filters).unwrap();

        assert!(
            !fingerprint.contains_key("empty"),
            "empty directories are not tracked by source control and should not force a refresh"
        );
    }

    #[test]
    fn test_checksum_filter_glob_keeps_file_on_disk() {
        // Files matched by checksum_exclude globs should remain on disk
        // but be absent from the checksum map.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("pancake-stack.h"), b"// header").unwrap();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();

        // Filter that excludes all .h files.
        let filter = glob_filter("*.h");
        let pkgdir = std::path::Path::new("vendor/flux-capacitor-1.21.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("pancake-stack.h"),
            ".h files should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );

        // File must still exist on disk (checksum_exclude only affects the map).
        assert!(
            root.join("pancake-stack.h").exists(),
            ".h file should remain on disk"
        );
    }

    #[test]
    fn test_checksum_filter_gitignore_excludes_source_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml.orig"),
            b"[package]\nname = \"orig\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            b"[package]\nname = \"normalized\"\n",
        )
        .unwrap();

        let filter = gitignore_filter("vendor/*/Cargo.toml.orig");
        let pkgdir = std::path::Path::new("vendor/fb-procfs-0.9.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("Cargo.toml.orig"),
            "gitignore-matched Cargo.toml.orig should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should remain in checksum map"
        );
    }

    #[test]
    fn test_synthesize_missing_build_rs_creates_stub() {
        // When Cargo.toml declares a build script path that does not exist,
        // synthesize_missing_build_rs should create a stub.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "dragon-breath"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();

        assert!(
            !root.join("build.rs").exists(),
            "build.rs should not exist before synthesis"
        );

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        assert!(
            root.join("build.rs").exists(),
            "build.rs should be created by synthesis"
        );
        let stub = fs::read_to_string(root.join("build.rs")).unwrap();
        assert_eq!(
            stub, "fn main() {}\n",
            "stub should contain only a main function"
        );

        let pkgdir = std::path::Path::new("vendor/dragon-breath-0.1.0");
        let cksums =
            compute_dir_checksums_filtered(root, pkgdir, None).expect("checksums computed");
        assert!(
            cksums.contains_key("build.rs"),
            "synthesized build.rs must be included in .cargo-checksum.json"
        );
    }

    #[test]
    fn test_postprocess_vendored_crate_dir_preserves_package_checksum() {
        let dir = tempfile::tempdir().expect("tempdir");
        let crate_dir = dir.path().join("biscuit-1.0.0");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            r#"[package]
name = "biscuit"
version = "1.0.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(crate_dir.join("lib.rs"), "pub fn biscuit() {}\n").unwrap();
        fs::write(crate_dir.join("ignore.h"), "// filtered header\n").unwrap();
        fs::write(crate_dir.join("BUCK"), "rust_library(name=\"biscuit\")\n").unwrap();
        fs::write(
            crate_dir.join(".cargo-checksum.json"),
            r#"{"package":"sha256abc","files":{"Cargo.toml":"old","lib.rs":"old","ignore.h":"old","BUCK":"old"}}"#,
        )
        .unwrap();

        let filters = VendorFilters {
            buck_file_name: Some("BUCK".to_owned()),
            checksum_filter: Some(glob_filter("*.h")),
        };
        let package_checksum = read_existing_package_checksum(&crate_dir).unwrap();
        let pkgdir = Path::new("vendor/biscuit-1.0.0");

        postprocess_vendored_crate_dir(&crate_dir, pkgdir, &filters, package_checksum.as_deref())
            .unwrap();

        assert!(
            !crate_dir.join("BUCK").exists(),
            "split BUCK should be removed before checksums are recomputed"
        );
        assert!(
            crate_dir.join("build.rs").exists(),
            "post-process should synthesize missing build.rs"
        );

        let checksum: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate_dir.join(".cargo-checksum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            checksum.get("package").and_then(|value| value.as_str()),
            Some("sha256abc"),
            "post-process should preserve the existing package checksum"
        );
        let files = checksum
            .get("files")
            .and_then(|value| value.as_object())
            .unwrap();
        assert!(
            files.contains_key("Cargo.toml") && files.contains_key("lib.rs"),
            "kept files should remain in the checksum map"
        );
        assert!(
            files.contains_key("build.rs"),
            "synthesized build.rs should be added to the checksum map"
        );
        assert!(
            !files.contains_key("BUCK")
                && !files.contains_key("ignore.h")
                && !files.contains_key(".cargo-checksum.json"),
            "split BUCK, filtered files, and the old checksum file should all be absent from the rewritten checksum map"
        );
    }

    #[test]
    fn test_synthesize_no_op_when_build_rs_present() {
        // When build.rs already exists, synthesize_missing_build_rs should
        // leave it unchanged.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let original = b"fn main() { println!(\"cargo:rustc-cfg=boomerang\"); }\n";
        fs::write(root.join("build.rs"), original).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "always-comes-back"
version = "1.0.0"
build = "build.rs"
"#,
        )
        .unwrap();

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        let after = fs::read(root.join("build.rs")).unwrap();
        assert_eq!(
            after, original,
            "existing build.rs should not be overwritten"
        );
    }

    #[test]
    fn test_synthesize_missing_build_rs_creates_parent_dirs() {
        // Non-root build script paths should create their missing parent dirs
        // before the stub is written.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "nested-boomerang"
version = "0.1.0"
build = "scripts/build.rs"
"#,
        )
        .unwrap();

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        let nested = root.join("scripts").join("build.rs");
        assert!(nested.exists(), "nested build.rs should be created");
        let stub = fs::read_to_string(nested).unwrap();
        assert_eq!(stub, "fn main() {}\n");
    }

    #[test]
    fn test_copy_vendor_sources_excludes_buck_file() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(src.join("lib.rs"), b"pub fn fell_off_a_truck() {}").unwrap();
        fs::write(src.join("BUCK"), b"rust_library(name=\"fell-off-a-truck\")").unwrap();

        let file_paths = vec![src.join("lib.rs"), src.join("BUCK")];
        let filters = VendorFilters {
            buck_file_name: Some("BUCK".to_owned()),
            checksum_filter: None,
        };

        copy_vendor_sources(src, &file_paths, None, dst, &filters).expect("copy succeeded");

        assert!(
            dst.join("lib.rs").exists(),
            "lib.rs should be copied to dst"
        );
        assert!(
            !dst.join("BUCK").exists(),
            "BUCK should not be written to dst"
        );
    }

    #[test]
    fn test_copy_vendor_sources_gitignore_filter_keeps_source_file() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(
            src.join("Cargo.toml"),
            b"[package]\nname = \"normalized\"\n",
        )
        .unwrap();
        fs::write(src.join("Cargo.toml.orig"), b"[package]\nname = \"orig\"\n").unwrap();

        let file_paths = vec![src.join("Cargo.toml"), src.join("Cargo.toml.orig")];
        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: Some(gitignore_filter("vendor/*/Cargo.toml.orig")),
        };

        copy_vendor_sources(src, &file_paths, None, dst, &filters).expect("copy succeeded");

        assert!(
            dst.join("Cargo.toml.orig").exists(),
            "gitignore-matched Cargo.toml.orig should still be copied to dst"
        );
    }

    #[test]
    fn test_copy_vendor_sources_uses_normalized_cargo_toml() {
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(
            src.join("Cargo.toml"),
            b"[package]\nname.workspace = true\n",
        )
        .unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let file_paths = vec![src.join("Cargo.toml"), src.join("lib.rs")];
        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: None,
        };
        let normalized =
            "# THIS FILE IS AUTOMATICALLY GENERATED BY CARGO\n\n[package]\nname = \"example\"\n";

        copy_vendor_sources(src, &file_paths, Some(normalized), dst, &filters)
            .expect("copy succeeded");

        assert_eq!(
            fs::read_to_string(dst.join("Cargo.toml")).unwrap(),
            normalized
        );
    }

    #[test]
    fn test_generate_vendor_config_uses_relative_vendor_dir() {
        let mut sources = BTreeSet::new();
        sources.insert(
            cargo::core::SourceId::from_url(
                "registry+https://github.com/rust-lang/crates.io-index",
            )
            .expect("valid registry URL"),
        );

        let config =
            generate_vendor_config(&sources, Path::new("/tmp/absolute/path/vendor")).unwrap();

        assert!(
            config.contains("directory = \"vendor\""),
            "config should write a relative vendor directory: {config}"
        );
        assert!(
            !config.contains("/tmp/absolute/path/vendor"),
            "config should not embed an absolute vendor path: {config}"
        );
    }

    #[test]
    fn test_generate_vendor_config_path_only_workspace() {
        // A workspace with only path dependencies has an empty sources set.
        // generate_vendor_config must still emit [source.vendored-sources] so
        // that is_vendored() returns true after fast_vendor() runs.
        let sources = BTreeSet::new();
        let vendor_dir = Path::new("/jellyfish-cove/vendor");
        let config = generate_vendor_config(&sources, vendor_dir).unwrap();
        assert!(
            config.contains("vendored-sources"),
            "config must contain vendored-sources even for path-only workspaces: {config}",
        );
    }

    #[test]
    fn test_parses_source_git() {
        for source in [
            "git+https://github.com/facebookincubator/reindeer?branch=patchv1#abcdef1234567890abcdef1234567890abcdef00",
            "git+https://github.com/facebookincubator/reindeer?rev=abcdef123#abcdef1234567890abcdef1234567890abcdef00",
            "git+https://github.com/facebookincubator/reindeer?rev=abcdef1234567890abcdef1234567890abcdef00",
            "git+https://github.com/facebookincubator/reindeer#abcdef1234567890abcdef1234567890abcdef00",
        ] {
            assert_eq!(
                parse_source(source),
                Some(Source::Git {
                    repo: "https://github.com/facebookincubator/reindeer".to_owned(),
                    commit_hash: "abcdef1234567890abcdef1234567890abcdef00".to_owned(),
                }),
            );
        }
    }

    // Invariant: parse_source recognizes the canonical crates.io registry URL
    #[test]
    fn test_parse_source_crates_io() {
        assert_eq!(
            parse_source("registry+https://github.com/rust-lang/crates.io-index"),
            Some(Source::CratesIo),
        );
    }

    // Invariant: parse_source returns None for unrecognized source formats
    #[test]
    fn test_parse_source_unrecognized() {
        assert_eq!(parse_source(""), None);
        assert_eq!(parse_source("path+file:///local/crate"), None);
        assert_eq!(
            parse_source("registry+https://example.com/custom-registry"),
            None,
        );
    }

    // Invariant: parse_source returns None for git URL with no commit hash anywhere
    #[test]
    fn test_parse_source_git_no_hash() {
        assert_eq!(parse_source("git+https://github.com/owner/repo.git"), None,);
    }

    // Invariant: vendor_this excludes .gitattributes, .gitignore, .git, .cargo-ok
    #[test]
    fn test_vendor_this_excludes_dotfiles() {
        assert!(!vendor_this(Path::new(".gitattributes")));
        assert!(!vendor_this(Path::new(".gitignore")));
        assert!(!vendor_this(Path::new(".git")));
        assert!(!vendor_this(Path::new(".cargo-ok")));
        assert!(!vendor_this(Path::new("nested/.gitattributes")));
        assert!(!vendor_this(Path::new("nested/.gitignore")));
        assert!(!vendor_this(Path::new("nested/.git/config")));
        assert!(!vendor_this(Path::new("nested/.cargo-ok")));
    }

    // Invariant: vendor_this includes normal source files and nested paths
    #[test]
    fn test_vendor_this_includes_normal_files() {
        assert!(vendor_this(Path::new("src/lib.rs")));
        assert!(vendor_this(Path::new("Cargo.toml")));
        assert!(vendor_this(Path::new("README.md")));
        assert!(vendor_this(Path::new("build.rs")));
        assert!(vendor_this(Path::new(".cargo/config.toml")));
    }

    // Invariant: generate_vendor_config emits git source replacement with branch/tag/rev fields
    #[test]
    fn test_generate_vendor_config_git_source() {
        let mut sources = BTreeSet::new();
        let sid =
            cargo::core::SourceId::from_url("git+https://github.com/example/crate.git?branch=main")
                .expect("valid git URL");
        sources.insert(sid);

        let config = generate_vendor_config(&sources, Path::new("/vendor")).unwrap();
        assert!(
            config.contains("git = \"https://github.com/example/crate.git\""),
            "config must contain git URL: {config}",
        );
        assert!(
            config.contains("branch = \"main\""),
            "config must contain branch: {config}",
        );
        assert!(
            config.contains("replace-with = \"vendored-sources\""),
            "config must redirect to vendored-sources: {config}",
        );
    }

    // Invariant: ArtifactKind deserializes "bin" as EveryBin, not Bin("bin")
    #[test]
    fn test_artifact_kind_deserialize_bin() {
        let kind: ArtifactKind = serde_json::from_str("\"bin\"").unwrap();
        assert_eq!(kind, ArtifactKind::EveryBin);
    }

    // Invariant: ArtifactKind deserializes "staticlib" and "cdylib" as named variants
    #[test]
    fn test_artifact_kind_deserialize_staticlib_cdylib() {
        let kind: ArtifactKind = serde_json::from_str("\"staticlib\"").unwrap();
        assert_eq!(kind, ArtifactKind::Staticlib);

        let kind: ArtifactKind = serde_json::from_str("\"cdylib\"").unwrap();
        assert_eq!(kind, ArtifactKind::Cdylib);
    }

    // Invariant: ArtifactKind deserializes "bin:<name>" as Bin(name)
    #[test]
    fn test_artifact_kind_deserialize_named_bin() {
        let kind: ArtifactKind = serde_json::from_str("\"bin:my-tool\"").unwrap();
        assert_eq!(kind, ArtifactKind::Bin("my-tool".to_owned()));
    }

    // Invariant: ArtifactKind deserialization rejects unknown strings
    #[test]
    fn test_artifact_kind_deserialize_invalid() {
        let result = serde_json::from_str::<ArtifactKind>("\"dylib\"");
        assert!(result.is_err());

        let result = serde_json::from_str::<ArtifactKind>("\"unknown\"");
        assert!(result.is_err());
    }

    // Invariant: deserialize_empty_string_as_none maps "" to None and non-empty to Some
    #[test]
    fn test_deserialize_empty_string_as_none() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(deserialize_with = "super::deserialize_empty_string_as_none")]
            value: Option<String>,
        }

        let w: Wrapper = serde_json::from_str(r#"{"value": ""}"#).unwrap();
        assert_eq!(w.value, None);

        let w: Wrapper = serde_json::from_str(r#"{"value": "hello"}"#).unwrap();
        assert_eq!(w.value, Some("hello".to_owned()));
    }

    // Invariant: deserialize_default_from_null maps null to Default::default()
    #[test]
    fn test_deserialize_default_from_null() {
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(deserialize_with = "super::deserialize_default_from_null")]
            items: Vec<String>,
        }

        let w: Wrapper = serde_json::from_str(r#"{"items": null}"#).unwrap();
        assert!(w.items.is_empty());

        let w: Wrapper = serde_json::from_str(r#"{"items": ["a", "b"]}"#).unwrap();
        assert_eq!(w.items, vec!["a", "b"]);
    }

    // Invariant: kind_lib returns true for both Rlib and Lib target kinds
    #[test]
    fn test_manifest_target_kind_lib() {
        let target = ManifestTarget {
            name: "foo".to_owned(),
            kind: BTreeSet::from([TargetKind::Rlib]),
            crate_types: BTreeSet::new(),
            src_path: PathBuf::from("src/lib.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(target.kind_lib());
        assert!(!target.kind_proc_macro());

        let target = ManifestTarget {
            name: "bar".to_owned(),
            kind: BTreeSet::from([TargetKind::Lib]),
            crate_types: BTreeSet::new(),
            src_path: PathBuf::from("src/lib.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(target.kind_lib());
    }

    // Invariant: kind_proc_macro only returns true for ProcMacro kind
    #[test]
    fn test_manifest_target_kind_proc_macro() {
        let target = ManifestTarget {
            name: "derive_foo".to_owned(),
            kind: BTreeSet::from([TargetKind::ProcMacro]),
            crate_types: BTreeSet::from([CrateType::ProcMacro]),
            src_path: PathBuf::from("src/lib.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(target.kind_proc_macro());
        assert!(!target.kind_lib());
    }

    // Invariant: crate_lib returns true for Rlib, Dylib, or Lib crate types
    #[test]
    fn test_manifest_target_crate_lib() {
        let target = ManifestTarget {
            name: "foo".to_owned(),
            kind: BTreeSet::from([TargetKind::Lib]),
            crate_types: BTreeSet::from([CrateType::Rlib]),
            src_path: PathBuf::from("src/lib.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(target.crate_lib());

        let target = ManifestTarget {
            name: "bar".to_owned(),
            kind: BTreeSet::from([TargetKind::Lib]),
            crate_types: BTreeSet::from([CrateType::Dylib]),
            src_path: PathBuf::from("src/lib.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(target.crate_lib());

        let bin_target = ManifestTarget {
            name: "baz".to_owned(),
            kind: BTreeSet::from([TargetKind::Bin]),
            crate_types: BTreeSet::from([CrateType::Bin]),
            src_path: PathBuf::from("src/main.rs"),
            edition: None,
            required_features: BTreeSet::new(),
            doctest: false,
        };
        assert!(!bin_target.crate_lib());
    }

    // Invariant: target_req returns Lib when artifact is None
    #[test]
    fn test_node_dep_kind_target_req_lib() {
        assert_eq!(NodeDepKind::ORDINARY.target_req(), super::TargetReq::Lib);
    }

    // Invariant: target_req returns Staticlib/Cdylib for those artifact kinds
    #[test]
    fn test_node_dep_kind_target_req_staticlib_cdylib() {
        let dep = NodeDepKind {
            kind: DepKind::Normal,
            target: None,
            artifact: Some(ArtifactKind::Staticlib),
            extern_name: None,
            compile_target: None,
            bin_name: None,
        };
        assert_eq!(dep.target_req(), super::TargetReq::Staticlib);

        let dep = NodeDepKind {
            kind: DepKind::Normal,
            target: None,
            artifact: Some(ArtifactKind::Cdylib),
            extern_name: None,
            compile_target: None,
            bin_name: None,
        };
        assert_eq!(dep.target_req(), super::TargetReq::Cdylib);
    }

    // Invariant: target_req returns Bin(name) when artifact is Bin/EveryBin with bin_name
    #[test]
    fn test_node_dep_kind_target_req_bin_with_name() {
        let dep = NodeDepKind {
            kind: DepKind::Normal,
            target: None,
            artifact: Some(ArtifactKind::EveryBin),
            extern_name: None,
            compile_target: None,
            bin_name: Some("my-bin".to_owned()),
        };
        assert_eq!(dep.target_req(), super::TargetReq::Bin("my-bin"));
    }

    // Invariant: target_req panics for Bin/EveryBin without bin_name
    #[test]
    #[should_panic(expected = "missing bin_name")]
    fn test_node_dep_kind_target_req_bin_panics_without_name() {
        let dep = NodeDepKind {
            kind: DepKind::Normal,
            target: None,
            artifact: Some(ArtifactKind::EveryBin),
            extern_name: None,
            compile_target: None,
            bin_name: None,
        };
        dep.target_req();
    }

    // Invariant: LimitReader returns data normally when under the limit
    #[test]
    fn test_limit_reader_under_limit() {
        let data = b"hello world";
        let mut reader = LimitReader::new(Cursor::new(data), 100);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    // Invariant: LimitReader returns an error when the byte limit is reached
    #[test]
    fn test_limit_reader_at_limit_errors() {
        let data = b"hello world";
        let mut reader = LimitReader::new(Cursor::new(data), 5);
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        let result = reader.read(&mut buf);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("maximum limit reached"),
        );
    }

    // Invariant: make_staging_destination creates a unique directory under the parent
    #[test]
    fn test_make_staging_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("my-crate-1.0.0");

        let (staging_root, staging_dst) = make_staging_destination(&dst).unwrap();
        assert!(staging_root.exists());
        assert!(staging_root.starts_with(tmp.path()));
        assert_eq!(staging_dst.file_name().unwrap(), "my-crate-1.0.0");
        assert!(staging_dst.starts_with(&staging_root));

        std::fs::remove_dir_all(&staging_root).unwrap();
    }

    // Invariant: compute_dir_checksums_filtered without a filter produces SHA256 hashes for all
    // files in a tree.
    #[test]
    fn test_compute_dir_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), b"world").unwrap();

        let cksums =
            compute_dir_checksums_filtered(tmp.path(), Path::new("vendor/example-0.1.0"), None)
                .unwrap();
        assert_eq!(cksums.len(), 2);
        assert!(cksums.contains_key("a.txt"));
        assert!(cksums.contains_key("sub/b.txt"));

        let expected_a = format!("{:x}", sha2::Sha256::digest(b"hello"));
        assert_eq!(cksums["a.txt"], expected_a);
    }

    // Invariant: write_checksum_json creates .cargo-checksum.json with package and files fields
    #[test]
    fn test_write_checksum_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".to_owned(), "abc123".to_owned());

        write_checksum_json(tmp.path(), Some("pkg_hash_xyz"), &files).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".cargo-checksum.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["package"], "pkg_hash_xyz");
        assert_eq!(parsed["files"]["src/lib.rs"], "abc123");
    }

    // Invariant: write_checksum_json handles None package checksum as JSON null
    #[test]
    fn test_write_checksum_json_null_package() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::new();

        write_checksum_json(tmp.path(), None, &files).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".cargo-checksum.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["package"].is_null());
    }

    // Invariant: copy_vendor_sources copies files while skipping excluded paths.
    #[test]
    fn test_copy_vendor_sources() {
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("lib.rs"), b"fn main() {}").unwrap();
        std::fs::write(src_dir.path().join(".gitignore"), b"/target").unwrap();
        std::fs::write(src_dir.path().join("Cargo.toml"), b"[package]").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let file_paths = vec![
            src_dir.path().join("lib.rs"),
            src_dir.path().join(".gitignore"),
            src_dir.path().join("Cargo.toml"),
        ];

        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: None,
        };
        copy_vendor_sources(src_dir.path(), &file_paths, None, dst_dir.path(), &filters).unwrap();

        assert!(dst_dir.path().join("lib.rs").exists());
        assert!(!dst_dir.path().join(".gitignore").exists());
        assert!(dst_dir.path().join("Cargo.toml").exists());
    }

    #[test]
    fn test_write_regular_file_replaces_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".cargo-config");
        fs::create_dir(&path).unwrap();

        super::write_regular_file(&path, b"[source]\n").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[source]\n",
            "write_regular_file should replace preexisting directories"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_regular_file_replaces_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        let path = dir.path().join(".cargo-config");
        fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        super::write_regular_file(&path, b"[source]\n").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[source]\n",
            "write_regular_file should replace preexisting symlinks"
        );
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_file(),
            "repaired path should be a regular file"
        );
    }

    #[test]
    fn test_replace_vendor_dir_replaces_preexisting_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        fs::write(&dst, b"old").unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair preexisting files at the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_vendor_dir_replaces_preexisting_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        let target = dir.path().join("target");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair preexisting symlinks at the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_vendor_dir_replaces_dangling_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        let target = dir.path().join("missing-target");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair dangling symlinks at the destination"
        );
    }

    #[test]
    fn test_is_split_buck_file_matches_only_root_configured_name() {
        assert!(is_split_buck_file(Path::new("BUCK"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("src/BUCK"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("BUCK.v2"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("BUCK"), None));
    }

    #[test]
    fn test_find_cached_registry_archive_uses_matching_crate_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cargo_home = dir.path();
        let cache_a = cargo_home.join("registry/cache/index-a");
        let cache_b = cargo_home.join("registry/cache/index-b");
        fs::create_dir_all(&cache_a).unwrap();
        fs::create_dir_all(&cache_b).unwrap();

        let tarball_name = "example-0.1.0.crate";
        fs::write(cache_a.join(tarball_name), b"wrong archive").unwrap();
        let archive = cache_b.join(tarball_name);
        fs::write(&archive, b"right archive").unwrap();
        let checksum = file_sha256(&archive).unwrap();

        assert_eq!(
            find_cached_registry_archive_by_tarball(cargo_home, tarball_name, &checksum).unwrap(),
            Some(archive),
            "the cache lookup should select the archive matching the lockfile checksum"
        );
    }

    #[test]
    fn test_find_cached_registry_archive_treats_checksum_mismatch_as_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("registry/cache/index");
        fs::create_dir_all(&cache).unwrap();

        let tarball_name = "example-0.1.0.crate";
        fs::write(cache.join(tarball_name), b"wrong archive").unwrap();

        assert!(
            find_cached_registry_archive_by_tarball(dir.path(), tarball_name, "expected-checksum")
                .unwrap()
                .is_none(),
            "checksum mismatches should fall back to Cargo package loading"
        );
    }

    #[test]
    fn test_find_cached_registry_archive_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("registry/cache/index")).unwrap();

        assert!(
            find_cached_registry_archive_by_tarball(dir.path(), "missing-0.1.0.crate", "checksum",)
                .unwrap()
                .is_none(),
            "missing cached archives should fall back to Cargo package loading"
        );
    }

    #[test]
    fn test_collect_vendor_cleanup_entries_removes_old_stamp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        fs::write(vendor_dir.join(".reindeer-vendor-stamp"), b"{}").unwrap();
        fs::write(vendor_dir.join(".reindeer-vendor-stamps"), b"{}").unwrap();
        fs::write(vendor_dir.join(".junk"), b"crumbs").unwrap();
        fs::create_dir(vendor_dir.join(".reindeer-staging-123-0")).unwrap();
        fs::create_dir(vendor_dir.join("sourdough-1.0.0")).unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".junk"),
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join(".reindeer-vendor-stamps"),
            vendor_dir.join(".reindeer-staging-123-0"),
            vendor_dir.join("sourdough-1.0.0"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "repair cleanup should remove hidden junk, stale staging dirs, and old stamp files"
        );
    }

    #[test]
    fn test_collect_vendor_cleanup_entries_removes_invalid_bookkeeping_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        fs::create_dir(vendor_dir.join(".reindeer-vendor-stamp")).unwrap();
        fs::create_dir(vendor_dir.join(".reindeer-vendor-stamps")).unwrap();
        fs::write(vendor_dir.join(".junk"), b"crumbs").unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".junk"),
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join(".reindeer-vendor-stamps"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "old bookkeeping directories should be removed as stale entries"
        );
    }

    #[test]
    fn test_remove_expected_vendor_entries_from_cleanup_preserves_split_buck_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        let source_dir = vendor_dir.join("example-0.1.0");
        let split_buck_dir = vendor_dir.join("example");
        let stale_dir = vendor_dir.join("stale-0.1.0");
        let mut to_remove = BTreeSet::from([
            source_dir.clone(),
            split_buck_dir.clone(),
            stale_dir.clone(),
        ]);

        remove_expected_vendor_entries_from_cleanup(
            &mut to_remove,
            vendor_dir,
            &source_dir,
            "example",
            Some("BUCK"),
        );

        assert_eq!(
            to_remove,
            BTreeSet::from([stale_dir]),
            "current source and split BUCK package dirs should survive cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_vendor_cleanup_entries_removes_invalid_bookkeeping_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        let target = vendor_dir.join("outside");
        fs::write(&target, b"crumbs").unwrap();
        std::os::unix::fs::symlink(&target, vendor_dir.join(".reindeer-vendor-stamp")).unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join("outside"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "old bookkeeping symlinks should be treated as drift and removed"
        );
    }
}
