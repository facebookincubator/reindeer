/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;
use std::iter;
use std::iter::Empty;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use globset::Glob;
use globset::GlobBuilder;
use globset::GlobSet;
use globset::GlobSetBuilder;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use walkdir::WalkDir;

#[derive(Default, Debug)]
pub struct SerializableGlobSet {
    vec: Vec<Glob>,
    globset: GlobSet,
}

impl SerializableGlobSet {
    pub fn is_empty(&self) -> bool {
        self.globset.is_empty()
    }

    pub fn is_match(&self, path: impl AsRef<Path>) -> bool {
        self.globset.is_match(path)
    }
}

impl<'de> Deserialize<'de> for SerializableGlobSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let vec: Vec<Glob> = Deserialize::deserialize(deserializer)?;

        let mut builder = GlobSetBuilder::new();
        for glob in &vec {
            builder.add(glob.clone());
        }

        let globset = builder.build().map_err(serde::de::Error::custom)?;
        Ok(SerializableGlobSet { vec, globset })
    }
}

impl Serialize for SerializableGlobSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.vec.serialize(serializer)
    }
}

pub struct Globs {
    original_globs: Vec<String>,
    globset: GlobSet,
    exceptset: GlobSet,
    /// Sequence number of every glob pattern that has matched any path so far.
    globs_used: HashSet<usize>,
}

pub const NO_EXCLUDE: Empty<&str> = iter::empty();

impl Globs {
    pub fn new(
        globs: impl IntoIterator<Item = impl AsRef<str>>,
        excepts: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> anyhow::Result<Self> {
        let globs: Vec<String> = globs.into_iter().map(|g| g.as_ref().to_owned()).collect();

        let mut builder = GlobSetBuilder::new();
        for glob in &globs {
            builder.add(GlobBuilder::new(glob).literal_separator(true).build()?);
        }
        let globset = builder.build()?;

        let mut builder = GlobSetBuilder::new();
        for except in excepts {
            builder.add(
                GlobBuilder::new(except.as_ref())
                    .literal_separator(true)
                    .build()?,
            );
        }
        let exceptset = builder.build()?;

        Ok(Globs {
            original_globs: globs,
            globset,
            exceptset,
            globs_used: HashSet::new(),
        })
    }

    /// Returns relative paths (relative to `dir`) of all the matching files.
    pub fn walk<T: AsRef<Path>>(&mut self, dir: T) -> impl Iterator<Item = PathBuf> + use<T> {
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
                let matches = self.globset.matches(path);
                let found = !matches.is_empty();
                self.globs_used.extend(matches);
                if found && !self.exceptset.is_match(path) {
                    Some(path.to_owned())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn check_all_globs_used(&self) -> anyhow::Result<()> {
        if self.globs_used.len() == self.globset.len() {
            return Ok(());
        }
        let mut unmatched = Vec::new();
        for (idx, original) in self.original_globs.iter().enumerate() {
            if !self.globs_used.contains(&idx) {
                unmatched.push(original);
            }
        }
        bail!("Unmatched globs: {:?}", unmatched);
    }
}
