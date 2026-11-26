/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Where the generated BUCK file finds a package's files.

use std::fmt;
use std::fmt::Display;
use std::path::Component;
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
use crate::path::normalize_path;
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
    /// ":foo-1.0.0.crate[src/lib.rs]", given its path relative to the manifest
    /// directory as it appears in Cargo.toml, a fixup, or a glob match.
    pub fn file(
        &self,
        relative_to_manifest_dir: &Path,
    ) -> Result<SubtargetOrPath, UnreachableFile> {
        if let Some(Component::RootDir | Component::Prefix(_)) =
            relative_to_manifest_dir.components().next()
        {
            return Err(UnreachableFile::Absolute);
        }
        // Normalized within the root that the path must not escape.
        let within_root = |root: &Path| {
            let path = normalize_path(&root.join(relative_to_manifest_dir));
            match path.components().next() {
                Some(Component::ParentDir) => Err(UnreachableFile::OutsideRoot),
                _ => Ok(path),
            }
        };
        let subtarget = |relative: PathBuf| {
            let valid = relative.to_str().is_some_and(|name| {
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_\\/.=,+-".contains(&b))
            });
            if !valid {
                return Err(UnreachableFile::InvalidSubtargetName);
            }
            Ok(SubtargetOrPath::Subtarget(Subtarget {
                target: self.archive_target().unwrap(),
                relative: BuckPath(relative),
            }))
        };
        match self {
            // Path dependencies may use files elsewhere in the workspace, such
            // as `license-file = "../LICENSE"`.
            PackageLocation::Local { dir } => {
                Ok(SubtargetOrPath::Path(BuckPath(within_root(dir)?)))
            }
            PackageLocation::Vendored { dir } => Ok(SubtargetOrPath::Path(BuckPath(
                dir.join(within_root(Path::new(""))?),
            ))),
            PackageLocation::Crate { .. } => subtarget(within_root(Path::new(""))?),
            // A git_fetch is of the whole repository.
            PackageLocation::Git { subdir, .. } => subtarget(within_root(subdir)?),
        }
    }
}

/// Why a package's file can't be referred to from the BUCK file.
#[derive(Debug, PartialEq, Eq)]
pub enum UnreachableFile {
    /// An absolute path in Cargo.toml only means something on one machine.
    Absolute,
    /// Buck rejects `..` in source paths, and there is nothing outside an
    /// archive to refer to.
    OutsideRoot,
    /// Buck provider names allow only `[A-Za-z0-9_\\/.=,+-]`.
    InvalidSubtargetName,
}

impl Display for UnreachableFile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            UnreachableFile::Absolute => "is an absolute path",
            UnreachableFile::OutsideRoot => "is outside the package",
            UnreachableFile::InvalidSubtargetName => "is not a valid Buck subtarget name",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(location: &PackageLocation, path: &str) -> Result<String, UnreachableFile> {
        let file = location.file(Path::new(path))?;
        Ok(serde_json::to_value(file)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned())
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
            file(&git("serde"), "build.rs").as_deref(),
            Ok(":serde-643d78919f35d883.git[serde/build.rs]"),
        );
        assert_eq!(
            git("serde").mapped_manifest_dir(),
            Path::new("serde-643d78919f35d883/serde"),
        );
        // A crate at the root of its repository.
        assert_eq!(
            file(&git(""), "src/lib.rs").as_deref(),
            Ok(":serde-643d78919f35d883.git[src/lib.rs]"),
        );
        assert_eq!(
            git("").mapped_manifest_dir(),
            Path::new("serde-643d78919f35d883"),
        );
    }

    #[test]
    fn unreachable_files() {
        let local = PackageLocation::Local {
            dir: PathBuf::from("crates/foo"),
        };
        let vendored = PackageLocation::Vendored {
            dir: PathBuf::from("vendor/foo-1.0.0"),
        };
        let krate = PackageLocation::Crate {
            name: "foo-1.0.0.crate".to_owned(),
        };
        let ok = |s: &str| Ok(s.to_owned());

        // A git workspace member can use the workspace's files, but nothing
        // outside the repository.
        assert_eq!(
            file(&git("serde"), "../LICENSE"),
            ok(":serde-643d78919f35d883.git[LICENSE]"),
        );
        assert_eq!(
            file(&git("serde"), "../../LICENSE"),
            Err(UnreachableFile::OutsideRoot),
        );
        // A crate archive or vendored crate holds only the crate.
        assert_eq!(
            file(&krate, "../LICENSE"),
            Err(UnreachableFile::OutsideRoot)
        );
        assert_eq!(
            file(&vendored, "../LICENSE"),
            Err(UnreachableFile::OutsideRoot)
        );
        assert_eq!(file(&vendored, "./LICENSE"), ok("vendor/foo-1.0.0/LICENSE"));
        // Path dependencies are confined to the third-party directory.
        assert_eq!(file(&local, "../../LICENSE"), ok("LICENSE"));
        assert_eq!(
            file(&local, "../../../LICENSE"),
            Err(UnreachableFile::OutsideRoot)
        );

        // Subtargets are Buck provider names, which can't contain spaces.
        assert_eq!(
            file(&krate, "License Text"),
            Err(UnreachableFile::InvalidSubtargetName),
        );
        assert_eq!(file(&local, "License Text"), ok("crates/foo/License Text"));

        assert_eq!(
            file(&local, "/repo/LICENSE"),
            Err(UnreachableFile::Absolute)
        );
        assert_eq!(
            file(&krate, "/repo/LICENSE"),
            Err(UnreachableFile::Absolute)
        );
    }
}
