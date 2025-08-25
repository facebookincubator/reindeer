/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Index for Cargo metadata, and various useful traversals.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Context as _;
use anyhow::bail;
use cargo::core::PackageId;
use foldhash::HashMap;
use foldhash::HashSet;
use semver::VersionReq;

use crate::buck::Name;
use crate::cargo::ArtifactKind;
use crate::cargo::DepKind;
use crate::cargo::Manifest;
use crate::cargo::ManifestDep;
use crate::cargo::ManifestTarget;
use crate::cargo::Metadata;
use crate::cargo::Node;
use crate::cargo::NodeDep;
use crate::cargo::NodeDepKind;
use crate::cargo::TargetKind;
use crate::cargo::TargetReq;
use crate::config::Config;
use crate::fixups::FixupsCache;
use crate::platform::PlatformConfig;
use crate::platform::PlatformExpr;
use crate::platform::PlatformName;

/// Index for interesting things in Cargo metadata
pub struct Index<'meta> {
    config: &'meta Config,
    /// Map a PackageId to the Manifest (package) with its details
    pkgid_to_pkg: HashMap<PackageId, &'meta Manifest>,
    /// Map a PackageId to a Node (ie all the details of a resolve dependency)
    pkgid_to_node: HashMap<PackageId, &'meta Node>,
    /// Per-platform feature resolution.
    pkgid_platform_features: HashMap<(PackageId, &'meta PlatformName), ResolvedFeatures<'meta>>,
    /// Represents the Cargo.toml itself, if not a virtual manifest.
    pub root_pkg: Option<&'meta Manifest>,
    /// All packages considered part of the workspace.
    pub workspace_members: Vec<&'meta Manifest>,
    /// Faster lookup for workspace members
    workspace_packages: HashSet<PackageId>,
    /// Set of package IDs from which at least one target is public mapped to an optional rename
    public_packages: BTreeMap<PackageId, Option<&'meta str>>,
    /// The (possibly renamed) names of all packages which have at least one
    /// public target.
    public_package_names: BTreeSet<&'meta str>,
    /// Set of public targets. These consist of:
    /// - root_pkg, if it is being made public (aka "real", and not just a pseudo package)
    /// - first-order dependencies of root_pkg, including artifact dependencies
    public_targets: BTreeMap<(PackageId, TargetReq<'meta>), Option<&'meta str>>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDep<'meta> {
    pub package: &'meta Manifest,
    pub rename: &'meta str,
    pub dep_kind: &'meta NodeDepKind,
}

#[derive(Debug, Default)]
pub struct ResolvedFeatures<'meta> {
    /// Is this crate present in the dependency graph.
    ///
    /// A disabled optional dependency might have nonempty `features` in the
    /// case of "dep?/feature" feature syntax.
    pub enabled: bool,
    /// If the crate is present or will become present, which of its features
    /// are to be enabled.
    pub features: BTreeSet<&'meta str>,
    /// If the crate is present or will become present, what dependencies it
    /// would have.
    pub deps: HashSet<(&'meta str, &'meta NodeDepKind, PackageId)>,
}

impl<'meta> Index<'meta> {
    /// Construct an index for a set of Cargo metadata to allow convenient and efficient
    /// queries. The metadata represents a top level package and all its transitive
    /// dependencies.
    pub fn new(
        config: &'meta Config,
        metadata: &'meta Metadata,
        fixups: &FixupsCache<'meta>,
    ) -> anyhow::Result<Index<'meta>> {
        let pkgid_to_pkg: HashMap<_, _> = metadata.packages.iter().map(|m| (m.id, m)).collect();

        let root_pkg = metadata.resolve.root.as_ref().map(|root_pkgid| {
            *pkgid_to_pkg
                .get(root_pkgid)
                .expect("couldn't identify unambiguous top-level crate")
        });

        let top_levels = if config.include_top_level {
            Some(
                root_pkg
                    .context("`include_top_level = true` is not supported on a virtual manifest")?
                    .id,
            )
        } else {
            None
        };

