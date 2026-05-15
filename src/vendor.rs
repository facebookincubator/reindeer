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
use crate::cargo::postprocess_vendored_directory;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::remap::RemapConfig;
use crate::remap::RemapSource;

pub(crate) fn cargo_vendor(
    config: &Config,
    no_delete: bool,
    #[cfg(fbcode_build)] audit_sec: bool,
    #[cfg(fbcode_build)] no_fetch: bool,
    args: &Args,
    paths: &Paths,
    fast: bool,
) -> anyhow::Result<()> {
    let vendordir = Path::new("vendor"); // relative to third_party_dir
    let full_vendor_dir = paths.third_party_dir.join("vendor");
    let source_filters = matches!(&config.vendor, VendorConfig::Source(_))
        .then(|| build_filters(config, paths))
        .transpose()?;

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
        if fast {
            log::info!("Running fast vendor (library mode)");
            let filters = source_filters.ok_or_else(|| {
                anyhow::anyhow!(
                    "`--fast` currently only works with `vendor = true` in reindeer.toml"
                )
            })?;
            fast_vendor(config, no_delete, args, paths, filters)?;
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
            if let Some(filters) = source_filters {
                postprocess_vendored_directory(&paths.third_party_dir, &filters)?;
            }
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

fn build_filters(config: &Config, paths: &Paths) -> anyhow::Result<VendorFilters> {
    let buck_file_name = if config.buck.split {
        Some((*config.buck.file_name).clone())
    } else {
        None
    };

    let checksum_filter = if let VendorConfig::Source(source_config) = &config.vendor {
        build_checksum_filter(
            config.buck.split,
            &config.buck.file_name,
            &source_config.checksum_exclude,
            &source_config.gitignore_checksum_exclude,
            &paths.third_party_dir,
        )?
    } else {
        None
    };

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
    use std::path::Path;
    use std::path::PathBuf;

    use clap::Parser;

    use super::*;
    fn test_paths_for_dir(third_party_dir: PathBuf) -> crate::Paths {
        let manifest_path = third_party_dir.join("Cargo.toml");
        let cargo_home = third_party_dir.join(".cargo");
        crate::Paths {
            buck_package: "test.third_party".to_owned(),
            third_party_dir,
            lockfile_path: manifest_path.with_file_name("Cargo.lock"),
            manifest_path,
            cargo_home,
        }
    }

    fn fake_cargo_path(root: &Path) -> PathBuf {
        root.join(if cfg!(windows) {
            "fake-cargo.cmd"
        } else {
            "fake-cargo.sh"
        })
    }

    fn write_fake_cargo_vendor_program(fake_cargo: &Path) {
        #[cfg(windows)]
        fs::write(
            fake_cargo,
            r#"@echo off
if not "%1"=="vendor" (
  echo unexpected args: %* 1>&2
  exit /b 1
)
set "vendor_dir=%4"
mkdir "%vendor_dir%\biscuit-1.0.0"
(
echo [package]
echo name = "biscuit"
echo version = "1.0.0"
echo build = "build.rs"
) > "%vendor_dir%\biscuit-1.0.0\Cargo.toml"
> "%vendor_dir%\biscuit-1.0.0\lib.rs" echo pub fn biscuit() {}
> "%vendor_dir%\biscuit-1.0.0\ignore.h" echo // filtered header
> "%vendor_dir%\biscuit-1.0.0\BUCK" echo rust_library(name="biscuit")
> "%vendor_dir%\biscuit-1.0.0\.cargo-checksum.json" echo {"package":"sha256abc","files":{"Cargo.toml":"old","lib.rs":"old","ignore.h":"old","BUCK":"old"}}
echo [source.vendored-sources]
echo directory = "%vendor_dir%"
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            fs::write(
                fake_cargo,
                r#"#!/bin/bash
set -euo pipefail
if [[ "$1" != "vendor" ]]; then
  echo "unexpected args: $@" >&2
  exit 1
fi
vendor_dir="$4"
mkdir -p "$vendor_dir/biscuit-1.0.0"
cat > "$vendor_dir/biscuit-1.0.0/Cargo.toml" <<'EOF'
[package]
name = "biscuit"
version = "1.0.0"
build = "build.rs"
EOF
printf 'pub fn biscuit() {}\n' > "$vendor_dir/biscuit-1.0.0/lib.rs"
printf '// filtered header\n' > "$vendor_dir/biscuit-1.0.0/ignore.h"
printf 'rust_library(name="biscuit")\n' > "$vendor_dir/biscuit-1.0.0/BUCK"
printf '{"package":"sha256abc","files":{"Cargo.toml":"old","lib.rs":"old","ignore.h":"old","BUCK":"old"}}' > "$vendor_dir/biscuit-1.0.0/.cargo-checksum.json"
printf '[source.vendored-sources]\ndirectory = "%s"\n' "$vendor_dir"
"#,
            )
            .unwrap();

            use std::os::unix::fs::PermissionsExt;

            let mut perms = fs::metadata(fake_cargo).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(fake_cargo, perms).unwrap();
        }
    }

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

    #[test]
    fn test_cargo_vendor_fast_errors_without_source_vendor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let third_party_dir = dir.path().join("third-party");
        fs::create_dir_all(&third_party_dir).unwrap();

        let mut config: Config = toml::from_str("vendor = false").unwrap();
        config.config_dir = third_party_dir.clone();

        let args = Args::parse_from(["reindeer", "vendor", "--fast"]);
        let paths = test_paths_for_dir(third_party_dir);

        let err = cargo_vendor(
            &config,
            false,
            #[cfg(fbcode_build)]
            false,
            #[cfg(fbcode_build)]
            false,
            &args,
            &paths,
            true,
        )
        .expect_err("expected fast-vendor error");

        assert!(
            format!("{err:#}")
                .contains("`--fast` currently only works with `vendor = true` in reindeer.toml"),
            "error should explain that fast vendoring only supports source vendoring: {err:#}"
        );
    }

    #[test]
    fn test_postprocess_vendored_directory_preserves_package_checksum() {
        let dir = tempfile::tempdir().expect("tempdir");
        let third_party_dir = dir.path();
        let vendor_dir = third_party_dir.join("vendor");
        let crate_dir = vendor_dir.join("biscuit-1.0.0");
        fs::create_dir_all(&crate_dir).unwrap();
        fs::write(
            crate_dir.join("Cargo.toml"),
            r#"[package]
name = "biscuit"
version = "1.0.0"
build = "build.rs"
"#,
        )
        .unwrap();
        fs::write(crate_dir.join("lib.rs"), "pub fn biscuit() {}\n").unwrap();
        fs::write(crate_dir.join("ignore.h"), "// filtered header\n").unwrap();
        fs::write(crate_dir.join("BUCK"), "rust_library(name=\"biscuit\")\n").unwrap();
        fs::write(
            crate_dir.join(".cargo-checksum.json"),
            r#"{"package":"sha256abc","files":{"Cargo.toml":"old","lib.rs":"old","ignore.h":"old","BUCK":"old"}}"#,
        )
        .unwrap();

        let filters = VendorFilters {
            buck_file_name: Some("BUCK".to_owned()),
            checksum_filter: Some(
                build_checksum_filter(true, "BUCK", &["*.h".to_owned()], &[], third_party_dir)
                    .unwrap()
                    .unwrap(),
            ),
        };

        postprocess_vendored_directory(third_party_dir, &filters).unwrap();

        assert!(
            !crate_dir.join("BUCK").exists(),
            "split BUCK should be removed by the slow-path post-process"
        );
        assert!(
            crate_dir.join("build.rs").exists(),
            "missing build.rs should be synthesized during slow-path post-process"
        );

        let checksum: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate_dir.join(".cargo-checksum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            checksum.get("package").and_then(|value| value.as_str()),
            Some("sha256abc"),
            "slow-path post-process must preserve cargo vendor's package checksum"
        );
        let files = checksum
            .get("files")
            .and_then(|value| value.as_object())
            .unwrap();
        assert!(
            files.contains_key("Cargo.toml") && files.contains_key("lib.rs"),
            "kept files should remain in the checksum map"
        );
        assert!(
            files.contains_key("build.rs"),
            "synthesized build.rs should be added to the checksum map"
        );
        assert!(
            !files.contains_key("BUCK") && !files.contains_key("ignore.h"),
            "split BUCK and checksum-filtered files should be omitted from the checksum map"
        );
    }

    #[test]
    fn test_cargo_vendor_slow_source_path_runs_postprocess_fixups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let third_party_dir = dir.path().join("third-party");
        fs::create_dir_all(&third_party_dir).unwrap();
        fs::write(
            third_party_dir.join("Cargo.toml"),
            r#"[package]
name = "workspace"
version = "0.1.0"
"#,
        )
        .unwrap();

        let fake_cargo = fake_cargo_path(dir.path());
        write_fake_cargo_vendor_program(&fake_cargo);

        let mut config: Config = toml::from_str(
            r#"
[vendor]
checksum_exclude = ["*.h"]

[buck]
split = true
"#,
        )
        .unwrap();
        config.config_dir = third_party_dir.clone();

        let args = Args::parse_from([
            "reindeer",
            "--cargo-path",
            fake_cargo.to_str().unwrap(),
            "vendor",
        ]);
        let paths = test_paths_for_dir(third_party_dir.clone());

        cargo_vendor(
            &config,
            false,
            #[cfg(fbcode_build)]
            false,
            #[cfg(fbcode_build)]
            false,
            &args,
            &paths,
            false,
        )
        .unwrap();

        let crate_dir = third_party_dir.join("vendor/biscuit-1.0.0");
        assert!(
            !crate_dir.join("BUCK").exists(),
            "slow source vendor branch should remove split BUCK files"
        );
        assert!(
            crate_dir.join("build.rs").exists(),
            "slow source vendor branch should synthesize missing build.rs"
        );

        let checksum: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(crate_dir.join(".cargo-checksum.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            checksum.get("package").and_then(|value| value.as_str()),
            Some("sha256abc"),
            "slow source vendor branch should preserve cargo vendor's package checksum"
        );
        let files = checksum
            .get("files")
            .and_then(|value| value.as_object())
            .unwrap();
        assert!(
            !files.contains_key("BUCK") && !files.contains_key("ignore.h"),
            "restored slow-path fixups should rewrite filtered checksum entries"
        );
    }
}
