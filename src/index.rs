/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Index for Cargo metadata, and various useful traversals.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    error, fmt,
    sync::Arc,
};

use anyhow::Result;
use serde::Deserialize;

use crate::{
    cargo::{DepKind, Manifest, ManifestDep, ManifestTarget, Metadata, Node, NodeDep, PkgId},
    platform::{PlatformExpr, PlatformPredicate},
};

/// Index for interesting things in Cargo metadata
pub struct Index<'meta> {
    /// Map a PkgId to the Manifest (package) with its details
    pkgid_to_pkg: Arc<HashMap<&'meta PkgId, &'meta Manifest>>,
    /// Map a PkgId to a Node (ie all the details of a resolve dependency)
    pkgid_to_node: HashMap<&'meta PkgId, &'meta Node>,
    /// Represents the Cargo.toml itself
    root_pkg: &'meta Manifest,
    /// Set of public targets. These consist of:
    /// - root_pkg, if it is being made public (aka "real", and not just a pseudo package)
    /// - first-order dependencies of root_pkg
    /// - any extra top-levels (binary and cdylib only packages)
    public_set: BTreeMap<&'meta PkgId, Option<&'meta str>>,
}

/// Extra per-package metadata to be kept in sync with the package list
#[derive(Debug, Deserialize)]
pub struct ExtraMetadata {
    pub oncall: String, // oncall shortname for use as maintainer
}

// Cumulative errors in package metadata
#[derive(Debug, Clone)]
struct PackageMetaError {
    extra: BTreeSet<String>,
}

impl PackageMetaError {
    fn new() -> Self {
        PackageMetaError {
            extra: BTreeSet::new(),
        }
    }

    fn all_ok(&self) -> bool {
        self.extra.is_empty()
    }

    fn add_extra(&mut self, s: impl ToString) {
        self.extra.insert(s.to_string());
    }
}

impl error::Error for PackageMetaError {}

