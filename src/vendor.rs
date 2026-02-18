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
use anyhow::bail;
use cargo_toml::OptionalFile;
use globset::Glob;
use globset::GlobBuilder;
use globset::GlobSetBuilder;
use ignore::gitignore::GitignoreBuilder;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;

use crate::Args;
use crate::Paths;
use crate::cargo;
use crate::config::BuckConfig;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::config::VendorSourceConfig;
use crate::path::relative_path;
use crate::remap::RemapConfig;
use crate::remap::RemapSource;

#[derive(Debug, Deserialize, Serialize)]
struct CargoChecksums {
    files: IndexMap<String, String>,
    package: Option<String>,
}

pub(crate) fn cargo_vendor(
    config: &Config,
    no_delete: bool,
    #[cfg(fbcode_build)] audit_sec: bool,
    #[cfg(fbcode_build)] no_fetch: bool,
    args: &Args,
    paths: &Paths,
) -> anyhow::Result<()> {
    let vendordir = Path::new("vendor"); // relative to third_party_dir
    let full_vendor_dir = paths.third_party_dir.join("vendor");

    if let VendorConfig::LocalRegistry = config.vendor {
        let mut cmdline = vec![
            "local-registry",
            "-s",
            paths.lockfile_path.to_str().unwrap(),
            full_vendor_dir.to_str().unwrap(),
            "--git",
        ];
        if no_delete {
            cmdline.push("--no-delete");
        }
        log::info!("Running cargo {:?}", cmdline);
        let _ = cargo::run_cargo(config, Some(&paths.cargo_home), None, args, &cmdline)?;
        let mut remap = RemapConfig::default();
        remap.sources.insert(
            "crates-io".to_owned(),
            RemapSource {
                registry: Some("sparse+https://index.crates.io/".to_owned()),
                replace_with: Some("local-registry".to_owned()),
                ..RemapSource::default()
            },
        );
        remap.sources.insert(
            "local-registry".to_owned(),
            RemapSource {
                local_registry: Some(vendordir.to_owned()),
                ..RemapSource::default()
            },
        );
        let config_toml = toml::to_string(&remap).context("failed to serialize config.toml")?;
        fs::write(paths.cargo_home.join("config.toml"), config_toml)?;
        assert!(is_vendored(config, paths)?);
    } else {
        let mut cmdline = vec![
            "vendor",
            "--manifest-path",
            paths.manifest_path.to_str().unwrap(),
            full_vendor_dir.to_str().unwrap(),
            "--versioned-dirs",
        ];
        if no_delete {
            cmdline.push("--no-delete");
        }

        fs::create_dir_all(&paths.cargo_home)?;

        log::info!("Running cargo {:?}", cmdline);
        let mut cargoconfig =
            cargo::run_cargo(config, Some(&paths.cargo_home), None, args, &cmdline)?;

        // .cargo/config.toml will contain a section like this, in which we do
        // not want an absolute path:
        //
        //     [source.vendored-sources]
        //     directory = "vendor"
        cargoconfig = cargoconfig.replace(
            &*full_vendor_dir.to_string_lossy(),
            &vendordir.to_string_lossy(),
        );

        fs::write(paths.cargo_home.join("config.toml"), &cargoconfig)?;
        if !cargoconfig.is_empty() {
            assert!(is_vendored(config, paths)?);
        }

        if let VendorConfig::Source(source_config) = &config.vendor {
            delete_buck_files(&paths.third_party_dir, &config.buck)?;
            filter_checksum_files(
                &paths.third_party_dir,
                vendordir,
                source_config,
                &config.buck,
            )?;
            write_excluded_build_scripts(&paths.third_party_dir, vendordir)?;
        }
    }

    #[cfg(fbcode_build)]
    {
        if audit_sec {
            crate::audit_sec::audit_sec(paths, no_fetch).context("doing audit_sec")?;
        }
    }

    Ok(())
}

pub(crate) fn is_vendored(config: &Config, paths: &Paths) -> anyhow::Result<bool> {
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

    let source_name = match config.vendor {
        VendorConfig::LocalRegistry => "local-registry",
        VendorConfig::Source(_) => "vendored-sources",
        _ => return Ok(false),
    };
    match remap_config.sources.get(source_name) {
        Some(source) => match config.vendor {
            VendorConfig::LocalRegistry => Ok(source.local_registry.is_some()),
            VendorConfig::Source(_) => Ok(source.directory.is_some()),
            VendorConfig::Off => Ok(false),
        },
        None => Ok(false),
    }
}

fn delete_buck_files(third_party_dir: &Path, buck_config: &BuckConfig) -> anyhow::Result<()> {
    if !buck_config.split {
        // Assume you will use .gitignore to ignore vendored BUCK files.
        return Ok(());
    }

    // Remove top-level BUCK files (vendor/*/BUCK). Almost all of these would be
    // overwritten anyway by buckify, except in the case that a crate containing
    // BUCK is vendored by `cargo vendor` and then not buckified, either because
    // it is a platform-specific dependency for some platform that Reindeer is
    // not configured for, or because of an omit_deps fixup.
    //
    // Assume you will use .gitignore to ignore nested ones (vendor/*/*/**/BUCK).
    for entry in fs::read_dir(third_party_dir.join("vendor"))? {
        let entry = entry?;
        let vendored_buck_file = entry.path().join(&buck_config.file_name);
        if let Err(err) = fs::remove_file(&vendored_buck_file)
            && err.kind() != ErrorKind::NotFound
        {
            bail!("failed to remove {}: {}", vendored_buck_file.display(), err);
        }
    }

    Ok(())
}