        let workspace_members: Vec<_> = metadata
            .workspace_members
            .iter()
            .filter_map(|pkgid| pkgid_to_pkg.get(pkgid).copied())
            .collect();

        let mut index = Index {
            config,
            pkgid_to_pkg,
            pkgid_to_node: metadata.resolve.nodes.iter().map(|n| (n.id, n)).collect(),
            pkgid_platform_features: HashMap::default(),
            root_pkg,
            workspace_packages: workspace_members.iter().map(|x| x.id).collect(),
            workspace_members,
            public_packages: BTreeMap::new(),
            public_package_names: BTreeSet::new(),
            public_targets: BTreeMap::new(),
        };

        // Keep an index of renamed crates, mapping from _ normalized name to actual name.
        // Only the root package's renames matter. We don't attempt to merge different
        // rename choices made by different workspace members.
        let dep_renamed: HashMap<String, &'meta str> = root_pkg
            .iter()
            .flat_map(|root_pkg| &root_pkg.dependencies)
            .filter_map(|dep| {
                let rename = dep.rename.as_deref()?;
                Some((rename.replace('-', "_"), rename))
            })
            .collect();

        // Compute public set, with pkgid mapped to rename if it has one. Public set is
        // anything in top_levels, or first-order dependencies of any workspace member.
        index.public_targets = index
            .workspace_members
            .iter()
            .flat_map(|member| &index.pkgid_to_node[&member.id].deps)
            .flat_map(|node_dep| {
                let pkg = &index.pkgid_to_pkg[&node_dep.pkg];
                node_dep.dep_kinds.iter().map(|dep_kind| {
                    let name = node_dep
                        .name
                        .as_deref()
                        .or(dep_kind.extern_name.as_deref())
                        .unwrap();
                    let target_req = dep_kind.target_req();
                    let opt_rename = dep_renamed.get(name).cloned();
                    ((pkg.id, target_req), opt_rename)
                })
            })
            .chain(top_levels.iter().flat_map(|&pkgid| {
                [
                    ((pkgid, TargetReq::Lib), None),
                    ((pkgid, TargetReq::EveryBin), None),
                ]
            }))
            .collect::<BTreeMap<_, _>>();

        for (&(id, _), &rename) in &index.public_targets {
            index.public_packages.insert(id, rename);
            index
                .public_package_names
                .insert(if let Some(rename) = rename {
                    rename
                } else {
                    &index.pkgid_to_pkg[&id].name
                });
        }

        let mut manifest_deps = HashMap::default();
        for (&pkgid, node) in &index.pkgid_to_node {
            for node_dep in &node.deps {
                let manifest = index.pkgid_to_pkg[&node_dep.pkg];
                for dep_kind in &node_dep.dep_kinds {
                    manifest_deps
                        .entry((
                            pkgid,
                            manifest.name.as_str(),
                            dep_kind.kind,
                            &dep_kind.target,
                        ))
                        .or_insert_with(Vec::new)
                        .push((node_dep, dep_kind, manifest));
                }
            }
        }

        for (platform_name, platform_config) in &config.platform {
            let mut resolve = FeatureResolver {
                pkgid_platform_features: &mut index.pkgid_platform_features,
                pkgid_to_pkg: &index.pkgid_to_pkg,
                manifest_deps: &manifest_deps,
                public_package_names: &index.public_package_names,
                config,
                fixups,
            };
            // Feature selection for the current workspace.
            for pkg in &index.workspace_members {
                resolve.enable_crate_for_platform(pkg.id, platform_name)?;
                if let Some(features) = &platform_config.features {
                    for feature in features {
                        resolve.enable_feature_for_platform(pkg.id, platform_name, feature)?;
                    }
                } else if pkg.features.contains_key("default") {
                    resolve.enable_feature_for_platform(pkg.id, platform_name, "default")?;
                }
            }
        }

