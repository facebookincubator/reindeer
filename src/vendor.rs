/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use globset::Glob;
use globset::GlobBuilder;
use globset::GlobSetBuilder;
use ignore::gitignore::GitignoreBuilder;

use crate::Args;
use crate::Paths;
use crate::cargo;
use crate::cargo::ChecksumFilter;
use crate::cargo::VendorFilters;
use crate::cargo::fast_vendor;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::config::VendorSourceConfig;
use crate::remap::RemapConfig;
use crate::remap::RemapSource;

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

    match &config.vendor {
        VendorConfig::Off => {
            unreachable!(
                "VendorConfig::Off is only ever set during `reindeer buckify`, not `reindeer vendor`"
            );
        }
        VendorConfig::LocalRegistry => {
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
        }
        VendorConfig::Source(source_config) => {
            log::info!("Running fast vendor (library mode)");
            let filters = build_filters(config, paths, source_config)?;
            fast_vendor(config, no_delete, args, paths, filters)?;
            assert!(is_vendored(config, paths)?);
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
            VendorConfig::Source(_) | VendorConfig::Off => Ok(source.directory.is_some()),
        },
        None => Ok(false),
    }
}

fn build_filters(
    config: &Config,
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<VendorFilters> {
    let buck_file_name = if config.buck.split {
        Some((*config.buck.file_name).clone())
    } else {
        None
    };

    let checksum_filter = build_checksum_filter(
        config.buck.split,
        &config.buck.file_name,
        &source_config.checksum_exclude,
        &source_config.gitignore_checksum_exclude,
        &paths.third_party_dir,
    )?;

    Ok(VendorFilters {
        buck_file_name,
        checksum_filter,
    })
}

/// Build the checksum filter from primitive inputs.
///
/// Returns `None` when no filtering is needed. Extracted from `build_filters`
/// so the logic can be tested without constructing `Config` or `Paths`.
fn build_checksum_filter(
    buck_split: bool,
    buck_file_name: &str,
    checksum_exclude: &[String],
    gitignore_checksum_exclude: &[PathBuf],
    third_party_dir: &Path,
) -> anyhow::Result<Option<ChecksumFilter>> {
    // Build a checksum filter when there are explicit excludes configured, or
    // when split mode is enabled (BUCK files must be excluded from checksums to
    // match the on-disk exclusion that split mode also applies).
    let needs_filter =
        !checksum_exclude.is_empty() || !gitignore_checksum_exclude.is_empty() || buck_split;
    if !needs_filter {
        return Ok(None);
    }

    log::debug!(
        "vendor.gitignore_checksum_exclude = {:?} vendor.checksum_exclude = {:?}",
        gitignore_checksum_exclude,
        checksum_exclude
    );

    let mut remove_globs = GlobSetBuilder::new();
    for glob in checksum_exclude {
        let glob = GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
            .with_context(|| format!("Invalid checksum exclude glob `{}`", glob))?;
        remove_globs.add(glob);
    }
    // Exclude the BUCK file from checksums only when split mode is enabled.
    // This keeps checksum exclusion aligned with on-disk exclusion: both are
    // gated on the same split-mode condition.
    if buck_split {
        let buck_glob = Glob::new(buck_file_name)
            .with_context(|| format!("Invalid buck.file_name glob `{}`", buck_file_name))?;
        remove_globs.add(buck_glob);
    }
    let remove_globs = remove_globs.build()?;

    let mut gitignore = GitignoreBuilder::new(third_party_dir);
    for ignore in gitignore_checksum_exclude {
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

    Ok(Some(ChecksumFilter {
        remove_globs,
        gitignore,
    }))
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

    #[test]
    fn test_build_checksum_filter_split_true_no_other_excludes() {
        // When split=true and no other excludes, the filter must still be built
        // and BUCK must be in the remove set (to align with on-disk exclusion).
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(true, "BUCK", &[], &[], dir.path())
            .expect("build_checksum_filter should succeed");

        let filter = filter.expect("filter should be Some when split=true");
        assert!(
            filter.remove_globs.is_match("BUCK"),
            "BUCK must be in checksum exclusion when split=true"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_false_no_excludes() {
        // When split=false and no other excludes, no filter is needed.
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(false, "BUCK", &[], &[], dir.path())
            .expect("build_checksum_filter should succeed");

        assert!(
            filter.is_none(),
            "filter should be None when split=false and no excludes"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_false_with_checksum_exclude() {
        // When split=false but checksum_exclude is non-empty, a filter is built
        // but BUCK must NOT be in it (split mode is what gates BUCK exclusion).
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(false, "BUCK", &["*.json".to_owned()], &[], dir.path())
            .expect("build_checksum_filter should succeed");

        let filter = filter.expect("filter should be Some when checksum_exclude is non-empty");
        assert!(
            !filter.remove_globs.is_match("BUCK"),
            "BUCK must NOT be in checksum exclusion when split=false"
        );
        assert!(
            filter.remove_globs.is_match("something.json"),
            "configured checksum_exclude glob should still match"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_true_with_checksum_exclude() {
        // When split=true AND checksum_exclude is set, both BUCK and the
        // configured globs must be in the filter.
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(true, "BUCK", &["*.json".to_owned()], &[], dir.path())
            .expect("build_checksum_filter should succeed");

        let filter = filter.expect("filter should be Some");
        assert!(
            filter.remove_globs.is_match("BUCK"),
            "BUCK must be in checksum exclusion when split=true"
        );
        assert!(
            filter.remove_globs.is_match("something.json"),
            "configured checksum_exclude glob should also match"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_true_invalid_buck_glob_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = build_checksum_filter(true, "[", &[], &[], dir.path());

        assert!(result.is_err(), "invalid buck.file_name glob should error");
        let err = result.err().expect("error should be present");

        assert!(
            format!("{err:#}").contains("Invalid buck.file_name glob `[`"),
            "error should point at invalid buck.file_name glob: {err:#}"
        );
    }
}