fn filter_checksum_files(
    third_party_dir: &Path,
    vendordir: &Path,
    config: &VendorSourceConfig,
    buck_config: &BuckConfig,
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
    if let Ok(buck_glob) = Glob::new(&buck_config.file_name) {
        remove_globs.add(buck_glob);
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
        let manifest_dir = entry.path(); // full/path/to/vendor/foo-1.2.3
        let checksum = manifest_dir.join(".cargo-checksum.json"); // full/path/to/vendor/foo-1.2.3/.cargo-checksum.json

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

        let pkgdir = relative_path(third_party_dir, &manifest_dir); // vendor/foo-1.2.3

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

// Work around https://github.com/rust-lang/cargo/issues/14348.
//
// This step can be deleted if that `cargo vendor` bug is fixed in a future
// version of Cargo.
fn write_excluded_build_scripts(third_party_dir: &Path, vendordir: &Path) -> anyhow::Result<()> {
    type TomlManifest = cargo_toml::Manifest<serde::de::IgnoredAny>;

    let third_party_vendor = third_party_dir.join(vendordir);
    if !third_party_vendor.try_exists()? {
        // If there are no dependencies from a remote registry (because there
        // are no dependencies whatsoever, or all dependencies are local path
        // dependencies) then `cargo vendor` won't have created a "vendor"
        // directory.
        return Ok(());
    }

    for entry in fs::read_dir(third_party_vendor)? {
        let entry = entry?;
        let manifest_dir = entry.path(); // full/path/to/vendor/foo-1.2.3
        let cargo_toml = manifest_dir.join("Cargo.toml");

        log::trace!("Reading manifest {}", cargo_toml.display());

        let content = match fs::read_to_string(&cargo_toml) {
            Ok(file) => file,
            Err(err) => {
                log::warn!("Failed to read {}: {}", cargo_toml.display(), err);
                continue;
            }
        };

        let manifest: TomlManifest = match toml::from_str(&content) {
            Ok(cs) => cs,
            Err(err) => {
                log::warn!("Failed to deserialize {}: {}", cargo_toml.display(), err);
                continue;
            }
        };

        let Some(package) = &manifest.package else {
            continue;
        };
        let Some(OptionalFile::Path(build_script_path)) = &package.build else {
            continue;
        };

        let expected_build_script = manifest_dir.join(build_script_path);
        if !expected_build_script.try_exists()? {
            log::trace!(
                "Synthesizing build script {}",
                expected_build_script.display(),
            );
            fs::write(expected_build_script, "fn main() {}\n")?;
        }
    }

    Ok(())
}

/// Extract crate name from a versioned directory
fn extract_crate_name(dir_name: &str) -> &str {
    let dot_pos = dir_name.find('.').unwrap_or(dir_name.len());
    match dir_name[..dot_pos].rfind('-') {
        Some(i) => &dir_name[..i],
        None => dir_name,
    }
}

/// Delete vendored directories for crates not in the `vendored` list (extern_crates).
/// Crates not in the vendored set are expected to be resolved by external Buck
/// targets, so their vendored sources are unnecessary.
pub(crate) fn cleanup_extern_crates(config: &Config, paths: &Paths) -> anyhow::Result<()> {
    let Some(extern_config) = &config.extern_crates else {
        log::info!("No extern_crates configured, nothing to clean up");
        return Ok(());
    };

    let vendor_path = paths.third_party_dir.join("vendor");
    if !vendor_path.try_exists()? {
        log::info!("No vendor directory found at {}", vendor_path.display());
        return Ok(());
    }

    let mut removed = 0;
    for entry in fs::read_dir(&vendor_path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();
        let crate_name = extract_crate_name(&dir_name);

        if !extern_config.vendored.contains(crate_name) {
            let path = entry.path();
            log::info!("Removing non-vendored crate dir: {}", path.display());
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed += 1;
        }
    }

    log::info!("Removed {} non-vendored crate directories", removed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_crate_name() {
        assert_eq!(extract_crate_name("serde-1.0.228"), "serde");
        assert_eq!(extract_crate_name("aes-gcm-siv-0.11.1"), "aes-gcm-siv");
        assert_eq!(extract_crate_name("asn1-rs-derive-0.6.0"), "asn1-rs-derive");
        assert_eq!(
            extract_crate_name("curve25519-dalek-derive-0.1.0"),
            "curve25519-dalek-derive"
        );
        assert_eq!(extract_crate_name("ed25519-dalek-2.2.0"), "ed25519-dalek");
        assert_eq!(extract_crate_name("base64-url-2.0.2"), "base64-url");
        assert_eq!(extract_crate_name("lz4-sys-1.11.1+lz4-1.10.0"), "lz4-sys");
        assert_eq!(extract_crate_name("x86_64-0.15.2"), "x86_64");
        assert_eq!(extract_crate_name("aarch64-0.1.0"), "aarch64");
        assert_eq!(extract_crate_name("base64-0.22.1"), "base64");
        assert_eq!(extract_crate_name("sha2-0.10.9"), "sha2");
        assert_eq!(extract_crate_name("sha3-0.10.8"), "sha3");
        assert_eq!(extract_crate_name("p256-0.13.2"), "p256");
        assert_eq!(extract_crate_name("p384-0.13.1"), "p384");
        assert_eq!(extract_crate_name("h2-0.4.13"), "h2");
        assert_eq!(extract_crate_name("md-5-0.10.6"), "md-5");
        assert_eq!(extract_crate_name("blake3-1.8.2"), "blake3");
        assert_eq!(extract_crate_name("adler32-1.2.0"), "adler32");
        assert_eq!(extract_crate_name("anymap3-1.0.1"), "anymap3");
        assert_eq!(extract_crate_name("akd-0.12.0-pre.11"), "akd");
        assert_eq!(extract_crate_name("sha2-0.11.0-pre.4"), "sha2");
    }
}
