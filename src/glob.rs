/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::io::ErrorKind;
use std::ops::Range;
use std::path;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use globset::GlobBuilder;
use globset::GlobMatcher;
use globset::GlobSet;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::Visitor;
use toml::Spanned;
use walkdir::WalkDir;

use crate::gitignore::Gitignore;
use crate::path::normalize_path;
use crate::unused::UnusedFixups;

pub struct SerializedGlob {
    matcher: GlobMatcher,
}

impl<'de> Deserialize<'de> for SerializedGlob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SerializedGlobVisitor;

        impl<'de> Visitor<'de> for SerializedGlobVisitor {
            type Value = SerializedGlob;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("glob")
            }

            fn visit_str<E>(self, string: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let matcher = GlobBuilder::new(string)
                    .literal_separator(true)
                    .build()
                    .map_err(E::custom)?
                    .compile_matcher();
                Ok(SerializedGlob { matcher })
            }
        }

        deserializer.deserialize_str(SerializedGlobVisitor)
    }
}

#[derive(Debug)]
pub struct TrackedGlob {
    matcher: GlobMatcher,
    span: Range<usize>,
    used: AtomicBool,
}

impl TrackedGlob {
    /// Used for selecting the right outermost directory to walk when matching
    /// this glob.
    pub fn components(&self) -> path::Components<'_> {
        Path::new(self.matcher.glob().glob()).components()
    }

    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        let is_match = self.matcher.is_match(path);
        if is_match {
            self.mark_used();
        }
        is_match
    }

    pub fn mark_used(&self) {
        self.used.store(true, Ordering::Relaxed);
    }
}

impl<'de> Deserialize<'de> for TrackedGlob {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let glob = Spanned::<SerializedGlob>::deserialize(deserializer)?;
        Ok(TrackedGlob {
            span: glob.span(),
            matcher: glob.into_inner().matcher,
            used: AtomicBool::new(false),
        })
    }
}

#[derive(Default, Debug)]
pub struct TrackedGlobSet {
    vec: Vec<TrackedGlob>,
    globset: GlobSet,
}

impl TrackedGlobSet {
    pub fn is_empty(&self) -> bool {
        self.globset.is_empty()
    }

    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        let matches = self.globset.matches(path);
        for &i in &matches {
            self.vec[i].mark_used();
        }
        !matches.is_empty()
    }

    pub fn collect_unused_globs(&self, unused: &mut UnusedFixups, pkg: &str, toml: &str) {
        for glob in &self.vec {
            if !glob.used.load(Ordering::Relaxed) {
                unused.globs.insert(
                    (pkg.to_owned(), glob.span.start),
                    (
                        toml[..glob.span.start].split('\n').count(),
                        glob.matcher.glob().clone(),
                    ),
                );
            }
        }
    }
}

impl<'de> Deserialize<'de> for TrackedGlobSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<TrackedGlob> = Deserialize::deserialize(deserializer)?;

        let mut builder = GlobSet::builder();
        for tracked in &vec {
            builder.add(tracked.matcher.glob().clone());
        }

        let globset = builder.build().map_err(serde::de::Error::custom)?;
        Ok(TrackedGlobSet { vec, globset })
    }
}

impl<'a> IntoIterator for &'a TrackedGlobSet {
    type Item = &'a TrackedGlob;
    type IntoIter = std::slice::Iter<'a, TrackedGlob>;

    fn into_iter(self) -> Self::IntoIter {
        self.vec.iter()
    }
}

pub struct Globs<'a> {
    globset: GlobSetKind<'a>,
    exceptset: GlobSetKind<'a>,
}

impl<'a> Globs<'a> {
    pub fn new(globs: impl Into<GlobSetKind<'a>>, excepts: impl Into<GlobSetKind<'a>>) -> Self {
        Globs {
            globset: globs.into(),
            exceptset: excepts.into(),
        }
    }

    /// Returns matching non-ignored files relative to `dir`.
    pub fn walk(
        &self,
        dir: impl AsRef<Path>,
        gitignore: &Gitignore,
    ) -> anyhow::Result<Vec<PathBuf>> {
        // `extra_srcs` can reach sibling crates through `..` components.
        let dir = normalize_path(dir.as_ref());
        let mut result = Vec::new();
        let walk = WalkDir::new(&dir)
            .into_iter()
            .filter_entry(|entry| !gitignore.is_ignored(entry.path(), entry.file_type().is_dir()));
        for entry in walk {
            let entry = match entry {
                Ok(entry) => entry,
                Err(walkdir_error) => {
                    if let Some(io_error) = walkdir_error.io_error()
                        && io_error.kind() == ErrorKind::NotFound
                    {
                        // This can happen in a correctly-written fixup that applies
                        // to multiple library versions, with `extra_srcs` containing
                        // some globs referring to directories that only exist within
                        // a subset of the versions.
                        //
                        // If a glob does not match any file in any library version,
                        // that gets reported as an error by a different codepath.
                        continue;
                    }
                    return Err(anyhow::Error::new(walkdir_error)
                        .context(format!("failed to walk {}", dir.display())));
                }
            };
            if entry.file_type().is_dir() {
                continue;
            }
            let path = entry
                .path()
                .strip_prefix(&dir)
                .expect("walkdir produced paths not inside intended dir");
            if self.globset.is_match(path) && !self.exceptset.is_match(path) {
                result.push(path.to_owned());
            }
        }
        Ok(result)
    }
}

