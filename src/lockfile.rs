/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;

use anyhow::Context;
use serde::Deserialize;
use serde::Deserializer;

use crate::Paths;
use crate::cargo::Manifest;
use crate::cargo::Source;

#[derive(Deserialize, Debug)]
pub struct Lockfile {
    #[allow(dead_code)]
    pub version: Hopefully4,
    #[serde(rename = "package")]
    pub packages: Vec<LockfilePackage>,
}

impl Lockfile {
    pub fn load(paths: &Paths) -> anyhow::Result<Self> {
        let cargo_lock_content = fs::read_to_string(&paths.lockfile_path)
            .with_context(|| format!("Failed to load {}", paths.lockfile_path.display()))?;

        let mut lockfile: Lockfile = toml::from_str(&cargo_lock_content)
            .with_context(|| format!("Failed to parse {}", paths.lockfile_path.display()))?;

        lockfile.packages.sort_by(|a, b| {
            let a = (&a.name, &a.version, &a.source);
            let b = (&b.name, &b.version, &b.source);
            a.cmp(&b)
        });

        Ok(lockfile)
    }

    pub fn find(&self, manifest: &Manifest) -> Option<&LockfilePackage> {
        let key = (&manifest.name, &manifest.version, &manifest.source);
        match self
            .packages
            .binary_search_by(|pkg| (&pkg.name, &pkg.version, &pkg.source).cmp(&key))
        {
            Ok(i) => Some(&self.packages[i]),
            Err(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct Hopefully4;

#[derive(Deserialize, Debug)]
pub struct LockfilePackage {
    pub name: String,
    pub version: semver::Version,
    pub source: Source,
    pub checksum: Option<String>,
}

impl<'de> Deserialize<'de> for Hopefully4 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = usize::deserialize(deserializer)?;
        if version != 4 {
            log::warn!("Unrecognized Cargo.lock format version: {}", version);
        }
        Ok(Hopefully4)
    }
}
