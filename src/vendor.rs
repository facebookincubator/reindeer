/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::Context;
use globset::GlobBuilder;
use globset::GlobSetBuilder;
use ignore::gitignore::GitignoreBuilder;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;

use crate::buckify::relative_path;
use crate::cargo;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::config::VendorSourceConfig;
use crate::remap::RemapConfig;
use crate::Args;
use crate::Paths;

#[derive(Debug, Deserialize, Serialize)]
struct CargoChecksums {
    files: IndexMap<String, String>,
    package: Option<String>,
}

pub(crate) fn cargo_vendor(
    config: &Config,
    no_delete: bool,
    audit_sec: bool,
    no_fetch: bool,
    args: &Args,
    paths: &Paths,
) -> anyhow::Result<()> {
    let vendordir = Path::new("vendor"); // relative to third_party_dir

    let mut cmdline = vec![
        "vendor",
        "--manifest-path",
        paths.manifest_path.to_str().unwrap(),
        vendordir.to_str().unwrap(),
        "--versioned-dirs",
    ];
    if no_delete {
        cmdline.push("--no-delete");
    }

    fs::create_dir_all(&paths.cargo_home)?;

    log::info!("Running cargo {:?}", cmdline);
    let cargoconfig = cargo::run_cargo(
        config,
        Some(&paths.cargo_home),
        &paths.third_party_dir,
        args,
        &cmdline,
    )?;

    fs::write(paths.cargo_home.join("config.toml"), &cargoconfig)?;
    if !cargoconfig.is_empty() {
        assert!(is_vendored(paths)?);
    }

    if let VendorConfig::Source(source_config) = &config.vendor {
        filter_checksum_files(&paths.third_party_dir, vendordir, source_config)?;
    }

    if audit_sec {
        crate::audit_sec::audit_sec(paths, no_fetch).context("doing audit_sec")?;
    }

    Ok(())
}

pub(crate) fn is_vendored(paths: &Paths) -> anyhow::Result<bool> {
    // .cargo/config.toml is Cargo's preferred name for the config, but .cargo/config
    // is the older name so it takes priority if present.
    let mut cargo_config_path = paths.cargo_home.join("config");
    let result = match fs::read_to_string(&cargo_config_path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            cargo_config_path = paths.cargo_home.join("config.toml");
            match fs::read_to_string(&cargo_config_path) {
                Ok(content) => Ok(content),
                Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    };

    let content = result.with_context(|| {
        format!(
            "Failed to read cargo config {}",
            cargo_config_path.display(),
        )
    })?;

    let remap_config: RemapConfig = toml::from_str(&content)
        .context(format!("Failed to parse {}", cargo_config_path.display()))?;

    match remap_config.sources.get("vendored-sources") {
        Some(vendored_sources) => Ok(vendored_sources.directory.is_some()),
        None => Ok(false),
    }
}

fn filter_checksum_files(
    third_party_dir: &Path,
    vendordir: &Path,
    config: &VendorSourceConfig,
) -> anyhow::Result<()> {
    if config.checksum_exclude.is_empty() && config.gitignore_checksum_exclude.is_empty() {
        return Ok(());
    }

    log::debug!(
        "vendor.gitignore_checksum_exclude = {:?} vendor.checksum_exclude = {:?}",
        config.gitignore_checksum_exclude,
        config.checksum_exclude
    );

    // re-write checksum files to exclude things we don't want (like Cargo.lock)
    let mut remove_globs = GlobSetBuilder::new();
    for glob in &config.checksum_exclude {
        let glob = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid checksum exclude glob `{}`", glob))?;
        remove_globs.add(glob);
    }
    let remove_globs = remove_globs.build()?;

    let mut gitignore = GitignoreBuilder::new(third_party_dir);
    for ignore in &config.gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(third_party_dir.join(ignore)) {
            log::warn!(
                "Failed to read ignore file {}: {}; skipping",
                ignore.display(),
                err
            );
        }
    }
    let gitignore = gitignore.build()?;

    log::debug!(
        "remove_globs {:#?}, gitignore {:#?}",
        remove_globs,
        gitignore
    );

    for entry in fs::read_dir(third_party_dir.join(vendordir))? {
        let entry = entry?;
        let path = entry.path(); // full/path/to/vendor/foo-1.2.3
        let checksum = path.join(".cargo-checksum.json"); // full/path/to/vendor/foo-1.2.3/.cargo-checksum.json

        log::trace!("Reading checksum {}", checksum.display());

        let file = match fs::read(&checksum) {
            Err(err) => {
                log::warn!("Failed to read {}: {}", checksum.display(), err);
                continue;
            }
            Ok(file) => file,
        };

        let mut checksums: CargoChecksums = match serde_json::from_slice(&file) {
            Err(err) => {
                log::warn!("Failed to deserialize {}: {}", checksum.display(), err);
                continue;
            }
            Ok(cs) => cs,
        };

        let mut changed = false;

        let pkgdir = relative_path(third_party_dir, &path); // vendor/foo-1.2.3

        checksums.files.retain(|k, _| {
            log::trace!("{}: checking {}", checksum.display(), k);
            let del = remove_globs.is_match(k)
                || gitignore
                    .matched_path_or_any_parents(pkgdir.join(k), false)
                    .is_ignore();
            if del {
                log::debug!("{}: removing {}", checksum.display(), k);
                changed = true;
            };
            !del
        });

        if changed {
            log::info!("Rewriting checksum {}", checksum.display());
            fs::write(checksum, serde_json::to_vec(&checksums)?)?;
        }
    }

    Ok(())
}