#[derive(Default)]
pub enum GlobSetKind<'a> {
    TrackedSingle(&'a TrackedGlob),
    TrackedSet(&'a TrackedGlobSet),
    UntrackedSet(GlobSet),
    #[default]
    Empty,
}

pub const NO_EXCLUDE: GlobSetKind = GlobSetKind::Empty;

impl<'a> GlobSetKind<'a> {
    pub fn from_iter(globs: impl IntoIterator<Item: AsRef<Path>>) -> Result<Self, globset::Error> {
        let mut builder = GlobSet::builder();
        for path in globs {
            let path = &*path.as_ref().to_string_lossy();
            builder.add(GlobBuilder::new(path).literal_separator(true).build()?);
        }
        let globset = builder.build()?;
        Ok(GlobSetKind::UntrackedSet(globset))
    }

    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        match self {
            GlobSetKind::TrackedSingle(glob) => glob.is_match(path),
            GlobSetKind::TrackedSet(globset) => globset.is_match(path),
            GlobSetKind::UntrackedSet(globset) => globset.is_match(path),
            GlobSetKind::Empty => false,
        }
    }
}

impl<'a> From<&'a TrackedGlob> for GlobSetKind<'a> {
    fn from(glob: &'a TrackedGlob) -> Self {
        GlobSetKind::TrackedSingle(glob)
    }
}

impl<'a> From<&'a TrackedGlobSet> for GlobSetKind<'a> {
    fn from(globset: &'a TrackedGlobSet) -> Self {
        GlobSetKind::TrackedSet(globset)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::Paths;
    use crate::buck::BuckPath;
    use crate::config::VendorSourceConfig;
    use crate::gitignore::load_gitignore;

    fn all_files() -> Globs<'static> {
        Globs::new(
            GlobSetKind::from_iter(["**"]).expect("all-files glob should be valid"),
            NO_EXCLUDE,
        )
    }

    fn gitignore_at(third_party_dir: &Path) -> Gitignore {
        let paths = Paths {
            buck_package: BuckPath(PathBuf::new()),
            third_party_dir: third_party_dir.to_path_buf(),
            manifest_path: PathBuf::new(),
            lockfile_path: PathBuf::new(),
            cargo_home: PathBuf::new(),
        };
        load_gitignore(&paths, &VendorSourceConfig::default()).expect("gitignore should load")
    }

    #[test]
    fn walk_prunes_ignored_directories() {
        let temp = tempfile::tempdir().expect("temp directory should be created");
        let root = temp.path();
        fs::write(root.join(".gitignore"), "target/\n").expect("gitignore should be written");
        let target = root.join("target/debug");
        fs::create_dir_all(&target).expect("ignored directory should be created");
        fs::write(target.join("libexample.a"), "").expect("ignored file should be written");
        fs::write(root.join("visible.rs"), "").expect("visible file should be written");

        let gitignore = gitignore_at(root);
        let from_root = all_files()
            .walk(root, &gitignore)
            .expect("root directory should be walked");
        let from_ignored_root = all_files()
            .walk(root.join("target"), &gitignore)
            .expect("ignored directory should be walked");

        assert!(
            from_root.contains(&PathBuf::from("visible.rs")),
            "non-ignored files should remain in the walk"
        );
        assert!(
            !from_root.contains(&PathBuf::from("target/debug/libexample.a")),
            "ignored subtrees should be pruned"
        );
        assert!(
            from_ignored_root.is_empty(),
            "ignored root should have no matches"
        );
    }

    #[test]
    fn walk_filters_a_root_reached_through_parent_dirs() {
        let temp = tempfile::tempdir().expect("temp directory should be created");
        let root = temp.path();
        fs::write(root.join(".gitignore"), "sibling/generated.rs\n")
            .expect("gitignore should be written");
        fs::create_dir_all(root.join("vendor/example"))
            .expect("vendor directory should be created");
        fs::create_dir_all(root.join("sibling")).expect("sibling directory should be created");
        fs::write(root.join("sibling/generated.rs"), "").expect("ignored file should be written");
        fs::write(root.join("sibling/lib.rs"), "").expect("source file should be written");

        let via_parent = root.join("vendor/example/../../sibling");
        let actual = all_files()
            .walk(via_parent, &gitignore_at(root))
            .expect("sibling directory should be walked");

        assert_eq!(actual, vec![PathBuf::from("lib.rs")]);
    }
}