        Ok(index)
    }

    /// Test if a package is the root package
    pub fn is_root_package(&self, pkg: &Manifest) -> bool {
        match self.root_pkg {
            Some(root_pkg) => root_pkg.id == pkg.id,
            None => false,
        }
    }

    /// Test if there is any target from the package which is public
    pub fn is_public_package(&self, pkg: &Manifest) -> bool {
        self.public_packages.contains_key(&pkg.id)
    }

    /// Test if this is a workspace member
    pub fn is_workspace_package(&self, pkg: &Manifest) -> bool {
        self.workspace_packages.contains(&pkg.id)
    }

    /// Test if there is any target from any package with the given (possibly
    /// renamed) crate name which is public.
    pub fn is_public_package_name(&self, name: &str) -> bool {
        self.public_package_names.contains(&name)
    }

    /// Test if a specific target from a package is public
    pub fn is_public_target(&self, pkg: &Manifest, target_req: TargetReq) -> bool {
        self.public_targets.contains_key(&(pkg.id, target_req))
    }

    /// Return the private package rule name.
    pub fn private_rule_name(&self, pkg: &Manifest) -> Name {
        Name(match self.public_packages.get(&pkg.id) {
            Some(None) | None => pkg.to_string(), // Full version info
            Some(Some(rename)) => format!("{}-{}", pkg, rename), // Rename
        })
    }

    /// Return the package public rule name.
    pub fn public_rule_name(&self, pkg: &'meta Manifest) -> Name {
        Name(match self.public_packages.get(&pkg.id) {
            Some(None) | None => pkg.name.to_owned(), // Package name
            Some(&Some(rename)) => rename.to_owned(), // Rename
        })
    }

    pub fn compatible_platforms(&self, pkg: &Manifest) -> BTreeSet<&'meta PlatformName> {
        let mut compatible_platforms = BTreeSet::new();
        for platform_name in self.config.platform.keys() {
            if self
                .pkgid_platform_features
                .contains_key(&(pkg.id, platform_name))
            {
                compatible_platforms.insert(platform_name);
            }
        }
        compatible_platforms
    }

    /// Return the set of features resolved for a particular package
    pub fn resolved_features(
        &self,
        pkg: &Manifest,
        platform_name: &PlatformName,
    ) -> BTreeSet<&str> {
        let mut features = BTreeSet::new();
        if let Some(resolve) = self.pkgid_platform_features.get(&(pkg.id, platform_name)) {
            for &feature in &resolve.features {
                if !feature.starts_with("dep:") {
                    features.insert(feature);
                }
            }
        }
        features
    }

    /// Return resolved dependencies for a target.
    pub fn resolved_deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
        platform_name: &PlatformName,
    ) -> impl Iterator<Item = ResolvedDep<'meta>> + '_ {
        // Target must be the target for the given package.
        assert!(pkg.targets.contains(tgt));

        let mut resolved_deps = HashMap::default();

        if let Some(resolve) = self.pkgid_platform_features.get(&(pkg.id, platform_name)) {
            for &(rename, dep_kind, dep_id) in &resolve.deps {
                if match dep_kind.kind {
                    DepKind::Normal => {
                        tgt.kind_lib()
                            || tgt.kind_proc_macro()
                            || tgt.kind_bin()
                            || tgt.kind_cdylib()
                            || tgt.kind_staticlib()
                    }
                    DepKind::Dev => tgt.kind_bench() || tgt.kind_test() || tgt.kind_example(),
                    DepKind::Build => tgt.kind_custom_build(),
                } {
                    let dep = &self.pkgid_to_pkg[&dep_id];
                    // Key by everything except `target`.
                    let NodeDepKind {
                        kind,
                        target: _,
                        artifact,
                        extern_name,
                        compile_target,
                        bin_name,
                    } = dep_kind;
                    let (unconditional_deps, conditional_deps) = resolved_deps
                        .entry((
                            &dep.id,
                            kind,
                            artifact,
                            extern_name,
                            compile_target,
                            bin_name,
                        ))
                        .or_insert_with(|| (vec![], vec![]));
                    let v = (rename, dep_kind, dep);
                    if dep_kind.target.is_none() {
                        unconditional_deps.push(v);
                    } else {
                        conditional_deps.push(v);
                    };
                }
            }
        }

        resolved_deps
            .into_iter()
            .flat_map(|((pkgid, ..), (unconditional_deps, conditional_deps))| {
                // When there are "unconditional" deps (i.e. `target` is None),
                // all "conditional" deps are ignored. AFAIK, it's not possible
                // to have more than one "unconditional" dep. Make sure that
                // assumption holds up because otherwise it means somewhere
                // in `resolved_deps` we did something wrong.
                match unconditional_deps.len() {
                    0 => conditional_deps,
                    1 => unconditional_deps,
                    _ => panic!(
                        "`{}` had more than one unconditional dep for `{}` {:?}",
                        pkg.name, pkgid, unconditional_deps,
                    ),
                }
            })
            .map(|(rename, dep_kind, dep)| ResolvedDep {
                package: dep,
                rename,
                dep_kind,
            })
    }
}

