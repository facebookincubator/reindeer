/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap as Map;
use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Context as _;
use cargo::core::GitReference;
use cargo::core::SourceId;
use cargo::sources::CRATES_IO_REGISTRY;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RemapConfig {
    #[serde(rename = "source", default)]
    pub sources: Map<String, RemapSource>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RemapSource {
    pub registry: Option<String>,
    pub directory: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    #[serde(rename = "replace-with")]
    pub replace_with: Option<String>,
    #[serde(rename = "local-registry")]
    pub local_registry: Option<PathBuf>,
}

/// Generate a `.cargo/config.toml` string with source replacement entries
/// that point all resolved sources to the `vendored-sources` directory.
pub(crate) fn generate_vendor_config(sources: &BTreeSet<SourceId>) -> anyhow::Result<String> {
    let mut remap = RemapConfig::default();
    let merged = "vendored-sources";

    for sid in sources {
        let name = if sid.is_crates_io() {
            CRATES_IO_REGISTRY.to_string()
        } else {
            sid.without_precise().as_url().to_string()
        };

        let source = if sid.is_crates_io() {
            RemapSource {
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_remote_registry() {
            RemapSource {
                registry: Some(sid.url().to_string()),
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_git() {
            let mut branch = None;
            let mut tag = None;
            let mut rev = None;
            if let Some(reference) = sid.git_reference() {
                match reference {
                    GitReference::Branch(b) => branch = Some(b.clone()),
                    GitReference::Tag(t) => tag = Some(t.clone()),
                    GitReference::Rev(r) => rev = Some(r.clone()),
                    GitReference::DefaultBranch => {}
                }
            }
            RemapSource {
                git: Some(sid.url().to_string()),
                branch,
                tag,
                rev,
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else {
            anyhow::bail!("unsupported source type: {}", sid);
        };

        remap.sources.insert(name, source);
    }

    // Always write [source.vendored-sources] so that is_vendored() returns true
    // even for workspaces with only path dependencies (where sources is empty).
    remap.sources.insert(
        merged.to_owned(),
        RemapSource {
            directory: Some(PathBuf::from("vendor")),
            ..RemapSource::default()
        },
    );

    toml::to_string(&remap).context("failed to serialize vendor config")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::remap::generate_vendor_config;

    #[test]
    fn test_generate_vendor_config_uses_relative_vendor_dir() {
        let mut sources = BTreeSet::new();
        sources.insert(
            cargo::core::SourceId::from_url(
                "registry+https://github.com/rust-lang/crates.io-index",
            )
            .expect("valid registry URL"),
        );

        let config = generate_vendor_config(&sources).unwrap();

        assert!(
            config.contains("directory = \"vendor\""),
            "config should write a relative vendor directory: {config}"
        );
        assert!(
            !config.contains("/tmp/absolute/path/vendor"),
            "config should not embed an absolute vendor path: {config}"
        );
    }

    #[test]
    fn test_generate_vendor_config_path_only_workspace() {
        // A workspace with only path dependencies has an empty sources set.
        // generate_vendor_config must still emit [source.vendored-sources] so
        // that is_vendored() returns true after fast_vendor() runs.
        let sources = BTreeSet::new();
        let config = generate_vendor_config(&sources).unwrap();
        assert!(
            config.contains("vendored-sources"),
            "config must contain vendored-sources even for path-only workspaces: {config}",
        );
    }

    // Invariant: generate_vendor_config emits git source replacement with branch/tag/rev fields
    #[test]
    fn test_generate_vendor_config_git_source() {
        let mut sources = BTreeSet::new();
        let sid =
            cargo::core::SourceId::from_url("git+https://github.com/example/crate.git?branch=main")
                .expect("valid git URL");
        sources.insert(sid);

        let config = generate_vendor_config(&sources).unwrap();
        assert!(
            config.contains("git = \"https://github.com/example/crate.git\""),
            "config must contain git URL: {config}",
        );
        assert!(
            config.contains("branch = \"main\""),
            "config must contain branch: {config}",
        );
        assert!(
            config.contains("replace-with = \"vendored-sources\""),
            "config must redirect to vendored-sources: {config}",
        );
    }
}
