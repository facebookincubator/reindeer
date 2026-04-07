/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Short version naming for Buck target names.
//!
//! Uses the leftmost non-zero version component as the compatibility
//! boundary: `>=1.0.0` uses major only (e.g. `1`), `0.y.z` where
//! `y >= 1` uses major.minor (e.g. `0.2`), and `0.0.z` uses the full
//! version (e.g. `0.0.3`). This module resolves collisions when
//! multiple crates share the same short version slot.

use foldhash::HashMap;
use foldhash::HashSet;

use crate::cargo::Manifest;
use crate::cargo::Source;

/// Returns the short version string for a semver version.
///
/// Uses the leftmost non-zero component as the compatibility boundary:
/// - `>=1.0.0`: major only (e.g. `"3"`)
/// - `0.y.z` where `y >= 1`: major.minor (e.g. `"0.2"`)
/// - `0.0.z`: full version (e.g. `"0.0.3"`)
fn short_version(version: &semver::Version) -> String {
    if version.major >= 1 {
        version.major.to_string()
    } else if version.minor >= 1 {
        format!("{}.{}", version.major, version.minor)
    } else {
        format!("{}.{}.{}", version.major, version.minor, version.patch)
    }
}

/// Tracks which version slots have multiple occupants so we can disambiguate.
///
/// When multiple packages share the same short version slot, CratesIo keeps
/// the clean name and all other sources get a suffix.
#[derive(Debug)]
pub struct CollisionInfo {
    /// Set of (crate_name, short_version) keys that have more than one package.
    collisions: HashSet<(String, String)>,
}

impl CollisionInfo {
    /// Build collision info by scanning all resolved packages.
    pub fn new(packages: &[&Manifest]) -> Self {
        let mut counts: HashMap<(String, String), u32> = HashMap::default();
        for pkg in packages {
            *counts
                .entry((pkg.name.clone(), short_version(&pkg.version)))
                .or_default() += 1;
        }
        let collisions = counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(key, _)| key)
            .collect();
        CollisionInfo { collisions }
    }

    /// Returns the version string for use in target names.
    ///
    /// The short form uses the leftmost non-zero component: `"3"` for
    /// >=1.0.0, `"0.2"` for 0.y.z where y >= 1, `"0.0.3"` for 0.0.z.
    ///
    /// When only one version exists for a short version slot, or when this
    /// package is from CratesIo, returns the short form. Otherwise appends
    /// a disambiguation suffix (e.g. `"1-a1b2c3d4"`).
    ///
    /// Suffix rules:
    /// - Git gets `-{hash8}`, Local gets `-local`
    /// - Unrecognized falls back to full semver (matches vendored dir name)
    pub fn target_version(&self, pkg: &Manifest) -> String {
        let short = short_version(&pkg.version);

        let needs_suffix = !matches!(pkg.source, Source::CratesIo)
            && self.collisions.contains(&(pkg.name.clone(), short.clone()));

        if !needs_suffix {
            return short;
        }

        let suffix = match &pkg.source {
            Source::CratesIo => unreachable!(),
            Source::Git { commit_hash, .. } => commit_hash.chars().take(8).collect(),
            Source::Local => "local".to_owned(),
            Source::Unrecognized(_) => return pkg.version.to_string(),
        };

        format!("{}-{}", short, suffix)
    }

    /// Returns `"{name}-{target_version}"` for use in non-split target names.
    pub fn target_display(&self, pkg: &Manifest) -> String {
        format!("{}-{}", pkg.name, self.target_version(pkg))
    }
}

#[cfg(test)]
mod tests {
    use semver::Version;

    use super::*;

    fn make_manifest(name: &str, version: &str, source: Source) -> Manifest {
        Manifest {
            name: name.to_owned(),
            version: Version::parse(version).unwrap(),
            id: test_package_id(name, version),
            license: None,
            license_file: None,
            description: None,
            source,
            dependencies: vec![],
            targets: vec![],
            features: Default::default(),
            manifest_path: Default::default(),
            authors: vec![],
            repository: None,
            readme: None,
            edition: crate::cargo::Edition::Rust2021,
            links: None,
        }
    }

    fn test_package_id(name: &str, version: &str) -> cargo::core::PackageId {
        use cargo::core::PackageId;
        use cargo::util::interning::InternedString;

        let ver = Version::parse(version).unwrap();
        let source_id = cargo::core::SourceId::from_url(
            "registry+https://github.com/rust-lang/crates.io-index",
        )
        .unwrap();
        PackageId::new(InternedString::new(name), ver, source_id)
    }

