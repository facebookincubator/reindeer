/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::fmt;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UniverseConfig {
    /// A set of additional features to enable in this universe only.
    /// If omitted, the "default" feature will be enabled if one exists.
    pub features: Option<BTreeSet<String>>,
    /// When present, the universe will contain only crates reachable from this
    /// set. Crates outside the set will not be considered during feature
    /// resolution.
    #[serde(default)]
    pub include_crates: BTreeSet<String>,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct UniverseName(String);

impl Default for UniverseName {
    fn default() -> Self {
        Self("DEFAULT".to_owned())
    }
}

impl fmt::Display for UniverseName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for UniverseName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // More compact than a derived Debug impl
        let UniverseName(name) = self;
        write!(f, "UniverseName({name:?})")
    }
}

impl From<String> for UniverseName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

pub fn validate_universe_config(
    universe_name: &UniverseName,
    config: &UniverseConfig,
    index: &crate::index::Index,
) -> anyhow::Result<()> {
    for feature in config.features.iter().flatten() {
        if feature.contains('/') {
            anyhow::bail!(
                "Universe {universe_name} specifies features {feature:?}. \
                 Features containing '/' are not supported in universe configuration. \
                 Move this to a feature in the [features] section of Cargo.toml."
            );
        }
    }
    for krate in &config.include_crates {
        if !index.is_public_package_name(krate) {
            anyhow::bail!(
                "Universe {universe_name} specifies in `include_crates` \
                 a crate which is not a public package: {krate:?}. \
                 Add it as a dependency of the root crate \
                 or remove it from the `include_crates` list in Reindeer config."
            );
        };
    }
    Ok(())
}
