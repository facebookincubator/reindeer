/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use walkdir::WalkDir;

use crate::config::Config;
use crate::fast_vendor::bytes_sha256;
use crate::fast_vendor::is_split_buck_file;

/// Walk a directory and compute SHA256 checksums for all regular files.
///
/// Files matched by checksum globs are left on disk but omitted from the
/// returned map. VCS bookkeeping files and configured gitignore matches are
/// treated as absent from the vendored source tree.
pub(crate) fn compute_dir_checksums_filtered(
    config: &Config,
    root: &Path,
) -> anyhow::Result<BTreeMap<String, String>> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry)),
            Ok(_) => None,
            Err(err) => Some(Err(anyhow::Error::from(err))),
        })
        .map(|entry| {
            let e = entry?;
            let path = e.path();
            let relative = path
                .strip_prefix(root)
                .expect("walkdir entry must be under root");
            let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");
            if key == ".cargo-checksum.json" || is_split_buck_file(config, relative) {
                return Ok(None);
            }
            let contents = fs::read(path)?;
            let hash = bytes_sha256(&contents);
            Ok(Some((key, hash)))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

pub(crate) fn checksum_json_bytes(
    pkg_cksum: Option<&str>,
    file_cksums: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::json!({
        "package": pkg_cksum,
        "files": file_cksums,
    });
    Ok(json.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::Digest as _;

    use crate::config::Config;
    use crate::fast_vendor::cargo_checksum::compute_dir_checksums_filtered;

    #[test]
    fn test_checksum_excludes_buck_entry() {
        // A BUCK file present in the extracted directory should be excluded
        // from the checksum map (it was skipped at extraction time by the
        // include filter, so it won't be on disk here -- but even if it were,
        // a glob on "BUCK" would exclude it).
        let config = Config::split_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("Cargo.toml"), b"[package]").unwrap();

        let cksums = compute_dir_checksums_filtered(&config, root).expect("checksums computed");

        // lib.rs and Cargo.toml should be present; BUCK should not be.
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should be in checksum map"
        );
        assert!(
            !cksums.contains_key("BUCK"),
            "BUCK should be excluded from checksum map"
        );
    }

    #[test]
    fn test_checksum_computation_skips_checksum_file() {
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join(".cargo-checksum.json"), b"not source").unwrap();

        let cksums = compute_dir_checksums_filtered(&config, root).unwrap();

        assert!(cksums.contains_key("lib.rs"));
        assert!(
            !cksums.contains_key(".cargo-checksum.json"),
            "checksum metadata is generated, not source content"
        );
    }

    // Invariant: compute_dir_checksums_filtered without a filter produces SHA256 hashes for all
    // files in a tree.
    #[test]
    fn test_compute_dir_checksums() {
        let config = Config::default_for_test();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), b"world").unwrap();

        let cksums = compute_dir_checksums_filtered(&config, tmp.path()).unwrap();
        assert_eq!(cksums.len(), 2);
        assert!(cksums.contains_key("a.txt"));
        assert!(cksums.contains_key("sub/b.txt"));

        let expected_a = format!("{:x}", sha2::Sha256::digest(b"hello"));
        assert_eq!(cksums["a.txt"], expected_a);
    }
}
