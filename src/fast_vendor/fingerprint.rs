/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;
use cargo_toml::OptionalFile;
use sha2::Digest as _;
use sha2::Sha256;
use walkdir::WalkDir;

use crate::config::Config;
use crate::fast_vendor::ExpectedCrate;
use crate::fast_vendor::SYNTHESIZED_BUILD_RS;
use crate::fast_vendor::bytes_sha256;
use crate::fast_vendor::cargo_checksum::checksum_json_bytes;
use crate::fast_vendor::checksum_excluded;
use crate::fast_vendor::file_sha256;
use crate::fast_vendor::filter::VendorFilters;
use crate::fast_vendor::limit_reader::LimitReader;
use crate::fast_vendor::materialization::Materialization;
use crate::fast_vendor::normalize_manifest_path;
use crate::fast_vendor::path_file_type_no_follow;
use crate::fast_vendor::source_excluded;

#[derive(Debug, Eq, PartialEq)]
pub(super) enum TreeEntryFingerprint {
    File(String),
    Symlink(String),
}

pub(super) fn vendor_dir_matches_expected_source(
    config: &Config,
    expected: &ExpectedCrate,
    filters: &VendorFilters,
) -> anyhow::Result<bool> {
    let Some(actual_type) = path_file_type_no_follow(&expected.dst)? else {
        return Ok(false);
    };
    if !actual_type.is_dir() {
        return Ok(false);
    }

    Ok(expected_tree_fingerprint(config, expected, filters)?
        == tree_fingerprint(config, &expected.dst, &expected.pkgdir, filters)?)
}

fn expected_tree_fingerprint(
    config: &Config,
    expected: &ExpectedCrate,
    filters: &VendorFilters,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    match &expected.materialization {
        Materialization::RegistryArchive { archive } => expected_registry_archive_fingerprint(
            config,
            expected,
            archive,
            filters,
            expected.pkg_cksum.as_deref(),
        ),
        Materialization::CopyFiles {
            src_root,
            file_paths,
            normalized_cargo_toml,
        } => expected_copy_source_fingerprint(
            config,
            src_root,
            file_paths,
            normalized_cargo_toml.as_deref(),
            &expected.pkgdir,
            filters,
            expected.pkg_cksum.as_deref(),
        ),
    }
}

fn expected_registry_archive_fingerprint(
    config: &Config,
    expected: &ExpectedCrate,
    archive: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let tarball =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let archive_size = tarball.metadata()?.len();
    let size_limit = u64::max(512 * 1024 * 1024, archive_size * 20);
    let gz = flate2::read::GzDecoder::new(LimitReader::new(tarball, size_limit));
    let mut tar = tar::Archive::new(gz);
    let prefix = Path::new(&expected.dst_name);

    let mut fingerprint = BTreeMap::new();
    let mut file_cksums = BTreeMap::new();
    let mut cargo_toml = None;

    for entry in tar.entries().context("failed to read archive entries")? {
        let mut entry = entry.context("failed to read archive entry")?;
        let entry_path = entry
            .path()
            .context("failed to read entry path")?
            .into_owned();
        let relative = entry_path.strip_prefix(prefix).with_context(|| {
            format!("invalid tarball: entry at {entry_path:?} is not under {prefix:?}")
        })?;

        if source_excluded(config, &expected.pkgdir, relative, filters)
            || relative == Path::new(".cargo-checksum.json")
        {
            continue;
        }

        if relative.as_os_str().is_empty() {
            continue;
        }

        let key = path_key(relative)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            continue;
        } else if entry_type.is_file() {
            let mut sha = Sha256::new();
            let mut buf = [0u8; 8 * 1024];
            let mut cargo_toml_contents = (key == "Cargo.toml").then(Vec::new);
            let hash = loop {
                let n = entry.read(&mut buf).with_context(|| {
                    format!("failed to read archive entry {}", entry_path.display())
                })?;
                if n == 0 {
                    break format!("{:x}", sha.finalize());
                }
                sha.update(&buf[..n]);
                if let Some(contents) = cargo_toml_contents.as_mut() {
                    contents.extend_from_slice(&buf[..n]);
                }
            };
            if let Some(contents) = cargo_toml_contents {
                if let Ok(content) = String::from_utf8(contents) {
                    cargo_toml = Some(content);
                }
            }
            fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
            maybe_insert_checksum(
                &mut file_cksums,
                &expected.pkgdir,
                relative,
                &key,
                &hash,
                filters,
            );
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name()
                .with_context(|| {
                    format!("failed to read symlink target for {}", entry_path.display())
                })?
                .map(|target| target.to_string_lossy().into_owned())
                .unwrap_or_default();
            fingerprint.insert(key, TreeEntryFingerprint::Symlink(target));
        } else {
            bail!("unsupported archive entry type at {}", entry_path.display());
        }
    }

    finish_expected_fingerprint(
        fingerprint,
        file_cksums,
        &expected.pkgdir,
        filters,
        pkg_cksum,
        cargo_toml.as_deref(),
    )
}

