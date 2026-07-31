/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;
use cargo_toml::OptionalFile;

use crate::config::Config;
use crate::fast_vendor::ExpectedCrate;
use crate::fast_vendor::SYNTHESIZED_BUILD_RS;
use crate::fast_vendor::cargo_checksum::checksum_json_bytes;
use crate::fast_vendor::cargo_checksum::compute_dir_checksums_filtered;
use crate::fast_vendor::filter::VendorFilter;
use crate::fast_vendor::limit_reader::LimitReader;
use crate::fast_vendor::materialization_excluded;
use crate::fast_vendor::normalize_manifest_path;
use crate::fast_vendor::prepare_regular_file_target;
use crate::fast_vendor::remove_existing_path;
use crate::fast_vendor::write_regular_file;

pub(super) enum Materialization {
    RegistryArchive {
        archive: PathBuf,
    },
    CopyFiles {
        src_root: PathBuf,
        file_paths: Vec<PathBuf>,
        normalized_cargo_toml: Option<String>,
    },
}

pub(super) fn materialize_expected_crate(
    config: &Config,
    expected: &ExpectedCrate,
    staging_dst: &Path,
    filter: &VendorFilter,
) -> anyhow::Result<()> {
    match &expected.materialization {
        Materialization::RegistryArchive { archive } => {
            unpack_package_archive(config, archive, staging_dst).with_context(|| {
                format!(
                    "failed to unpack {} into {}",
                    archive.display(),
                    staging_dst.display(),
                )
            })?;
        }
        Materialization::CopyFiles {
            src_root,
            file_paths,
            normalized_cargo_toml,
        } => {
            copy_vendor_sources(
                config,
                src_root,
                file_paths,
                normalized_cargo_toml.as_deref(),
                staging_dst,
            )?;
        }
    }
    postprocess_vendored_crate_dir(
        config,
        staging_dst,
        &expected.pkgdir,
        filter,
        expected.pkg_cksum.as_deref(),
    )
}

/// Copy source files from a non-registry package into the vendor directory.
///
/// BUCK files (when `config.buck.split` is set) and VCS bookkeeping files are
/// excluded from the copy entirely. Files matched by checksum globs or
/// configured gitignore rules are copied to disk and handled later when
/// `.cargo-checksum.json` is rewritten.
fn copy_vendor_sources(
    config: &Config,
    src_root: &Path,
    file_paths: &[PathBuf],
    normalized_cargo_toml: Option<&str>,
    dst: &Path,
) -> anyhow::Result<()> {
    for src_path in file_paths {
        let relative = src_path.strip_prefix(src_root).with_context(|| {
            format!("{} is not under {}", src_path.display(), src_root.display(),)
        })?;

        if materialization_excluded(config, relative) {
            continue;
        }

        // Build destination preserving directory structure.
        let dst_path = relative
            .iter()
            .fold(dst.to_path_buf(), |acc, component| acc.join(component));

        if let Some(parent) = dst_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");
        let contents = if key == "Cargo.toml" {
            match normalized_cargo_toml {
                Some(contents) => contents.as_bytes().to_vec(),
                None => fs::read(src_path)
                    .with_context(|| format!("failed to read {}", src_path.display()))?,
            }
        } else {
            fs::read(src_path).with_context(|| format!("failed to read {}", src_path.display()))?
        };
        write_vendored_source_file(
            #[cfg(unix)]
            src_path,
            &dst_path,
            &contents,
            #[cfg(unix)]
            {
                key == "Cargo.toml" && normalized_cargo_toml.is_some()
            },
        )?;
    }

    Ok(())
}