/// Information referenced while computing feature resolution for an Index.
#[derive(Debug)]
struct FeatureResolver<'a, 'meta> {
    pkgid_platform_features:
        &'a mut HashMap<(PackageId, &'meta PlatformName), ResolvedFeatures<'meta>>,
    pkgid_to_pkg: &'a HashMap<PackageId, &'meta Manifest>,
    manifest_deps: &'a HashMap<
        (PackageId, &'meta str, DepKind, &'meta Option<PlatformExpr>),
        Vec<(&'meta NodeDep, &'meta NodeDepKind, &'meta Manifest)>,
    >,
    public_package_names: &'a BTreeSet<&'meta str>,
    config: &'meta Config,
    fixups: &'a FixupsCache<'meta>,
}

impl<'a, 'meta> FeatureResolver<'a, 'meta> {
    fn enable_crate_for_platform(
        &mut self,
        pkgid: PackageId,
        platform_name: &'meta PlatformName,
    ) -> anyhow::Result<()> {
        let resolve = self
            .pkgid_platform_features
            .entry((pkgid, platform_name))
            .or_insert_with(ResolvedFeatures::default);
        if resolve.enabled {
            // Already been enabled.
            return Ok(());
        }

        resolve.enabled = true;

        // Omit dependencies according to fixups.
        let pkg = self.pkgid_to_pkg[&pkgid];
        let public = self.public_package_names.contains(pkg.name.as_str());
        let fixups = self.fixups.get(pkg, public)?;

        // Go through the manifest dependencies and enable all that are non-optional.
        for manifest_dep in &pkg.dependencies {
            if !manifest_dep.optional
                && match manifest_dep.kind {
                    DepKind::Normal | DepKind::Build => true,
                    DepKind::Dev => false,
                }
                && match &manifest_dep.target {
                    Some(platform_expr) => platform_expr.eval(
                        &self.config.platform[platform_name],
                        None,
                        &fixups.compute_rustc_cfg(platform_name),
                    ),
                    None => true,
                }
                && !fixups.omit_dep(
                    platform_name,
                    manifest_dep.rename.as_ref().unwrap_or(&manifest_dep.name),
                )
            {
                let node_dep = self.resolve_dep(pkgid, manifest_dep)?;
                for dep_kind in &node_dep.dep_kinds {
                    self.pkgid_platform_features
                        .entry((pkgid, platform_name))
                        .or_insert_with(ResolvedFeatures::default)
                        .deps
                        .insert((
                            node_dep
                                .name
                                .as_deref()
                                .or(dep_kind.extern_name.as_deref())
                                .unwrap(),
                            dep_kind,
                            node_dep.pkg,
                        ));
                }
                for dep_platform_name in platforms_for_dependency(
                    node_dep,
                    self.pkgid_to_pkg[&node_dep.pkg],
                    platform_name,
                    &self.config.platform[platform_name],
                ) {
                    self.enable_crate_for_platform(node_dep.pkg, dep_platform_name)?;
                    if manifest_dep.uses_default_features
                        && self.pkgid_to_pkg[&node_dep.pkg]
                            .features
                            .contains_key("default")
                    {
                        self.enable_feature_for_platform(
                            node_dep.pkg,
                            dep_platform_name,
                            "default",
                        )?;
                    }
                    for required_feature in &manifest_dep.features {
                        self.enable_feature_for_platform(
                            node_dep.pkg,
                            dep_platform_name,
                            required_feature,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn enable_feature_for_platform(
        &mut self,
        pkgid: PackageId,
        platform_name: &'meta PlatformName,
        enable_feature: &'meta str,
    ) -> anyhow::Result<()> {
        let resolve = self
            .pkgid_platform_features
            .entry((pkgid, platform_name))
            .or_insert_with(ResolvedFeatures::default);

        // Omit features according to fixups.
        let pkg = self.pkgid_to_pkg[&pkgid];
        let public = self.public_package_names.contains(pkg.name.as_str());
        let fixups = self.fixups.get(pkg, public)?;
        if fixups.omit_feature(platform_name, enable_feature) {
            return Ok(());
        }

        if resolve.features.insert(enable_feature) {
            if let Some(enable_dependency) = enable_feature.strip_prefix("dep:") {
                self.enable_dependency_for_platform(pkgid, platform_name, enable_dependency)?;
            } else if let Some(required_features) = pkg.features.get(enable_feature) {
                for required_feature in required_features {
                    if let Some((dep, enable_feature)) = required_feature.split_once("/") {
                        let enable_dep = !dep.ends_with('?');
                        let dep = dep.strip_suffix('?').unwrap_or(dep);
                        // Find which dependency this feature refers to.
                        for manifest_dep in &self.pkgid_to_pkg[&pkgid].dependencies {
                            if dep == manifest_dep.rename.as_ref().unwrap_or(&manifest_dep.name)
                                && match manifest_dep.kind {
                                    DepKind::Normal | DepKind::Build => true,
                                    DepKind::Dev => false,
                                }
                                && match &manifest_dep.target {
                                    Some(platform_expr) => platform_expr.eval(
                                        &self.config.platform[platform_name],
                                        None,
                                        &fixups.compute_rustc_cfg(platform_name),
                                    ),
                                    None => true,
                                }
                            {
                                let node_dep = self.resolve_dep(pkgid, manifest_dep)?;
                                let dep_platforms = platforms_for_dependency(
                                    node_dep,
                                    self.pkgid_to_pkg[&node_dep.pkg],
                                    platform_name,
                                    &self.config.platform[platform_name],
                                );
                                if manifest_dep.optional && enable_dep {
                                    if pkg.features.contains_key(dep) {
                                        self.enable_feature_for_platform(
                                            pkgid,
                                            platform_name,
                                            dep,
                                        )?;
                                    }
                                    self.enable_dependency_for_platform(pkgid, platform_name, dep)?;
                                    for &dep_platform_name in &dep_platforms {
                                        if manifest_dep.uses_default_features
                                            && self.pkgid_to_pkg[&node_dep.pkg]
                                                .features
                                                .contains_key("default")
                                        {
                                            self.enable_feature_for_platform(
                                                node_dep.pkg,
                                                dep_platform_name,
                                                "default",
                                            )?;
                                        }
                                        for required_feature in &manifest_dep.features {
                                            self.enable_feature_for_platform(
                                                node_dep.pkg,
                                                dep_platform_name,
                                                required_feature,
                                            )?;
                                        }
                                    }
                                }
                                for dep_platform_name in dep_platforms {
                                    self.enable_feature_for_platform(
                                        node_dep.pkg,
                                        dep_platform_name,
                                        enable_feature,
                                    )?;
                                }
                            }
                        }
                    } else {
                        self.enable_feature_for_platform(pkgid, platform_name, required_feature)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn enable_dependency_for_platform(
        &mut self,
        pkgid: PackageId,
        platform_name: &'meta PlatformName,
        enable_dependency: &'meta str,
    ) -> anyhow::Result<()> {
        // Omit dependencies according to fixups.
        let pkg = self.pkgid_to_pkg[&pkgid];
        let public = self.public_package_names.contains(pkg.name.as_str());
        let fixups = self.fixups.get(pkg, public)?;
        if fixups.omit_dep(platform_name, enable_dependency) {
            return Ok(());
        }

        // Find the matching crate and enable it.
        for manifest_dep in &pkg.dependencies {
            if enable_dependency == manifest_dep.rename.as_ref().unwrap_or(&manifest_dep.name)
                && manifest_dep.optional
                && match manifest_dep.kind {
                    DepKind::Normal | DepKind::Build => true,
                    DepKind::Dev => false,
                }
                && match &manifest_dep.target {
                    Some(platform_expr) => platform_expr.eval(
                        &self.config.platform[platform_name],
                        None,
                        &fixups.compute_rustc_cfg(platform_name),
                    ),
                    None => true,
                }
            {
                let node_dep = self.resolve_dep(pkgid, manifest_dep)?;
                for dep_kind in &node_dep.dep_kinds {
                    self.pkgid_platform_features
                        .entry((pkgid, platform_name))
                        .or_insert_with(ResolvedFeatures::default)
                        .deps
                        .insert((
                            node_dep
                                .name
                                .as_deref()
                                .or(dep_kind.extern_name.as_deref())
                                .unwrap(),
                            dep_kind,
                            node_dep.pkg,
                        ));
                }
                for dep_platform_name in platforms_for_dependency(
                    node_dep,
                    self.pkgid_to_pkg[&node_dep.pkg],
                    platform_name,
                    &self.config.platform[platform_name],
                ) {
                    self.enable_crate_for_platform(node_dep.pkg, dep_platform_name)?;
                    if manifest_dep.uses_default_features
                        && self.pkgid_to_pkg[&node_dep.pkg]
                            .features
                            .contains_key("default")
                    {
                        self.enable_feature_for_platform(
                            node_dep.pkg,
                            dep_platform_name,
                            "default",
                        )?;
                    }
                    for required_feature in &manifest_dep.features {
                        self.enable_feature_for_platform(
                            node_dep.pkg,
                            dep_platform_name,
                            required_feature,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    /// There is a many-to-one relationship between ManifestDep and NodeDep. One
    /// ManifestDep is always fulfilled by a single NodeDep. But one NodeDep
    /// might fulfill many ManifestDep. For example in this case:
    ///
    /// ```toml
    /// [dependencies]
    /// example = "1"
    ///
    /// [target.'cfg(...)'.dependencies]
    /// example = { version = "1", features = ["feat"] }
    /// ```
    ///
    /// This package would have 2 ManifestDep dependencies on `example` in the
    /// unresolved dependency graph, both satisfied by the same 1 NodeDep on
    /// `example` in the resolved graph.
    ///
    /// The logic in this method is for finding which NodeDep fulfills a
    /// particular ManifestDep. A ManifestDep can involve multiple crates (for
    /// example a package's library and multiple of its binaries). We pick any
    /// one of those and find which NodeDep provides that crate. The same
    /// NodeDep will provide all the other crates in this ManifestDep too.
    fn resolve_dep(
        &self,
        pkgid: PackageId,
        manifest_dep: &'meta ManifestDep,
    ) -> anyhow::Result<&'meta NodeDep> {
        let Some(candidates) = self.manifest_deps.get(&(
            pkgid,
            manifest_dep.name.as_str(),
            manifest_dep.kind,
            &manifest_dep.target,
        )) else {
            bail!(
                "No candidates found for dependency {} of {pkgid}",
                manifest_dep.name,
            );
        };

        let mut matching_node_dep = None;
        let mut ambiguous = false;
        let mut produce_match = |node_dep: &'meta NodeDep| {
            ambiguous |= matching_node_dep.is_some_and(|prev: &NodeDep| prev.pkg != node_dep.pkg);
            matching_node_dep = Some(node_dep);
        };

        for (node_dep, dep_kind, manifest) in candidates {
            // Compare version.
            if manifest_dep.req != VersionReq::STAR && !manifest_dep.req.matches(&manifest.version)
            {
                continue;
            }

            // Locate the package's library target, if any.
            let mut library_target = None;
            for manifest_target in &manifest.targets {
                if manifest_target
                    .kind
                    .iter()
                    .any(|target_kind| match target_kind {
                        // Library targets.
                        TargetKind::Dylib
                        | TargetKind::Lib
                        | TargetKind::Rlib
                        | TargetKind::ProcMacro
                        | TargetKind::Staticlib
                        | TargetKind::Cdylib => true,
                        // Not library targets.
                        TargetKind::Bench
                        | TargetKind::Bin
                        | TargetKind::CustomBuild
                        | TargetKind::Example
                        | TargetKind::Test => false,
                    })
                {
                    if library_target.is_some() {
                        // Unexpected: multiple library targets in the same crate.
                        bail!(
                            "Unexpected: multiple library targets found in {}",
                            manifest.id,
                        );
                    }
                    library_target = Some(manifest_target);
                }
            }

            if let Some(node_artifact_kind) = dep_kind.artifact {
                // Look for an artifact dependency.
                let Some(manifest_artifact) = &manifest_dep.artifact else {
                    continue;
                };
                if !manifest_artifact.kinds.contains(&node_artifact_kind) {
                    continue;
                }

                // Compare artifact name.
                if let Some(rename) = &manifest_dep.rename {
                    // If the ManifestDep has a rename, the NodeDepKind's
                    // extern_name will be that rename with a replacement of
                    // '-' -> '_'.
                    let expected_extern_name = rename.replace('-', "_");
                    if dep_kind.extern_name == Some(expected_extern_name) {
                        produce_match(node_dep);
                    }
                } else {
                    match node_artifact_kind {
                        // For bin artifacts without a rename, the extern_name
                        // will be based on bin_name.
                        ArtifactKind::Bin => {
                            let Some(bin_name) = &dep_kind.bin_name else {
                                continue;
                            };
                            let expected_extern_name = bin_name.replace('-', "_");
                            if dep_kind.extern_name == Some(expected_extern_name) {
                                produce_match(node_dep);
                            }
                        }
                        // For staticlib and cdylib artifacts, the extern_name
                        // wil be the package's library target's name.
                        ArtifactKind::Staticlib | ArtifactKind::Cdylib => {
                            if let Some(library_target) = library_target {
                                if dep_kind.extern_name.as_ref() == Some(&library_target.name) {
                                    produce_match(node_dep);
                                }
                            }
                        }
                    }
                }
            } else if manifest_dep
                .artifact
                .as_ref()
                .is_none_or(|artifact| artifact.lib)
            {
                // Look for library dependency.
                if let Some(rename) = &manifest_dep.rename {
                    let expected_node_name = rename.replace('-', "_");
                    if node_dep.name == Some(expected_node_name) {
                        produce_match(node_dep);
                    }
                } else if let Some(library_target) = library_target {
                    if node_dep.name.as_ref() == Some(&library_target.name) {
                        produce_match(node_dep);
                    }
                }
            }
        }

        if ambiguous {
            bail!(
                "Multiple matches found for dependency {} of {pkgid}",
                manifest_dep.name,
            );
        }

        let Some(unique_match) = matching_node_dep else {
            bail!(
                "No match found for dependency {} of {pkgid}",
                manifest_dep.name,
            );
        };

        Ok(unique_match)
    }
}

fn platforms_for_dependency<'meta>(
    node_dep: &'meta NodeDep,
    manifest: &'meta Manifest,
    platform_name: &'meta PlatformName,
    platform_config: &'meta PlatformConfig,
) -> BTreeSet<&'meta PlatformName> {
    let mut is_target_dep = false;
    let mut is_exec_dep = false;

    for dep_kind in &node_dep.dep_kinds {
        match dep_kind.kind {
            DepKind::Normal => {
                if dep_kind.artifact.is_none()
                    && manifest.targets.iter().any(ManifestTarget::kind_proc_macro)
                {
                    is_exec_dep = true;
                } else {
                    is_target_dep = true;
                }
            }
            DepKind::Dev => {}
            DepKind::Build => {
                is_exec_dep = true;
            }
        }
    }

    let mut dep_platforms = BTreeSet::new();
    if is_target_dep {
        dep_platforms.insert(platform_name);
    }
    if is_exec_dep {
        dep_platforms.extend(&platform_config.execution_platforms);
    }
    dep_platforms
}