fn expected_copy_source_fingerprint(
    config: &Config,
    src_root: &Path,
    file_paths: &[PathBuf],
    normalized_cargo_toml: Option<&str>,
    pkgdir: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let mut fingerprint = BTreeMap::new();
    let mut file_cksums = BTreeMap::new();
    let mut cargo_toml = None;

    for src_path in file_paths {
        let relative = src_path.strip_prefix(src_root).with_context(|| {
            format!("{} is not under {}", src_path.display(), src_root.display(),)
        })?;

        if source_excluded(config, pkgdir, relative, filters)
            || relative == Path::new(".cargo-checksum.json")
        {
            continue;
        }

        let key = path_key(relative)?;
        let contents = if key == "Cargo.toml" {
            match normalized_cargo_toml {
                Some(contents) => contents.as_bytes().to_vec(),
                None => fs::read(src_path)
                    .with_context(|| format!("failed to read {}", src_path.display()))?,
            }
        } else {
            fs::read(src_path).with_context(|| format!("failed to read {}", src_path.display()))?
        };
        let hash = bytes_sha256(&contents);
        if key == "Cargo.toml" {
            if let Ok(content) = String::from_utf8(contents.clone()) {
                cargo_toml = Some(content);
            }
        }
        fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
        maybe_insert_checksum(&mut file_cksums, pkgdir, relative, &key, &hash, filters);
    }

    finish_expected_fingerprint(
        fingerprint,
        file_cksums,
        pkgdir,
        filters,
        pkg_cksum,
        cargo_toml.as_deref(),
    )
}

fn finish_expected_fingerprint(
    mut fingerprint: BTreeMap<String, TreeEntryFingerprint>,
    mut file_cksums: BTreeMap<String, String>,
    pkgdir: &Path,
    filters: &VendorFilters,
    pkg_cksum: Option<&str>,
    cargo_toml: Option<&str>,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    synthesize_missing_build_rs_fingerprint(
        &mut fingerprint,
        &mut file_cksums,
        pkgdir,
        filters,
        cargo_toml,
    )?;
    let checksum_json = checksum_json_bytes(pkg_cksum, &file_cksums)?;
    fingerprint.insert(
        ".cargo-checksum.json".to_owned(),
        TreeEntryFingerprint::File(bytes_sha256(&checksum_json)),
    );
    Ok(fingerprint)
}

fn synthesize_missing_build_rs_fingerprint(
    fingerprint: &mut BTreeMap<String, TreeEntryFingerprint>,
    file_cksums: &mut BTreeMap<String, String>,
    pkgdir: &Path,
    filters: &VendorFilters,
    cargo_toml: Option<&str>,
) -> anyhow::Result<()> {
    type TomlManifest = cargo_toml::Manifest<serde::de::IgnoredAny>;

    let Some(cargo_toml) = cargo_toml else {
        return Ok(());
    };
    let Ok(manifest) = toml::from_str::<TomlManifest>(cargo_toml) else {
        return Ok(());
    };
    let Some(package) = &manifest.package else {
        return Ok(());
    };
    let Some(OptionalFile::Path(build_script_path)) = &package.build else {
        return Ok(());
    };
    let build_script_path = normalize_manifest_path(build_script_path);
    let key = path_key(&build_script_path)?;
    if fingerprint.contains_key(&key) {
        return Ok(());
    }

    let hash = bytes_sha256(SYNTHESIZED_BUILD_RS);
    fingerprint.insert(key.clone(), TreeEntryFingerprint::File(hash.clone()));
    maybe_insert_checksum(
        file_cksums,
        pkgdir,
        &build_script_path,
        &key,
        &hash,
        filters,
    );
    Ok(())
}

fn maybe_insert_checksum(
    file_cksums: &mut BTreeMap<String, String>,
    pkgdir: &Path,
    relative: &Path,
    key: &str,
    hash: &str,
    filters: &VendorFilters,
) {
    if checksum_excluded(pkgdir, relative, key, filters.checksum_filter.as_ref()) {
        return;
    }
    file_cksums.insert(key.to_owned(), hash.to_owned());
}

fn path_key(path: &Path) -> anyhow::Result<String> {
    let Some(path) = path.to_str() else {
        bail!("non-UTF8 vendor path {}", path.display());
    };
    Ok(path.replace('\\', "/"))
}

fn tree_fingerprint(
    config: &Config,
    root: &Path,
    pkgdir: &Path,
    filters: &VendorFilters,
) -> anyhow::Result<BTreeMap<String, TreeEntryFingerprint>> {
    let mut entries = BTreeMap::new();
    for entry in WalkDir::new(root).into_iter() {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("walkdir entry must be under root");
        if source_excluded(config, pkgdir, relative, filters) {
            continue;
        }
        let relative = relative.to_string_lossy().replace('\\', "/");
        let file_type = entry.file_type();
        if file_type.is_dir() {
            continue;
        }
        let fingerprint = if file_type.is_file() {
            TreeEntryFingerprint::File(file_sha256(path)?)
        } else if file_type.is_symlink() {
            let target = fs::read_link(path)
                .with_context(|| format!("failed to read symlink {}", path.display()))?;
            TreeEntryFingerprint::Symlink(target.to_string_lossy().into_owned())
        } else {
            bail!("unsupported vendor tree entry type at {}", path.display());
        };
        entries.insert(relative, fingerprint);
    }
    Ok(entries)
}

#[cfg(test)]
mod test {
    use std::fs;
    use std::path::Path;

    use crate::config::Config;
    use crate::fast_vendor::filter::VendorFilters;
    use crate::fast_vendor::fingerprint::tree_fingerprint;

    #[test]
    fn test_tree_fingerprint_ignores_empty_directories() {
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir(root.join("empty")).unwrap();

        let filters = VendorFilters {
            checksum_filter: None,
        };
        let fingerprint =
            tree_fingerprint(&config, root, Path::new("vendor/example-0.1.0"), &filters).unwrap();

        assert!(
            !fingerprint.contains_key("empty"),
            "empty directories are not tracked by source control and should not force a refresh"
        );
    }
}