    #[test]
    fn stable_cratesio_uses_major_only() {
        let m = make_manifest("unicorn-factory", "3.14.159", Source::CratesIo);
        let info = CollisionInfo::new(&[&m]);
        assert_eq!(info.target_version(&m), "3");
        assert_eq!(info.target_display(&m), "unicorn-factory-3");
    }

    #[test]
    fn cratesio_plus_git_at_same_major() {
        let m_crates = make_manifest("flux-capacitor", "1.21.0", Source::CratesIo);
        let m_git = make_manifest(
            "flux-capacitor",
            "1.21.3",
            Source::Git {
                repo: "https://github.com/doc-brown/flux-capacitor".to_owned(),
                commit_hash: "88mph88mph88mph88mph88mph88mph88".to_owned(),
            },
        );
        let info = CollisionInfo::new(&[&m_crates, &m_git]);

        assert_eq!(info.target_version(&m_crates), "1");
        assert_eq!(info.target_version(&m_git), "1-88mph88m");
    }

    #[test]
    fn two_git_versions_no_cratesio() {
        let m1 = make_manifest(
            "catapult",
            "2.0.1",
            Source::Git {
                repo: "https://github.com/siege-engines/catapult".to_owned(),
                commit_hash: "trebuchet1trebuchet1trebuchet1tre".to_owned(),
            },
        );
        let m2 = make_manifest(
            "catapult",
            "2.0.9",
            Source::Git {
                repo: "https://github.com/siege-engines/catapult".to_owned(),
                commit_hash: "onager999onager999onager999onager".to_owned(),
            },
        );
        let info = CollisionInfo::new(&[&m1, &m2]);

        // Neither owns the slot; both get suffixes
        assert_eq!(info.target_version(&m1), "2-trebuche");
        assert_eq!(info.target_version(&m2), "2-onager99");
    }

    #[test]
    fn local_source_disambiguation() {
        let m_crates = make_manifest("baguette", "1.0.200", Source::CratesIo);
        let m_local = make_manifest("baguette", "1.0.201", Source::Local);
        let info = CollisionInfo::new(&[&m_crates, &m_local]);

        assert_eq!(info.target_version(&m_crates), "1");
        assert_eq!(info.target_version(&m_local), "1-local");
    }

    #[test]
    fn different_majors_no_collision() {
        let m1 = make_manifest("pancake-stack", "1.28.1", Source::CratesIo);
        let m2 = make_manifest("pancake-stack", "2.0.0", Source::CratesIo);
        let info = CollisionInfo::new(&[&m1, &m2]);

        assert_eq!(info.target_version(&m1), "1");
        assert_eq!(info.target_version(&m2), "2");
    }

    #[test]
    fn different_crate_names_same_version_no_collision() {
        let m1 = make_manifest("warp-drive", "1.28.1", Source::CratesIo);
        let m2 = make_manifest("hyperdrive", "1.28.1", Source::CratesIo);
        let info = CollisionInfo::new(&[&m1, &m2]);

        assert_eq!(info.target_version(&m1), "1");
        assert_eq!(info.target_version(&m2), "1");
    }

    #[test]
    fn single_git_version_owns_slot() {
        let m = make_manifest(
            "dragon-breath",
            "0.5.3",
            Source::Git {
                repo: "https://github.com/fantasy-forge/dragon-breath".to_owned(),
                commit_hash: "f1ames0fglory0fdragon0flegend00".to_owned(),
            },
        );
        let info = CollisionInfo::new(&[&m]);
        assert_eq!(info.target_version(&m), "0.5");
    }

    #[test]
    fn single_local_version_owns_slot() {
        let m = make_manifest("sourdough-starter", "2.1.0", Source::Local);
        let info = CollisionInfo::new(&[&m]);
        assert_eq!(info.target_version(&m), "2");
    }

    #[test]
    fn git_plus_local_both_get_suffix() {
        let m_git = make_manifest(
            "chimera",
            "3.2.1",
            Source::Git {
                repo: "https://github.com/mythical-beasts/chimera".to_owned(),
                commit_hash: "l10nh3adl10nh3adl10nh3adl10nh3ad".to_owned(),
            },
        );
        let m_local = make_manifest("chimera", "3.2.9", Source::Local);
        let info = CollisionInfo::new(&[&m_git, &m_local]);

        assert_eq!(info.target_version(&m_git), "3-l10nh3ad");
        assert_eq!(info.target_version(&m_local), "3-local");
    }

