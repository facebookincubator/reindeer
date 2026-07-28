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

use crate::fast_vendor::ChecksumFilter;
use crate::fast_vendor::bytes_sha256;
use crate::fast_vendor::checksum_excluded;
use crate::fast_vendor::gitignore_excluded;
use crate::fast_vendor::vendor_this;

/// Walk a directory and compute SHA256 checksums for all regular files.
///
/// Files matched by checksum globs are left on disk but omitted from the
/// returned map. VCS bookkeeping files and configured gitignore matches are
/// treated as absent from the vendored source tree.
pub(crate) fn compute_dir_checksums_filtered(
    root: &Path,
    pkgdir: &Path,
    filter: Option<&ChecksumFilter>,
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
            if key == ".cargo-checksum.json" {
                return Ok(None);
            }
            if !vendor_this(relative) || gitignore_excluded(pkgdir, relative, filter) {
                log::trace!("checksum: skipping source-excluded file {}", key);
                return Ok(None);
            }
            if checksum_excluded(pkgdir, relative, &key, filter) {
                log::trace!("checksum: skipping excluded file {}", key);
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
    use std::path::Path;

    use globset::GlobBuilder;
    use globset::GlobSetBuilder;
    use ignore::gitignore::GitignoreBuilder;
    use sha2::Digest as _;

    use crate::fast_vendor::ChecksumFilter;
    use crate::fast_vendor::cargo_checksum::compute_dir_checksums_filtered;
    use crate::fast_vendor::tests::gitignore_filter;

    // Build a ChecksumFilter that matches a single glob pattern.
    fn glob_filter(pattern: &str) -> ChecksumFilter {
        let mut builder = GlobSetBuilder::new();
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .unwrap(),
        );
        let gitignore = GitignoreBuilder::new("/").build().unwrap();
        ChecksumFilter {
            remove_globs: builder.build().unwrap(),
            gitignore,
        }
    }

    #[test]
    fn test_checksum_excludes_buck_entry() {
        // A BUCK file present in the extracted directory should be excluded
        // from the checksum map (it was skipped at extraction time by the
        // include filter, so it won't be on disk here -- but even if it were,
        // a glob on "BUCK" would exclude it).
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("Cargo.toml"), b"[package]").unwrap();

        // Use a glob filter that matches the BUCK file name.
        let filter = glob_filter("BUCK");
        let pkgdir = std::path::Path::new("vendor/sourdough-starter-1.0.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

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
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join(".cargo-checksum.json"), b"not source").unwrap();

        let cksums =
            compute_dir_checksums_filtered(root, Path::new("vendor/example-0.1.0"), None).unwrap();

        assert!(cksums.contains_key("lib.rs"));
        assert!(
            !cksums.contains_key(".cargo-checksum.json"),
            "checksum metadata is generated, not source content"
        );
    }

    #[test]
    fn test_checksum_filter_glob_keeps_file_on_disk() {
        // Files matched by checksum_exclude globs should remain on disk
        // but be absent from the checksum map.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("pancake-stack.h"), b"// header").unwrap();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();

        // Filter that excludes all .h files.
        let filter = glob_filter("*.h");
        let pkgdir = std::path::Path::new("vendor/flux-capacitor-1.21.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("pancake-stack.h"),
            ".h files should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );

        // File must still exist on disk (checksum_exclude only affects the map).
        assert!(
            root.join("pancake-stack.h").exists(),
            ".h file should remain on disk"
        );
    }

    #[test]
    fn test_checksum_filter_gitignore_excludes_source_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml.orig"),
            b"[package]\nname = \"orig\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            b"[package]\nname = \"normalized\"\n",
        )
        .unwrap();

        let filter = gitignore_filter("vendor/*/Cargo.toml.orig");
        let pkgdir = std::path::Path::new("vendor/fb-procfs-0.9.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("Cargo.toml.orig"),
            "gitignore-matched Cargo.toml.orig should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should remain in checksum map"
        );
    }

    // Invariant: compute_dir_checksums_filtered without a filter produces SHA256 hashes for all
    // files in a tree.
    #[test]
    fn test_compute_dir_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), b"world").unwrap();

        let cksums =
            compute_dir_checksums_filtered(tmp.path(), Path::new("vendor/example-0.1.0"), None)
                .unwrap();
        assert_eq!(cksums.len(), 2);
        assert!(cksums.contains_key("a.txt"));
        assert!(cksums.contains_key("sub/b.txt"));

        let expected_a = format!("{:x}", sha2::Sha256::digest(b"hello"));
        assert_eq!(cksums["a.txt"], expected_a);
    }
}
