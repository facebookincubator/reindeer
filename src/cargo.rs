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
use std::io::BufRead;
use std::io::BufReader;
use std::iter;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::rc::Rc;
use std::task::Poll;
use std::thread;

use anyhow::Context;
use anyhow::bail;
use cargo::core::PackageId;
use cargo::util::interning::InternedString;
use foldhash::HashMap;
use foldhash::HashSet;
use indoc::indoc;
use semver::VersionReq;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::Visitor;
use serde_with::As;
use serde_with::DeserializeAs;

use crate::Args;
use crate::Paths;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::lockfile::Lockfile;
use crate::platform::PlatformExpr;

pub fn cargo_get_lockfile_and_metadata(
    config: &Config,
    args: &Args,
    paths: &Paths,
    fast: bool,
) -> anyhow::Result<(Lockfile, Metadata)> {
    if let VendorConfig::Source(_) = config.vendor {
        let lockfile = Lockfile::load(paths)?;
        let metadata = if fast {
            fast_metadata(config, paths)?
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

fn fast_metadata(config: &Config, paths: &Paths) -> anyhow::Result<Metadata> {
    // Configure Cargo global context.
    let shell = cargo::core::Shell::new();
    let cwd = paths.third_party_dir.clone();
    let cargo_home = paths.cargo_home.clone();
    let mut gctx = cargo::GlobalContext::new(shell, cwd, cargo_home);

    let mut unstable_flags = Vec::new();
    if config.cargo.bindeps {
        unstable_flags.push("bindeps".to_owned());
    }

    let verbose = 0;
    let quiet = false;
    let color = None;
    let frozen = true;
    let locked = true;
    let offline = true;
    let target_dir = None;
    let cli_config = [];
    gctx.configure(
        verbose,
        quiet,
        color,
        frozen,
        locked,
        offline,
        &target_dir,
        &unstable_flags,
        &cli_config,
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
                            "bin" => ArtifactKind::Bin,
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

    // Priority:
    // 1. `--rustc-path` arg.
    // 2. `RUSTC` env.
    // 3. `reindeer.toml` config value.
    if let Some(rustc_path) = args.rustc_path.as_ref() {
        cargo_command.env("RUSTC", rustc_path);
    } else if let Some(rustc_path) = env::var_os("RUSTC") {
        // Any relative RUSTC path in Reindeer's env is meant to be interpreted
        // relative to the directory Reindeer is running in, which is different
        // from the one we are about to invoke Cargo in. Patch up the RUSTC
        // in the env.
        if Path::new(&rustc_path).components().nth(1).is_some() {
            if let Ok(current_dir) = env::current_dir() {
                let rustc_path = current_dir.join(rustc_path);
                cargo_command.env("RUSTC", rustc_path);
            }
        }
    } else if let Some(bin) = config.cargo.rustc.as_ref() {
        cargo_command.env("RUSTC", config.config_dir.join(bin));
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
    /// Source repository
    pub repository: Option<String>,
    /// Default edition for the package (if targets don't have it)
    pub edition: Edition,
    /// Name of a native library that the build script links
    pub links: Option<String>,
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

#[derive(Debug, Deserialize, Eq, PartialEq, Hash, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Bin,
    Staticlib,
    Cdylib,
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
            Some(ArtifactKind::Bin) => {
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

#[cfg(test)]
mod test {
    use super::Source;
    use super::parse_source;

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
}
