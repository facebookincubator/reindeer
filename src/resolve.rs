/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! There is a many-to-one relationship between ManifestDep and NodeDep. One
//! ManifestDep is always fulfilled by a single NodeDep. But one NodeDep might
//! fulfill many ManifestDep. For example in this case:
//!
//! ```toml
//! [dependencies]
//! example = "1"
//!
//! [target.'cfg(...)'.dependencies]
//! example = { version = "1", features = ["feat"] }
//! ```
//!
//! This package would have 2 ManifestDep dependencies on `example` in the
//! unresolved dependency graph, both satisfied by the same 1 NodeDep on
//! `example` in the resolved graph.
//!
//! The logic in this module is for finding which NodeDep fulfills a particular
//! ManifestDep. A ManifestDep can involve multiple crates (for example a
//! package's library and multiple of its binaries). We pick any one of those
//! and find which NodeDep provides that crate. The same NodeDep will provide
//! all the other crates in this ManifestDep too.

use std::collections::HashMap;

use anyhow::bail;
use semver::VersionReq;

use crate::cargo::ArtifactKind;
use crate::cargo::DepKind;
use crate::cargo::Manifest;
use crate::cargo::ManifestDep;
use crate::cargo::Node;
use crate::cargo::NodeDep;
use crate::cargo::NodeDepKind;
use crate::cargo::PkgId;
use crate::cargo::TargetKind;
use crate::platform::PlatformExpr;

pub struct DepIndex<'meta> {
    pkgid: &'meta PkgId,
    deps: HashMap<
        (&'meta str, DepKind, &'meta Option<PlatformExpr>),
        Vec<(&'meta NodeDep, &'meta NodeDepKind, &'meta Manifest)>,
    >,
}

impl<'meta> DepIndex<'meta> {
    pub fn new(
        pkgid_to_pkg: &HashMap<&'meta PkgId, &'meta Manifest>,
        pkgid_to_node: &HashMap<&'meta PkgId, &'meta Node>,
        pkgid: &'meta PkgId,
    ) -> Self {
        let mut deps = HashMap::new();
        for node_dep in &pkgid_to_node[pkgid].deps {
            let manifest = pkgid_to_pkg[&node_dep.pkg];
            for dep_kind in &node_dep.dep_kinds {
                deps.entry((manifest.name.as_str(), dep_kind.kind, &dep_kind.target))
                    .or_insert_with(Vec::new)
                    .push((node_dep, dep_kind, manifest));
            }
        }
        DepIndex { pkgid, deps }
    }

    pub fn resolve(&self, manifest_dep: &'meta ManifestDep) -> anyhow::Result<&'meta NodeDep> {
        let Some(candidates) = self.deps.get(&(
            manifest_dep.name.as_str(),
            manifest_dep.kind,
            &manifest_dep.target,
        )) else {
            bail!(
                "No candidates found for dependency {} of {}",
                manifest_dep.name,
                self.pkgid,
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
                "Multiple matches found for dependency {} of {}",
                manifest_dep.name,
                self.pkgid,
            );
        }

        let Some(unique_match) = matching_node_dep else {
            bail!(
                "No match found for dependency {} of {}",
                manifest_dep.name,
                self.pkgid,
            );
        };

        Ok(unique_match)
    }
}
