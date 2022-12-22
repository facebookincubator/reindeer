/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Deserializer;

use crate::cargo::Source;

#[derive(Deserialize, Debug)]
pub struct Lockfile {
    pub version: Hopefully3,
    #[serde(rename = "package")]
    pub packages: Vec<LockfilePackage>,
}

#[derive(Debug)]
pub struct Hopefully3;

#[derive(Deserialize, Debug)]
pub struct LockfilePackage {
    pub source: Option<Source>,
}

impl<'de> Deserialize<'de> for Hopefully3 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let version = usize::deserialize(deserializer)?;
        if version != 3 {
            log::warn!("Unrecognized Cargo.lock format version: {}", version);
        }
        Ok(Hopefully3)
    }
}