impl fmt::Display for PackageMetaError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        if self.extra.is_empty() {
            write!(fmt, "Package Metadata: all OK")?;
        } else {
            write!(fmt, "Extra metadata for package(s):")?;
            for p in &self.extra {
                write!(fmt, " {}", p)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedDep<'meta> {
    pub alias: &'meta str,
    pub package: &'meta Manifest,
    pub platform: Option<PlatformExpr>,
}

impl<'meta> Index<'meta> {
    /// Construct an index for a set of Cargo metadata to allow convenient and efficient
    /// queries. The metadata represents a top level package and all its transitive
    /// dependencies.
    pub fn new(
        root_is_real: bool,
        extra_top_levels: bool,
        metadata: &'meta Metadata,
    ) -> Index<'meta> {
        let pkgid_to_pkg: HashMap<_, _> = metadata.packages.iter().map(|m| (&m.id, m)).collect();

        let root_pkg: &Manifest = pkgid_to_pkg
            .get(&metadata.resolve.root.as_ref().expect("missing root pkg"))
            .expect("couldn't identify unambiguous top-level crate");

        let mut top_levels = HashSet::new();
        if root_is_real {
            top_levels.insert(&root_pkg.id);
        }
        if extra_top_levels {
            for pkg in &metadata.packages {
                // If the package doesn't have any dependable targets (rlib/dylib or proc_maco)
                // but it does have a cdylib or binary then add it to the top levels.
                // (Ignored: tests, benchmarks, examples, build scripts, staticlib)
                if pkg != root_pkg
                    && !pkg
                        .targets
                        .iter()
                        .any(|tgt| tgt.kind_lib() || tgt.kind_proc_macro())
                    && pkg
                        .targets
                        .iter()
                        .any(|tgt| tgt.kind_cdylib() || tgt.kind_bin())
                {
                    log::info!("Extra top level {}", &pkg.id);
                    top_levels.insert(&pkg.id);
                }
            }
        }

        let tmp = Index {
            pkgid_to_pkg: Arc::new(pkgid_to_pkg),
            pkgid_to_node: metadata.resolve.nodes.iter().map(|n| (&n.id, n)).collect(),
            root_pkg,
            public_set: BTreeMap::new(),
        };

        // Keep an index of renamed crates, mapping from _ normalized name to actual name
        let dep_renamed: HashMap<String, &'meta str> = root_pkg
            .dependencies
            .iter()
            .filter_map(|dep| {
                dep.rename
                    .as_ref()
                    .map(|alias| (alias.replace("-", "_"), alias.as_str()))
            })
            .collect();

        // Compute public set, with pkgid mapped to alias if it has one. Public set is
        // anything in top_levels, or first-order dependencies of root_pkg.
        let public_set = tmp
            .resolved_deps(tmp.root_pkg)
            .map(|(alias, pkg)| (&pkg.id, dep_renamed.get(alias).cloned()))
            .chain(top_levels.iter().map(|pkgid| (*pkgid, None)))
            .collect();

        Index { public_set, ..tmp }
    }

    /// Test if a package is public
    pub fn is_public(&self, pkg: &Manifest) -> bool {
        self.public_set.contains_key(&pkg.id)
    }

    /// Return a package's local alias, if it has one.
    pub fn public_alias(&self, pkg: &Manifest) -> Option<&str> {
        self.public_set.get(&pkg.id).and_then(|x| *x)
    }

    /// Return all public packages
    pub fn public_packages(&'meta self) -> impl Iterator<Item = &'meta Manifest> {
        let pkgid_to_pkg = Arc::clone(&self.pkgid_to_pkg);
        self.public_set
            .keys()
            .map(move |id| *pkgid_to_pkg.get(id).expect("missing pkgid"))
    }

    /// Returns the transitive closure of dependencies of public packages.
    pub fn all_packages(&'meta self) -> impl Iterator<Item = &'meta Manifest> {
        self.pkgid_to_pkg.values().copied()
    }

    /// Return the public package rule name
    pub fn rule_name(&self, pkg: &Manifest) -> String {
        match self.public_set.get(&pkg.id) {
            Some(None) => pkg.name.to_string(),        // Base name
            Some(Some(alias)) => (*alias).to_string(), // Alias
            None => pkg.to_string(),                   // Full version info
        }
    }

    pub fn get_extra_meta(&self) -> Result<HashMap<&'meta str, ExtraMetadata>> {
        // Package names borrowed from metadata
        let pubpkgs: HashSet<&'meta str> = self
            .root_pkg
            .dependencies
            .iter()
            .map(|dep| dep.name.as_str())
            .collect();
        let mut pkgerrs = PackageMetaError::new();

        let res: HashMap<_, _> = self
            .root_pkg
            .metadata
            .get("third-party")
            .map(|v| serde_json::from_value::<HashMap<String, ExtraMetadata>>(v.clone()))
            .unwrap_or_else(|| Ok(HashMap::new()))?;

        let mut ret: HashMap<&'meta str, ExtraMetadata> = HashMap::new();
        for (name, val) in res {
            // remap names to borrowed from metadata, but also check to see if there's
            // extra metadata (metadata which references a pkg which doesn't exist)
            match pubpkgs.get(name.as_str()) {
                None => {
                    pkgerrs.add_extra(name);
                }
                Some(pkg) => {
                    ret.insert(pkg, val);
                }
            }
        }

        if pkgerrs.all_ok() {
            Ok(ret)
        } else {
            Err(From::from(pkgerrs))
        }
    }

    /// Return the set of features resolved for a particular package
    pub fn resolved_features(&self, pkg: &Manifest) -> impl Iterator<Item = &'meta str> {
        self.pkgid_to_node
            .get(&pkg.id)
            .unwrap()
            .features
            .iter()
            .map(String::as_str)
    }

    /// Return the resolved dependencies for a package
    /// This should generally be filtered by a target, but for the top-level we don't really care
    fn resolved_deps(
        &self,
        pkg: &'meta Manifest,
    ) -> impl Iterator<Item = (&'meta str, &'meta Manifest)> {
        let pkgid_to_pkg = Arc::clone(&self.pkgid_to_pkg);

        self.pkgid_to_node
            .get(&pkg.id)
            .unwrap()
            .deps
            .iter()
            .map(move |NodeDep { name, pkg, .. }| {
                (name.as_str(), pkgid_to_pkg.get(pkg).cloned().unwrap())
            })
    }

    /// Return the set of (unresolved) dependencies for a particular target.
    /// (Target must be the target for the given package.)
    fn deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
    ) -> impl Iterator<Item = &'meta ManifestDep> {
        assert!(pkg.targets.contains(tgt));

        pkg.dependencies.iter().filter(move |dep| {
            match dep.kind {
                DepKind::Normal => {
                    tgt.kind_lib() || tgt.kind_proc_macro() || tgt.kind_bin() || tgt.kind_cdylib()
                }
                DepKind::Dev => tgt.kind_bench() || tgt.kind_test() || tgt.kind_example(),
                DepKind::Build => tgt.kind_custom_build(),
            }
        })
    }

    /// Return resolved dependencies for a target
    pub fn resolved_deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
    ) -> impl Iterator<Item = ResolvedDep<'meta>> {
        // Unresolved dependency names
        let mut deps = HashMap::new();

        // Dependencies can be repeated with different target predicates;
        // retain them all.
        for dep in self.deps_for_target(pkg, tgt) {
            deps.entry(dep.name.as_str())
                .or_insert_with(HashSet::new)
                .insert(dep);
        }

        // Resolved dependencies filtered by deps for target
        self.resolved_deps(pkg).filter_map(move |(alias, dep)| {
            deps.get(dep.name.as_str()).map(|mdeps| {
                let mut platforms = vec![]; // empty = unconditional

                // If there are multiple manifestdeps then union all the
                // target predicates, where "unconditional" beats all.
                // (This is probably very over-engineered because all the times
                // this happens seem to be unconditional OR condition).
                for mdep in mdeps {
                    if let Some(plat) = &mdep.target {
                        match PlatformPredicate::parse(&plat) {
                            Ok(pred) => platforms.push(pred),
                            Err(err) => {
                                log::error!("Failed to parse predicate for {}: {}", dep, err);
                                continue;
                            }
                        }
                    } else {
                        // No platform condition = unconditional
                        platforms = vec![];
                        break;
                    }
                }

                ResolvedDep {
                    alias,
                    package: dep,
                    platform: match &*platforms {
                        [] => None,
                        [plat] => Some(format!("cfg({})", plat).into()),
                        _ => Some(format!("cfg({})", PlatformPredicate::Any(platforms)).into()),
                    },
                }
            })
        })
    }
}
