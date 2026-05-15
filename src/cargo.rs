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
use std::iter;
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
use cargo::core::PackageId;
use cargo::core::SourceId;
use cargo::sources::CRATES_IO_REGISTRY;
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

/// A unit of work for parallel vendor extraction.
///
/// Registry crates are extracted from `.crate` archives. Git/other crates
/// need their source files copied individually.
enum VendorWork {
    /// Extract a `.crate` archive into the vendor directory.
    RegistryArchive {
        archive: PathBuf,
        dst: PathBuf,
        pkg_cksum: Option<String>,
    },
    /// Copy individual source files for non-registry packages (git, etc.).
    CopyFiles {
        src_root: PathBuf,
        file_paths: Vec<PathBuf>,
        dst: PathBuf,
        pkg_cksum: Option<String>,
    },
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
    let gctx = make_gctx(
        config,
        args,
        paths,
        GctxProperties {
            frozen: false,
            locked: false,
            offline: false,
            quiet: true,
        },
    )?;

    let source_config = cargo::sources::SourceConfigMap::new(&gctx)?;

    let manifest_path = paths.manifest_path.clone();
    let ws = cargo::core::Workspace::new(&manifest_path, &gctx)?;

    // Resolve deps and download all packages.
    let (package_set, resolve) =
        cargo::ops::resolve_ws(&ws, false).context("failed to resolve workspace")?;

    package_set
        .get_many(resolve.iter())
        .context("failed to download packages")?;

    let vendor_dir = paths.third_party_dir.join("vendor");
    fs::create_dir_all(&vendor_dir)?;

    // Collect existing directories for cleanup (unless --no-delete).
    let mut to_remove: BTreeSet<PathBuf> = if no_delete {
        BTreeSet::new()
    } else {
        fs::read_dir(&vendor_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(|s| !s.starts_with('.')))
            .map(|e| e.path())
            .collect()
    };

    // Collect packages to vendor and build work items.
    let mut work_items = Vec::new();
    let mut sources: BTreeSet<SourceId> = BTreeSet::new();

    for pkg_id in resolve.iter() {
        let replaced_sid = source_config
            .load(pkg_id.source_id(), &Default::default())?
            .replaced_source_id();

        // Skip path dependencies -- they're already in the repo.
        if replaced_sid.is_path() {
            if let Ok(file_path) = replaced_sid.url().to_file_path() {
                to_remove.remove(&file_path);
            }
            continue;
        }

        let pkg = package_set
            .get_one(pkg_id)
            .context("failed to fetch package")?;
        let dst_name = format!("{}-{}", pkg_id.name(), pkg_id.version());
        let dst = vendor_dir.join(&dst_name);
        to_remove.remove(&dst);
        sources.insert(pkg_id.source_id());

        // If this is an immutable registry source and we already have it, skip.
        let cksum_path = dst.join(".cargo-checksum.json");
        if replaced_sid.is_registry() && cksum_path.exists() {
            continue;
        }

        let pkg_cksum = resolve.checksums().get(&pkg_id).and_then(|c| c.clone());

        if replaced_sid.is_registry() {
            // Derive archive path from the package root that cargo downloaded to.
            // After get_many(), pkg.root() is at:
            //   <cargo_home>/registry/src/<encoded>/<name>-<version>
            // We need: <cargo_home>/registry/cache/<encoded>/<tarball_name>
            let src_dir = pkg.root();
            let encoded = src_dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .context("cannot derive encoded registry dir from package root")?;
            let archive = paths
                .cargo_home
                .join("registry")
                .join("cache")
                .join(encoded)
                .join(pkg_id.tarball_name());

            work_items.push(VendorWork::RegistryArchive {
                archive,
                dst,
                pkg_cksum,
            });
        } else {
            // Git or other non-registry sources: list files and copy them.
            let src_root = pkg.root().to_path_buf();
            let file_paths = cargo::sources::PathSource::new(pkg.root(), replaced_sid, &gctx)
                .list_files(pkg)?
                .into_iter()
                .map(|entry| entry.into_path_buf())
                .collect::<Vec<_>>();

            work_items.push(VendorWork::CopyFiles {
                src_root,
                file_paths,
                dst,
                pkg_cksum,
            });
        }
    }

