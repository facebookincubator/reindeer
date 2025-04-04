/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Index for Cargo metadata, and various useful traversals.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;

use anyhow::Context as _;

use crate::buck::Name;
use crate::cargo::DepKind;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::Metadata;
use crate::cargo::Node;
use crate::cargo::NodeDep;
use crate::cargo::NodeDepKind;
use crate::cargo::PkgId;
use crate::cargo::TargetReq;
use crate::platform::PlatformExpr;

/// Index for interesting things in Cargo metadata
pub struct Index<'meta> {
    /// Map a PkgId to the Manifest (package) with its details
    pkgid_to_pkg: HashMap<&'meta PkgId, &'meta Manifest>,
    /// Map a PkgId to a Node (ie all the details of a resolve dependency)
    pkgid_to_node: HashMap<&'meta PkgId, &'meta Node>,
    /// Represents the Cargo.toml itself, if not a virtual manifest.
    pub root_pkg: Option<&'meta Manifest>,
    /// All packages considered part of the workspace.
    pub workspace_members: Vec<&'meta Manifest>,
    /// Faster lookup for workspace members
    workspace_packages: HashSet<&'meta PkgId>,
    /// Set of package IDs from which at least one target is public mapped to an optional rename
    public_packages: BTreeMap<&'meta PkgId, Option<&'meta str>>,
    /// The (possibly renamed) names of all packages which have at least one
    /// public target.
    public_package_names: BTreeSet<&'meta str>,
    /// Set of public targets. These consist of:
    /// - root_pkg, if it is being made public (aka "real", and not just a pseudo package)
    /// - first-order dependencies of root_pkg, including artifact dependencies
    public_targets: BTreeMap<(&'meta PkgId, TargetReq<'meta>), Option<&'meta str>>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDep<'meta> {
    pub package: &'meta Manifest,
    pub platform: Option<PlatformExpr>,
    pub rename: &'meta str,
    pub dep_kind: &'meta NodeDepKind,
}

impl<'meta> Index<'meta> {
    /// Construct an index for a set of Cargo metadata to allow convenient and efficient
    /// queries. The metadata represents a top level package and all its transitive
    /// dependencies.
    pub fn new(root_is_real: bool, metadata: &'meta Metadata) -> anyhow::Result<Index<'meta>> {
        let pkgid_to_pkg: HashMap<_, _> = metadata.packages.iter().map(|m| (&m.id, m)).collect();

        let root_pkg = metadata.resolve.root.as_ref().map(|root_pkgid| {
            *pkgid_to_pkg
                .get(root_pkgid)
                .expect("couldn't identify unambiguous top-level crate")
        });

        let top_levels = if root_is_real {
            Some(
                &root_pkg
                    .context("`include_top_level = true` is not supported on a virtual manifest")?
                    .id,
            )
        } else {
            None
        };

        let workspace_members: Vec<_> = metadata
            .workspace_default_members
            .iter()
            .filter_map(|pkgid| pkgid_to_pkg.get(pkgid).copied())
            .collect();

        let mut tmp = Index {
            pkgid_to_pkg,
            pkgid_to_node: metadata.resolve.nodes.iter().map(|n| (&n.id, n)).collect(),
            root_pkg,
            workspace_packages: workspace_members.iter().map(|x| &x.id).collect(),
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
        let public_targets = tmp
            .workspace_members
            .iter()
            .flat_map(|member| tmp.resolved_deps(member))
            .flat_map(|(rename, dep_kind, pkg)| {
                let target_req = dep_kind.target_req();
                let opt_rename = dep_renamed.get(rename).cloned();
                vec![((&pkg.id, target_req), opt_rename)]
            })
            .chain(top_levels.iter().flat_map(|pkgid| {
                [
                    ((*pkgid, TargetReq::Lib), None),
                    ((*pkgid, TargetReq::EveryBin), None),
                ]
            }))
            .collect::<BTreeMap<_, _>>();

        for ((id, _), rename) in public_targets.iter() {
            tmp.public_packages.insert(id, rename.clone());
            tmp.public_package_names
                .insert(if let &Some(rename) = rename {
                    rename
                } else {
                    &tmp.pkgid_to_pkg[id].name
                });
        }

        Ok(Index {
            public_targets,
            ..tmp
        })
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
        self.public_targets.contains_key(&(&pkg.id, target_req))
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
        pkg: &Manifest,
    ) -> impl Iterator<Item = (&'meta str, &'meta NodeDepKind, &'meta Manifest)> + '_ {
        self.pkgid_to_node
            .get(&pkg.id)
            .unwrap()
            .deps
            .iter()
            .flat_map(
                |NodeDep {
                     pkg,
                     name,
                     dep_kinds,
                 }| {
                    dep_kinds.iter().map(|dep_kind| {
                        (
                            name.as_deref().or(dep_kind.extern_name.as_deref()).unwrap(),
                            dep_kind,
                            self.pkgid_to_pkg.get(pkg).copied().unwrap(),
                        )
                    })
                },
            )
    }

    /// Return resolved dependencies for a target.
    pub fn resolved_deps_for_target(
        &self,
        pkg: &'meta Manifest,
        tgt: &'meta ManifestTarget,
    ) -> impl Iterator<Item = ResolvedDep<'meta>> + '_ {
        // Target must be the target for the given package.
        assert!(pkg.targets.contains(tgt));

        let mut resolved_deps = HashMap::new();

        for (rename, dep_kind, dep) in self.resolved_deps(pkg) {
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
                // Key by everything except `target`.
                let NodeDepKind {
                    kind,
                    target: _,
                    artifact,
                    extern_name,
                    compile_target,
                    bin_name,
                } = dep_kind;
                let (ref mut unconditional_deps, ref mut conditional_deps) = resolved_deps
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
                platform: dep_kind.target.clone(),
                rename,
                dep_kind,
            })
    }
}