    #[test]
    fn unrecognized_source_disambiguation() {
        let m_crates = make_manifest("mystery-crate", "1.0.0", Source::CratesIo);
        let m_unrec = make_manifest(
            "mystery-crate",
            "1.0.5",
            Source::Unrecognized("fell-off-a-truck".to_owned()),
        );
        let info = CollisionInfo::new(&[&m_crates, &m_unrec]);

        assert_eq!(info.target_version(&m_crates), "1");
        // Falls back to full semver (matches vendored dir name)
        assert_eq!(info.target_version(&m_unrec), "1.0.5");
    }

    #[test]
    fn zero_major_uses_major_minor() {
        let m = make_manifest("baby-steps", "0.1.2", Source::CratesIo);
        let info = CollisionInfo::new(&[&m]);
        assert_eq!(info.target_version(&m), "0.1");
        assert_eq!(info.target_display(&m), "baby-steps-0.1");
    }

    #[test]
    fn zero_major_different_minors_no_collision() {
        let m1 = make_manifest("tiny-dancer", "0.1.5", Source::CratesIo);
        let m2 = make_manifest("tiny-dancer", "0.2.0", Source::CratesIo);
        let info = CollisionInfo::new(&[&m1, &m2]);

        assert_eq!(info.target_version(&m1), "0.1");
        assert_eq!(info.target_version(&m2), "0.2");
    }

    #[test]
    fn zero_zero_uses_full_semver() {
        let m = make_manifest("embryo", "0.0.3", Source::CratesIo);
        let info = CollisionInfo::new(&[&m]);
        assert_eq!(info.target_version(&m), "0.0.3");
        assert_eq!(info.target_display(&m), "embryo-0.0.3");
    }

    #[test]
    fn zero_zero_different_patches_no_collision() {
        let m1 = make_manifest("primordial-soup", "0.0.1", Source::CratesIo);
        let m2 = make_manifest("primordial-soup", "0.0.2", Source::CratesIo);
        let info = CollisionInfo::new(&[&m1, &m2]);

        assert_eq!(info.target_version(&m1), "0.0.1");
        assert_eq!(info.target_version(&m2), "0.0.2");
    }

    #[test]
    fn zero_major_collision_at_same_minor() {
        let m_crates = make_manifest("hatchling", "0.3.1", Source::CratesIo);
        let m_git = make_manifest(
            "hatchling",
            "0.3.7",
            Source::Git {
                repo: "https://github.com/nest/hatchling".to_owned(),
                commit_hash: "ch1rp1ngch1rp1ngch1rp1ngch1rp1ng".to_owned(),
            },
        );
        let info = CollisionInfo::new(&[&m_crates, &m_git]);

        assert_eq!(info.target_version(&m_crates), "0.3");
        assert_eq!(info.target_version(&m_git), "0.3-ch1rp1ng");
    }

    #[test]
    fn collision_info_insertion_order_independent() {
        let m_crates = make_manifest("boomerang", "7.7.1", Source::CratesIo);
        let m_git = make_manifest(
            "boomerang",
            "7.7.3",
            Source::Git {
                repo: "https://github.com/always-comes-back/boomerang".to_owned(),
                commit_hash: "c0meb4ckc0meb4ckc0meb4ckc0meb4ck".to_owned(),
            },
        );

        // Order 1: crates first
        let info1 = CollisionInfo::new(&[&m_crates, &m_git]);
        // Order 2: git first
        let info2 = CollisionInfo::new(&[&m_git, &m_crates]);

        assert_eq!(
            info1.target_version(&m_crates),
            info2.target_version(&m_crates)
        );
        assert_eq!(info1.target_version(&m_git), info2.target_version(&m_git));
    }

    #[test]
    fn git_hash_truncated_to_8_chars() {
        let m_crates = make_manifest("telescope", "1.0.0", Source::CratesIo);
        let m_git = make_manifest(
            "telescope",
            "1.0.1",
            Source::Git {
                repo: "https://github.com/stargazers/telescope".to_owned(),
                commit_hash: "5ee5tar5f0reverand3verand3verand".to_owned(),
            },
        );
        let info = CollisionInfo::new(&[&m_crates, &m_git]);

        assert_eq!(info.target_version(&m_git), "1-5ee5tar5");
    }
}