    // Remove stale vendor directories.
    for stale in &to_remove {
        if stale.is_dir() {
            fs::remove_dir_all(stale).with_context(|| {
                format!("failed to remove stale vendor dir {}", stale.display())
            })?;
        }
    }

    // Extract/copy in parallel using thread::scope with static chunking.
    // Filters are shared by reference across threads; ChecksumFilter is Sync.
    let num_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let chunk_size = work_items.len().div_ceil(num_threads.max(1));
    let filters = &filters;

    thread::scope(|s| {
        let handles: Vec<_> = work_items
            .chunks(chunk_size.max(1))
            .map(|chunk| {
                s.spawn(move || {
                    for item in chunk {
                        match item {
                            VendorWork::RegistryArchive {
                                archive,
                                dst,
                                pkg_cksum,
                            } => {
                                let (staging_root, staging_dst) = make_staging_destination(dst)?;
                                let result = (|| -> anyhow::Result<()> {
                                    let buck_file_name = filters.buck_file_name.as_deref();
                                    let include = |rel: &Path| {
                                        vendor_this(rel)
                                            && buck_file_name.is_none_or(|n| rel != Path::new(n))
                                    };
                                    unpack_package_archive(archive, &staging_dst, &include)
                                        .with_context(|| {
                                            format!(
                                                "failed to unpack {} into {}",
                                                archive.display(),
                                                staging_dst.display(),
                                            )
                                        })?;

                                    let pkgdir = dst
                                        .strip_prefix(&paths.third_party_dir)
                                        .expect("dst is always under third_party_dir");
                                    postprocess_vendored_crate_dir(
                                        &staging_dst,
                                        pkgdir,
                                        filters,
                                        pkg_cksum.as_deref(),
                                    )?;
                                    replace_vendor_dir(&staging_dst, dst)?;
                                    Ok(())
                                })();
                                let _ = fs::remove_dir_all(&staging_root);
                                result?;
                            }
                            VendorWork::CopyFiles {
                                src_root,
                                file_paths,
                                dst,
                                pkg_cksum,
                            } => {
                                let (staging_root, staging_dst) = make_staging_destination(dst)?;
                                let result = (|| -> anyhow::Result<()> {
                                    let pkgdir = dst
                                        .strip_prefix(&paths.third_party_dir)
                                        .expect("dst is always under third_party_dir");
                                    copy_vendor_sources(
                                        src_root,
                                        file_paths,
                                        &staging_dst,
                                        filters,
                                        pkgdir,
                                    )?;
                                    postprocess_vendored_crate_dir(
                                        &staging_dst,
                                        pkgdir,
                                        filters,
                                        pkg_cksum.as_deref(),
                                    )?;
                                    replace_vendor_dir(&staging_dst, dst)?;
                                    Ok(())
                                })();
                                let _ = fs::remove_dir_all(&staging_root);
                                result?;
                            }
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
    })?;

    // Generate .cargo/config.toml with source replacements.
    let vendor_config = generate_vendor_config(&sources, &vendor_dir)?;
    fs::create_dir_all(&paths.cargo_home)?;
    fs::write(paths.cargo_home.join("config.toml"), &vendor_config)?;

    Ok(())
}

/// Returns `true` if this relative path should be included in the vendor dir.
///
/// Mirrors cargo's `vendor_this()`: excludes `.gitattributes`, `.gitignore`,
/// `.git`, and `.cargo-ok`.
fn vendor_this(relative: &Path) -> bool {
    match relative.to_str() {
        Some(".gitattributes" | ".gitignore" | ".git") => false,
        Some(".cargo-ok") => false,
        _ => true,
    }
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
    let _ = fs::remove_dir_all(dst);
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
/// Files matched by `filter` are left on disk but omitted from the returned map,
/// so the resulting `.cargo-checksum.json` never includes excluded entries.
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
            if let Some(f) = filter {
                let excluded = f.remove_globs.is_match(&key)
                    || f.gitignore
                        .matched_path_or_any_parents(pkgdir.join(relative), false)
                        .is_ignore();
                if excluded {
                    log::trace!("checksum: skipping excluded file {}", key);
                    return Ok(None);
                }
            }
            let contents = fs::read(path)?;
            let hash = format!("{:x}", sha2::Sha256::digest(&contents));
            Ok(Some((key, hash)))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

/// Copy source files from a non-registry package into the vendor directory,
/// computing per-file SHA256 checksums along the way.
///
/// BUCK files (when `filters.buck_file_name` is set) are excluded from the
/// copy entirely. Files matched by `filters.checksum_filter` are copied to
/// disk but omitted from the returned checksum map.
fn copy_vendor_sources(
    src_root: &Path,
    file_paths: &[PathBuf],
    dst: &Path,
    filters: &VendorFilters,
    pkgdir: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut cksums = BTreeMap::new();

    for src_path in file_paths {
        let relative = src_path.strip_prefix(src_root).with_context(|| {
            format!("{} is not under {}", src_path.display(), src_root.display(),)
        })?;

        if !vendor_this(relative) {
            continue;
        }

        // Exclude BUCK files from the copy entirely (not written to disk or checksum).
        if filters
            .buck_file_name
            .as_deref()
            .is_some_and(|n| relative == Path::new(n))
        {
            continue;
        }

        // Build destination preserving directory structure.
        let dst_path = relative
            .iter()
            .fold(dst.to_path_buf(), |acc, component| acc.join(component));

        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents =
            fs::read(src_path).with_context(|| format!("failed to read {}", src_path.display()))?;
        let hash = sha2::Sha256::digest(&contents);
        fs::write(&dst_path, &contents)
            .with_context(|| format!("failed to write {}", dst_path.display()))?;

        let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");

        // Skip checksum-excluded files: kept on disk, absent from checksum map.
        if let Some(f) = &filters.checksum_filter {
            let excluded = f.remove_globs.is_match(&key)
                || f.gitignore
                    .matched_path_or_any_parents(pkgdir.join(relative), false)
                    .is_ignore();
            if excluded {
                log::trace!("checksum: skipping excluded file {}", key);
                continue;
            }
        }

        cksums.insert(key, format!("{:x}", hash));
    }

    Ok(cksums)
}

fn path_file_type_no_follow(path: &Path) -> anyhow::Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
    }
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
            let expected = crate_dir.join(build_script_path);
            if !expected.try_exists()? {
                log::trace!("Synthesizing build script {}", expected.display());
                if let Some(parent) = expected.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(expected, "fn main() {}\n")?;
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
    let json = serde_json::json!({
        "package": pkg_cksum,
        "files": file_cksums,
    });
    let cksum_path = dst.join(".cargo-checksum.json");
    let contents = json.to_string();
    write_regular_file(&cksum_path, contents.as_bytes())
        .with_context(|| format!("failed to write {}", cksum_path.display()))
}

pub(crate) fn read_existing_package_checksum(crate_dir: &Path) -> anyhow::Result<Option<String>> {
    let cksum_path = crate_dir.join(".cargo-checksum.json");
    let content = fs::read_to_string(&cksum_path)
        .with_context(|| format!("missing or non-regular {}", cksum_path.display()))?;
    let checksum_json: CargoChecksumJson = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", cksum_path.display()))?;
    Ok(checksum_json.package)
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
    use super::LimitReader;
    use super::ManifestTarget;
    use super::NodeDepKind;
    use super::Source;
    use super::TargetKind;
    use super::VendorFilters;
    use super::compute_dir_checksums_filtered;
    use super::copy_vendor_sources;
    use super::generate_vendor_config;
    use super::make_staging_destination;
    use super::parse_source;
    use super::postprocess_vendored_crate_dir;
    use super::read_existing_package_checksum;
    use super::synthesize_missing_build_rs;
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
    fn test_checksum_filter_gitignore_keeps_cargo_toml_orig_on_disk() {
        // Files matched through gitignore rules should remain on disk while
        // being omitted from the checksum map.
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
        assert!(
            root.join("Cargo.toml.orig").exists(),
            "Cargo.toml.orig should remain on disk"
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
        // copy_vendor_sources with buck_file_name set should not write the BUCK
        // file to the destination directory and should not include it in the
        // returned checksum map.
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
        let pkgdir = std::path::Path::new("vendor/fell-off-a-truck-1.0.0");

        let cksums =
            copy_vendor_sources(src, &file_paths, dst, &filters, pkgdir).expect("copy succeeded");

        assert!(
            dst.join("lib.rs").exists(),
            "lib.rs should be copied to dst"
        );
        assert!(
            !dst.join("BUCK").exists(),
            "BUCK should not be written to dst"
        );
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );
        assert!(
            !cksums.contains_key("BUCK"),
            "BUCK should not be in checksum map"
        );
    }

    #[test]
    fn test_copy_vendor_sources_checksum_filter() {
        // copy_vendor_sources with a checksum filter should copy filtered files
        // to disk but omit them from the returned checksum map.
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(src.join("lib.rs"), b"pub fn catapult() {}").unwrap();
        fs::write(src.join("catapult.h"), b"// siege engine header").unwrap();

        let file_paths = vec![src.join("lib.rs"), src.join("catapult.h")];
        let filters = VendorFilters {
            buck_file_name: None,
            checksum_filter: Some(glob_filter("*.h")),
        };
        let pkgdir = std::path::Path::new("vendor/siege-engines-0.1.0");

        let cksums =
            copy_vendor_sources(src, &file_paths, dst, &filters, pkgdir).expect("copy succeeded");

        // Header file must be copied to disk (checksum filter does not delete).
        assert!(
            dst.join("catapult.h").exists(),
            ".h file should be copied to dst"
        );
        assert!(
            dst.join("lib.rs").exists(),
            "lib.rs should be copied to dst"
        );

        assert!(
            !cksums.contains_key("catapult.h"),
            ".h file should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );
    }
    #[test]
    fn test_copy_vendor_sources_gitignore_filter_keeps_cargo_toml_orig() {
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
        let pkgdir = std::path::Path::new("vendor/fb-procfs-0.9.0");

        let cksums =
            copy_vendor_sources(src, &file_paths, dst, &filters, pkgdir).expect("copy succeeded");

        assert!(
            dst.join("Cargo.toml.orig").exists(),
            "gitignore-matched Cargo.toml.orig should still be copied to dst"
        );
        assert!(
            !cksums.contains_key("Cargo.toml.orig"),
            "gitignore-matched Cargo.toml.orig should be absent from checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should remain in checksum map"
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

    // Invariant: copy_vendor_sources copies files while skipping excluded paths, computing checksums
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
        let cksums = copy_vendor_sources(
            src_dir.path(),
            &file_paths,
            dst_dir.path(),
            &filters,
            Path::new("vendor/example-0.1.0"),
        )
        .unwrap();

        assert!(dst_dir.path().join("lib.rs").exists());
        assert!(!dst_dir.path().join(".gitignore").exists());
        assert!(dst_dir.path().join("Cargo.toml").exists());
        assert_eq!(cksums.len(), 2);
        assert!(cksums.contains_key("lib.rs"));
        assert!(cksums.contains_key("Cargo.toml"));
        assert!(!cksums.contains_key(".gitignore"));
    }
}