fn write_vendored_source_file(
    #[cfg(unix)] src_path: &Path,
    dst_path: &Path,
    contents: &[u8],
    #[cfg(unix)] generated: bool,
) -> anyhow::Result<()> {
    prepare_regular_file_target(dst_path)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if !generated {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::OpenOptionsExt;

        let mode = fs::metadata(src_path)
            .with_context(|| format!("failed to stat {}", src_path.display()))?
            .mode();
        options.mode(mode);
    }

    let mut file = options
        .open(dst_path)
        .with_context(|| format!("failed to write {}", dst_path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", dst_path.display()))
}

fn remove_split_buck_file(config: &Config, crate_dir: &Path) -> anyhow::Result<()> {
    if config.buck.split {
        let buck_path = crate_dir.join(&*config.buck.file_name);
        remove_existing_path(&buck_path)?;
    }
    Ok(())
}

/// Synthesize a stub `build.rs` if the manifest declares a build script path
/// that does not exist on disk.
///
/// Workaround for [cargo#14348]: some crates declare a non-standard build
/// script path in `Cargo.toml` but omit the file from the published tarball.
/// Without the stub, cargo refuses to compile the crate.
fn synthesize_missing_build_rs(crate_dir: &Path) -> anyhow::Result<()> {
    type TomlManifest = cargo_toml::Manifest<serde::de::IgnoredAny>;

    let cargo_toml_path = crate_dir.join("Cargo.toml");
    let content = match fs::read_to_string(&cargo_toml_path) {
        Ok(s) => s,
        Err(err) => {
            log::warn!("Failed to read {}: {}", cargo_toml_path.display(), err);
            return Ok(());
        }
    };

    let manifest: TomlManifest = match toml::from_str(&content) {
        Ok(m) => m,
        Err(err) => {
            log::warn!(
                "Failed to deserialize {}: {}",
                cargo_toml_path.display(),
                err
            );
            return Ok(());
        }
    };

    if let Some(package) = &manifest.package {
        if let Some(OptionalFile::Path(build_script_path)) = &package.build {
            let expected = crate_dir.join(normalize_manifest_path(build_script_path));
            if !expected.try_exists()? {
                log::trace!("Synthesizing build script {}", expected.display());
                if let Some(parent) = expected.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(expected, SYNTHESIZED_BUILD_RS)?;
            }
        }
    }

    Ok(())
}

fn postprocess_vendored_crate_dir(
    config: &Config,
    crate_dir: &Path,
    pkgdir: &Path,
    filter: &VendorFilter,
    pkg_cksum: Option<&str>,
) -> anyhow::Result<()> {
    remove_split_buck_file(config, crate_dir)?;
    remove_existing_path(&crate_dir.join(".cargo-checksum.json"))?;
    synthesize_missing_build_rs(crate_dir)?;
    let file_cksums = compute_dir_checksums_filtered(config, crate_dir, pkgdir, filter)?;
    write_checksum_json(crate_dir, pkg_cksum, &file_cksums)
}

/// Write `.cargo-checksum.json` into a vendored crate directory.
fn write_checksum_json(
    dst: &Path,
    pkg_cksum: Option<&str>,
    file_cksums: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let cksum_path = dst.join(".cargo-checksum.json");
    write_regular_file(&cksum_path, &checksum_json_bytes(pkg_cksum, file_cksums)?)
        .with_context(|| format!("failed to write {}", cksum_path.display()))
}

/// Unpacks a `.crate` archive into `dst`, applying `include` to each entry
/// relative to the crate root. Replicates cargo's zip-bomb and path-traversal
/// protections. Size limit: 512 MiB minimum, or 20x the compressed archive
/// size (matching cargo's defaults).
fn unpack_package_archive(config: &Config, archive: &Path, dst: &Path) -> anyhow::Result<()> {
    let tarball =
        fs::File::open(archive).with_context(|| format!("failed to open {}", archive.display()))?;
    let archive_size = tarball.metadata()?.len();
    let size_limit = u64::max(512 * 1024 * 1024, archive_size * 20);

    let gz = flate2::read::GzDecoder::new(LimitReader::new(tarball, size_limit));
    let mut tar = tar::Archive::new(gz);

    let prefix = dst.file_name().expect("dst must have a file name");
    let parent = dst.parent().expect("dst must have a parent directory");

    for entry in tar.entries().context("failed to read archive entries")? {
        let mut entry = entry.context("failed to read archive entry")?;
        let entry_path = entry
            .path()
            .context("failed to read entry path")?
            .into_owned();

        let Ok(relative) = entry_path.strip_prefix(prefix) else {
            bail!("invalid tarball: entry at {entry_path:?} is not under {prefix:?}");
        };

        if materialization_excluded(config, relative) {
            continue;
        }

        // Skip `.cargo-ok` -- cargo's unpack-success marker file.
        if entry_path.file_name().is_some_and(|n| n == ".cargo-ok") {
            continue;
        }

        entry
            .unpack_in(parent)
            .with_context(|| format!("failed to unpack `{}`", entry_path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use crate::config::Config;
    use crate::fast_vendor::ExpectedCrate;
    use crate::fast_vendor::cargo_checksum::compute_dir_checksums_filtered;
    use crate::fast_vendor::fingerprint::vendor_dir_matches_expected_source;
    use crate::fast_vendor::materialization::Materialization;
    use crate::fast_vendor::materialization::copy_vendor_sources;
    use crate::fast_vendor::materialization::postprocess_vendored_crate_dir;
    use crate::fast_vendor::materialization::synthesize_missing_build_rs;
    use crate::fast_vendor::materialization::write_checksum_json;
    use crate::fast_vendor::tests::empty_filter;
    use crate::fast_vendor::tests::gitignore_filter;

    #[test]
    fn test_vendor_dir_matches_expected_source_does_not_trust_checksum_json() {
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();
        fs::write(
            src.join("Cargo.toml"),
            r#"[package]
name = "example"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(
            src.join(".cargo-checksum.json"),
            r#"{"package":"package-checksum","files":{"lib.rs":"wrong"}}"#,
        )
        .unwrap();

        fs::write(
            actual.join("Cargo.toml"),
            r#"[package]
name = "example"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let filter = empty_filter();
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(
            &config,
            &actual,
            &pkgdir,
            &filter,
            Some("package-checksum"),
        )
        .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual.clone(),
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join(".cargo-checksum.json"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&config, &expected, &filter).unwrap(),
            "matching source contents should take the fast no-op path"
        );

        fs::write(
            actual.join(".cargo-checksum.json"),
            r#"{"package":"package-checksum","files":{"lib.rs":"wrong"}}"#,
        )
        .unwrap();

        assert!(
            !vendor_dir_matches_expected_source(&config, &expected, &filter).unwrap(),
            "editing checksum metadata must invalidate the fast no-op path"
        );
    }

    #[test]
    fn test_vendor_dir_matches_expected_source_ignores_gitignore_filtered_files() {
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();

        let cargo_toml = r#"[package]
name = "example"
version = "0.1.0"
"#;
        fs::write(src.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(src.join("Cargo.lock"), b"source lockfile\n").unwrap();

        fs::write(actual.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let filter = gitignore_filter("vendor/*/Cargo.lock");
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(
            &config,
            &actual,
            &pkgdir,
            &filter,
            Some("package-checksum"),
        )
        .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual.clone(),
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join("Cargo.lock"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&config, &expected, &filter).unwrap(),
            "gitignore-filtered source files missing from actual should not invalidate no-op"
        );

        fs::write(actual.join("Cargo.lock"), b"actual lockfile\n").unwrap();
        assert!(
            vendor_dir_matches_expected_source(&config, &expected, &filter).unwrap(),
            "gitignore-filtered actual files should not invalidate no-op"
        );
    }

    #[test]
    fn test_vendor_dir_matches_expected_source_normalizes_build_script_path() {
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("source");
        let actual = dir.path().join("actual");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&actual).unwrap();

        let cargo_toml = r#"[package]
name = "example"
version = "0.1.0"
build = "./build.rs"
"#;
        fs::write(src.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(src.join("build.rs"), b"fn main() {}\n").unwrap();

        fs::write(actual.join("Cargo.toml"), cargo_toml).unwrap();
        fs::write(actual.join("lib.rs"), b"pub fn example() {}\n").unwrap();
        fs::write(actual.join("build.rs"), b"fn main() {}\n").unwrap();

        let filter = empty_filter();
        let pkgdir = PathBuf::from("vendor/example-0.1.0");
        postprocess_vendored_crate_dir(
            &config,
            &actual,
            &pkgdir,
            &filter,
            Some("package-checksum"),
        )
        .unwrap();

        let expected = ExpectedCrate {
            dst_name: "example-0.1.0".to_owned(),
            dst: actual,
            pkgdir,
            pkg_cksum: Some("package-checksum".to_owned()),
            materialization: Materialization::CopyFiles {
                src_root: src.clone(),
                file_paths: vec![
                    src.join("Cargo.toml"),
                    src.join("lib.rs"),
                    src.join("build.rs"),
                ],
                normalized_cargo_toml: None,
            },
        };

        assert!(
            vendor_dir_matches_expected_source(&config, &expected, &filter).unwrap(),
            "`./build.rs` in Cargo.toml should match `build.rs` in the vendor tree"
        );
    }

    #[test]
    fn test_synthesize_missing_build_rs_creates_stub() {
        // When Cargo.toml declares a build script path that does not exist,
        // synthesize_missing_build_rs should create a stub.
        let config = Config::default_for_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "dragon-breath"
version = "0.1.0"
build = "build.rs"
"#,
        )
        .unwrap();

        assert!(
            !root.join("build.rs").exists(),
            "build.rs should not exist before synthesis"
        );

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        assert!(
            root.join("build.rs").exists(),
            "build.rs should be created by synthesis"
        );
        let stub = fs::read_to_string(root.join("build.rs")).unwrap();
        assert_eq!(
            stub, "fn main() {}\n",
            "stub should contain only a main function"
        );

        let pkgdir = std::path::Path::new("vendor/dragon-breath-0.1.0");
        let filter = empty_filter();
        let cksums = compute_dir_checksums_filtered(&config, root, pkgdir, &filter)
            .expect("checksums computed");
        assert!(
            cksums.contains_key("build.rs"),
            "synthesized build.rs must be included in .cargo-checksum.json"
        );
    }

    #[test]
    fn test_synthesize_no_op_when_build_rs_present() {
        // When build.rs already exists, synthesize_missing_build_rs should
        // leave it unchanged.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let original = b"fn main() { println!(\"cargo:rustc-cfg=boomerang\"); }\n";
        fs::write(root.join("build.rs"), original).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "always-comes-back"
version = "1.0.0"
build = "build.rs"
"#,
        )
        .unwrap();

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        let after = fs::read(root.join("build.rs")).unwrap();
        assert_eq!(
            after, original,
            "existing build.rs should not be overwritten"
        );
    }

    #[test]
    fn test_synthesize_missing_build_rs_creates_parent_dirs() {
        // Non-root build script paths should create their missing parent dirs
        // before the stub is written.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "nested-boomerang"
version = "0.1.0"
build = "scripts/build.rs"
"#,
        )
        .unwrap();

        synthesize_missing_build_rs(root).expect("synthesis succeeded");

        let nested = root.join("scripts").join("build.rs");
        assert!(nested.exists(), "nested build.rs should be created");
        let stub = fs::read_to_string(nested).unwrap();
        assert_eq!(stub, "fn main() {}\n");
    }

    #[test]
    fn test_copy_vendor_sources_excludes_buck_file() {
        let config = Config::split_for_test();
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(src.join("lib.rs"), b"pub fn fell_off_a_truck() {}").unwrap();
        fs::write(src.join("BUCK"), b"rust_library(name=\"fell-off-a-truck\")").unwrap();

        let file_paths = vec![src.join("lib.rs"), src.join("BUCK")];

        copy_vendor_sources(&config, src, &file_paths, None, dst).expect("copy succeeded");

        assert!(
            dst.join("lib.rs").exists(),
            "lib.rs should be copied to dst"
        );
        assert!(
            !dst.join("BUCK").exists(),
            "BUCK should not be written to dst"
        );
    }

    #[test]
    fn test_copy_vendor_sources_gitignore_filter_keeps_source_file() {
        let config = Config::default_for_test();
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(
            src.join("Cargo.toml"),
            b"[package]\nname = \"normalized\"\n",
        )
        .unwrap();
        fs::write(src.join("Cargo.toml.orig"), b"[package]\nname = \"orig\"\n").unwrap();

        let file_paths = vec![src.join("Cargo.toml"), src.join("Cargo.toml.orig")];

        copy_vendor_sources(&config, src, &file_paths, None, dst).expect("copy succeeded");

        assert!(
            dst.join("Cargo.toml.orig").exists(),
            "gitignore-matched Cargo.toml.orig should still be copied to dst"
        );
    }

    #[test]
    fn test_copy_vendor_sources_uses_normalized_cargo_toml() {
        let config = Config::default_for_test();
        let src_dir = tempfile::tempdir().expect("src tempdir");
        let dst_dir = tempfile::tempdir().expect("dst tempdir");
        let src = src_dir.path();
        let dst = dst_dir.path();

        fs::write(
            src.join("Cargo.toml"),
            b"[package]\nname.workspace = true\n",
        )
        .unwrap();
        fs::write(src.join("lib.rs"), b"pub fn example() {}\n").unwrap();

        let file_paths = vec![src.join("Cargo.toml"), src.join("lib.rs")];
        let normalized =
            "# THIS FILE IS AUTOMATICALLY GENERATED BY CARGO\n\n[package]\nname = \"example\"\n";

        copy_vendor_sources(&config, src, &file_paths, Some(normalized), dst)
            .expect("copy succeeded");

        assert_eq!(
            fs::read_to_string(dst.join("Cargo.toml")).unwrap(),
            normalized
        );
    }

    // Invariant: write_checksum_json creates .cargo-checksum.json with package and files fields
    #[test]
    fn test_write_checksum_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("src/lib.rs".to_owned(), "abc123".to_owned());

        write_checksum_json(tmp.path(), Some("pkg_hash_xyz"), &files).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".cargo-checksum.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["package"], "pkg_hash_xyz");
        assert_eq!(parsed["files"]["src/lib.rs"], "abc123");
    }

    // Invariant: write_checksum_json handles None package checksum as JSON null
    #[test]
    fn test_write_checksum_json_null_package() {
        let tmp = tempfile::tempdir().unwrap();
        let files = BTreeMap::new();

        write_checksum_json(tmp.path(), None, &files).unwrap();

        let content = std::fs::read_to_string(tmp.path().join(".cargo-checksum.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["package"].is_null());
    }

    // Invariant: copy_vendor_sources copies files while skipping excluded paths.
    #[test]
    fn test_copy_vendor_sources() {
        let config = Config::default_for_test();
        let src_dir = tempfile::tempdir().unwrap();
        std::fs::write(src_dir.path().join("lib.rs"), b"fn main() {}").unwrap();
        std::fs::write(src_dir.path().join(".gitignore"), b"/target").unwrap();
        std::fs::write(src_dir.path().join("Cargo.toml"), b"[package]").unwrap();

        let dst_dir = tempfile::tempdir().unwrap();
        let file_paths = vec![
            src_dir.path().join("lib.rs"),
            src_dir.path().join(".gitignore"),
            src_dir.path().join("Cargo.toml"),
        ];

        copy_vendor_sources(&config, src_dir.path(), &file_paths, None, dst_dir.path()).unwrap();

        assert!(dst_dir.path().join("lib.rs").exists());
        assert!(!dst_dir.path().join(".gitignore").exists());
        assert!(dst_dir.path().join("Cargo.toml").exists());
    }
}
