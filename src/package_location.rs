/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Where the generated BUCK file finds a package's files.

use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;

use crate::Paths;
use crate::buck::BuckPath;
use crate::buck::Name;
use crate::buck::SubtargetOrPath;
use crate::buckify::find_repository_root;
use crate::buckify::short_name_for_git_repo;
use crate::cargo::Manifest;
use crate::cargo::Source;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::path::relative_path;
use crate::subtarget::Subtarget;

/// Where a package's files are, decided once from the vendoring mode and the
/// package's source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageLocation<'a> {
    /// A path dependency, in a directory relative to the third-party dir.
    Local { dir: PathBuf },
    /// `vendor = true`: a vendored directory relative to the third-party dir,
    /// such as "vendor/foo-1.0.0".
    Vendored { dir: PathBuf },
    /// An archive target named like "foo-1.0.0.crate": an `http_archive` with
    /// `vendor = false`, or with `vendor = "local-registry"` an
    /// `extract_archive`, which git dependencies are packaged into too.
    Crate { name: String },
    /// `vendor = false` git dependency: a `git_fetch` of the whole repository,
    /// with the package possibly in a subdirectory of it.
    Git {
        repo: &'a str,
        commit_hash: &'a str,
        short_name: String,
        subdir: PathBuf,
    },
}

impl<'a> PackageLocation<'a> {
    pub fn new(config: &Config, paths: &Paths, pkg: &'a Manifest) -> anyhow::Result<Self> {
        let manifest_dir = pkg.manifest_dir();
        Ok(match (&config.vendor, &pkg.source) {
            (_, Source::Local) => PackageLocation::Local {
                dir: relative_path(&paths.third_party_dir, manifest_dir),
            },
            (VendorConfig::Source(_), _) => PackageLocation::Vendored {
                dir: relative_path(&paths.third_party_dir, manifest_dir),
            },
            (VendorConfig::Off | VendorConfig::LocalRegistry, Source::Unrecognized(_)) => bail!(
                "`vendor = false` mode is supported only with exclusively crates.io and https git dependencies. \"{}\" {} is coming from some other source",
                pkg.name,
                pkg.version,
            ),
            (
                VendorConfig::Off,
                Source::Git {
                    repo, commit_hash, ..
                },
            ) => PackageLocation::Git {
                repo,
                commit_hash,
                short_name: short_name_for_git_repo(repo, commit_hash)?,
                subdir: relative_path(find_repository_root(manifest_dir)?, manifest_dir),
            },
            (
                VendorConfig::Off | VendorConfig::LocalRegistry,
                Source::CratesIo | Source::Git { .. },
            ) => PackageLocation::Crate {
                name: format!("{}-{}.crate", pkg.name, pkg.version),
            },
        })
    }

    /// The `http_archive`, `extract_archive` or `git_fetch` holding the
    /// package, if its files are not on disk.
    pub fn archive_target(&self) -> Option<Name> {
        match self {
            PackageLocation::Local { .. } | PackageLocation::Vendored { .. } => None,
            PackageLocation::Crate { name } => Some(Name(name.clone())),
            PackageLocation::Git { short_name, .. } => Some(Name(format!("{short_name}.git"))),
        }
    }

    /// The manifest directory, relative to the package's BUCK file for files
    /// on disk, or to where the archive's contents are mapped otherwise: e.g.
    /// "vendor/foo-1.0.0", "foo-1.0.0.crate", "serde-643d78919f35d883/serde".
    pub fn mapped_manifest_dir(&self) -> PathBuf {
        match self {
            PackageLocation::Local { dir } | PackageLocation::Vendored { dir } => dir.clone(),
            PackageLocation::Crate { name } => PathBuf::from(name),
            PackageLocation::Git {
                short_name, subdir, ..
            } => {
                let mut dir = PathBuf::from(short_name);
                if subdir.components().next().is_some() {
                    dir.push(subdir); // push("") would add a trailing slash
                }
                dir
            }
        }
    }

    /// The manifest directory for running a build script in, where it differs
    /// from the vendored directory the build script target is in.
    pub fn manifest_dir_subtarget(&self) -> Option<SubtargetOrPath> {
        match self {
            PackageLocation::Local { dir } => Some(SubtargetOrPath::Path(BuckPath(dir.clone()))),
            PackageLocation::Vendored { .. } | PackageLocation::Crate { .. } => None,
            PackageLocation::Git { subdir, .. } => Some(SubtargetOrPath::Subtarget(Subtarget {
                target: self.archive_target().unwrap(),
                relative: BuckPath(subdir.clone()),
            })),
        }
    }

    /// A file of the package, e.g. "vendor/foo-1.0.0/src/lib.rs" or
    /// ":foo-1.0.0.crate[src/lib.rs]".
    pub fn file(&self, relative_to_manifest_dir: &Path) -> SubtargetOrPath {
        match self {
            PackageLocation::Local { dir } | PackageLocation::Vendored { dir } => {
                SubtargetOrPath::Path(BuckPath(dir.join(relative_to_manifest_dir)))
            }
            PackageLocation::Crate { .. } => SubtargetOrPath::Subtarget(Subtarget {
                target: self.archive_target().unwrap(),
                relative: BuckPath(relative_to_manifest_dir.to_owned()),
            }),
            // A git_fetch is of the whole repository.
            PackageLocation::Git { subdir, .. } => SubtargetOrPath::Subtarget(Subtarget {
                target: self.archive_target().unwrap(),
                relative: BuckPath(subdir.join(relative_to_manifest_dir)),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(location: &PackageLocation, path: &str) -> String {
        let file = location.file(Path::new(path));
        serde_json::to_value(file)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn git(subdir: &str) -> PackageLocation<'static> {
        PackageLocation::Git {
            repo: "https://github.com/serde-rs/serde",
            commit_hash: "49d098debdf8b5c38bfb6868f455c6ce542c422c",
            short_name: "serde-643d78919f35d883".to_owned(),
            subdir: PathBuf::from(subdir),
        }
    }

    #[test]
    fn git_files_are_relative_to_the_repository() {
        // serde's crate is in serde/ of its repository.
        assert_eq!(
            file(&git("serde"), "build.rs"),
            ":serde-643d78919f35d883.git[serde/build.rs]",
        );
        assert_eq!(
            git("serde").mapped_manifest_dir(),
            Path::new("serde-643d78919f35d883/serde"),
        );
        // A crate at the root of its repository.
        assert_eq!(
            file(&git(""), "src/lib.rs"),
            ":serde-643d78919f35d883.git[src/lib.rs]",
        );
        assert_eq!(
            git("").mapped_manifest_dir(),
            Path::new("serde-643d78919f35d883"),
        );
    }
}
