/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::ops::Range;
use std::path;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use globset::Glob;
use globset::GlobBuilder;
use globset::GlobMatcher;
use globset::GlobSet;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::Visitor;
use toml::Spanned;
use walkdir::WalkDir;

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

    pub fn collect_unused_globs(&self, unused: &mut UnusedGlobs, pkg: &str, toml: &str) {
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

    /// Returns relative paths (relative to `dir`) of all the matching files.
    pub fn walk(&self, dir: impl AsRef<Path>) -> impl Iterator<Item = PathBuf> {
        let dir = dir.as_ref();
        WalkDir::new(dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| !entry.file_type().is_dir())
            .filter_map(move |entry| {
                let path = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walkdir produced paths not inside intended dir");
                if self.globset.is_match(path) && !self.exceptset.is_match(path) {
                    Some(path.to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .into_iter()
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

#[derive(Debug)]
pub struct UnusedGlobs {
    // key = (pkg, byte offset) for nicely sorted output
    // value = (line, glob)
    globs: BTreeMap<(String, usize), (usize, Glob)>,
}

impl UnusedGlobs {
    pub fn new() -> Self {
        UnusedGlobs {
            globs: BTreeMap::new(),
        }
    }

    pub fn check(self) -> anyhow::Result<()> {
        if self.globs.is_empty() {
            Ok(())
        } else {
            Err(anyhow::Error::new(self))
        }
    }
}

impl Error for UnusedGlobs {}

impl Display for UnusedGlobs {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Unused globs:")?;
        for ((pkg, _offset), (line, glob)) in &self.globs {
            write!(
                formatter,
                "\nfixups/{pkg}/fixups.toml line {line}: {glob:?} matches no files",
                glob = glob.glob(),
            )?;
        }
        Ok(())
    }
}
