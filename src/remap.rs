/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap as Map;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;

use crate::cargo::GitRef;
use crate::cargo::Source;
use crate::lockfile::Lockfile;

#[derive(Serialize, Deserialize)]
pub struct RemapConfig {
    #[serde(rename = "source", default)]
    pub sources: Map<String, RemapSource>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct RemapSource {
    pub directory: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    #[serde(rename = "replace-with")]
    pub replace_with: Option<String>,
}

/// Reads Cargo.lock and writes a .cargo/config remapping every source in the
/// lockfile to be provided by the vendored sources instead.
///
/// ```toml
/// [source.crates-io]
/// replace-with = "vendored-sources"
///
/// [source."https://github.com/facebookincubator/reindeer"]
/// git = "https://github.com/facebookincubator/reindeer"
/// branch = "main"
/// replace-with = "vendored-sources"
///
/// [source.vendored-sources]
/// directory = "/path/to/third-party-dir/vendor"
/// ```
pub fn write_remap_all_sources(
    cargo_config: &Path,
    third_party_dir: &Path,
    lockfile: &Lockfile,
) -> Result<()> {
    let mut sources = Map::new();
    for pkg in &lockfile.packages {
        let mut remap_source = RemapSource {
            replace_with: Some("vendored-sources".to_owned()),
            ..RemapSource::default()
        };
        let key = match &pkg.source {
            Source::CratesIo => "crates-io".to_owned(),
            Source::Git {
                repo,
                reference,
                commit_hash,
            } => {
                remap_source.git = Some(repo.clone());
                match reference {
                    GitRef::Revision => remap_source.rev = Some(commit_hash.clone()),
                    GitRef::Branch(branch) => remap_source.branch = Some(branch.clone()),
                    GitRef::Tag(tag) => remap_source.tag = Some(tag.clone()),
                    GitRef::Head => {}
                }
                repo.clone()
            }
            Source::Local | Source::Unrecognized(_) => continue,
        };
        sources.insert(key, remap_source);
    }

    if !sources.is_empty() {
        sources.insert(
            "vendored-sources".to_owned(),
            RemapSource {
                directory: Some(third_party_dir.join("vendor")),
                ..RemapSource::default()
            },
        );
    }

    let remap_config = RemapConfig { sources };
    let parent_dir = cargo_config.parent().unwrap();
    fs::create_dir_all(parent_dir)
        .with_context(|| format!("Failed to create directory {}", parent_dir.display()))?;
    let remap_toml = toml::to_string(&remap_config)?;
    fs::write(cargo_config, remap_toml)
        .with_context(|| format!("Failed to write {}", cargo_config.display()))?;
    Ok(())
}
