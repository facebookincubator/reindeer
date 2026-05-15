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
    use clap::Parser;

    use super::*;
    use crate::config::VendorSourceConfig;

    #[cfg(unix)]
    fn write_fake_cargo_script(script: &Path) {
        let script_body = r#"#!/bin/sh
set -eu
vendor_dir="$4"
crate_dir="$vendor_dir/sourdough-starter-1.0.0"
mkdir -p "$crate_dir"
printf '# generated\n' > "$crate_dir/BUCK"
printf 'pub fn sourdough() {}\n' > "$crate_dir/lib.rs"
printf '// filtered header\n' > "$crate_dir/ignore.h"
cat > "$crate_dir/Cargo.toml" <<'EOF'
[package]
name = "sourdough-starter"
version = "1.0.0"
build = "build.rs"
EOF
printf '{"files":{"BUCK":"abc123","ignore.h":"def456","lib.rs":"ghi789"},"package":null}' > "$crate_dir/.cargo-checksum.json"
printf '[source.vendored-sources]\ndirectory = "%s"\n' "$vendor_dir"
"#;

        fs::write(script, script_body).expect("write fake cargo script");
    }

    #[cfg(unix)]
    fn fake_vendor_args(script: &Path) -> Args {
        let script = script.to_string_lossy().into_owned();
        Args::try_parse_from([
            "reindeer".to_owned(),
            "--cargo-path".to_owned(),
            "sh".to_owned(),
            "--cargo-options".to_owned(),
            script,
            "vendor".to_owned(),
        ])
        .expect("parse fake cargo args")
    }

    #[cfg(unix)]
    fn make_test_paths(third_party_dir: &Path) -> Paths {
        Paths {
            buck_package: "fbcode/common/rust/tools/reindeer".to_owned(),
            third_party_dir: third_party_dir.to_path_buf(),
            manifest_path: third_party_dir.join("Cargo.toml"),
            lockfile_path: third_party_dir.join("Cargo.lock"),
            cargo_home: third_party_dir.join(".cargo"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_cargo_vendor_slow_path_runs_post_process_vendor() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let third_party_dir = dir.path().join("third-party");
        fs::create_dir_all(&third_party_dir).expect("create third_party_dir");

        let script = third_party_dir.join("fake_cargo.sh");
        write_fake_cargo_script(&script);

        let args = fake_vendor_args(&script);

        let reindeer_toml = third_party_dir.join("reindeer.toml");
        fs::write(&reindeer_toml, "").expect("write reindeer.toml");

        let mut config =
            crate::config::read_config(&reindeer_toml, &args).expect("read test config");
        config.buck.split = true;
        config.vendor = VendorConfig::Source(VendorSourceConfig {
            checksum_exclude: vec!["*.h".to_owned()],
            ..VendorSourceConfig::default()
        });

        let paths = make_test_paths(&third_party_dir);
        fs::write(
            &paths.manifest_path,
            "[package]\nname = \"workspace\"\nversion = \"0.1.0\"\n",
        )
        .expect("write workspace manifest");

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
        .expect("slow cargo vendor should succeed");

        let crate_dir = third_party_dir
            .join("vendor")
            .join("sourdough-starter-1.0.0");
        let checksum: serde_json::Value = serde_json::from_slice(
            &fs::read(crate_dir.join(".cargo-checksum.json")).expect("read checksum json"),
        )
        .expect("parse checksum json");
        let files = checksum
            .get("files")
            .and_then(|value| value.as_object())
            .expect("checksum json should contain object-valued files entry");

        assert!(
            !crate_dir.join("BUCK").exists(),
            "slow source vendoring should still remove split BUCK files"
        );
        assert!(
            crate_dir.join("build.rs").exists(),
            "slow source vendoring should still synthesize missing build scripts"
        );
        assert!(
            files.contains_key("lib.rs"),
            "kept files should remain in the rewritten checksum map"
        );
        assert!(
            !files.contains_key("BUCK"),
            "split BUCK should be removed from the checksum map"
        );
        assert!(
            !files.contains_key("ignore.h"),
            "checksum-excluded files should be omitted from the checksum map"
        );
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
}
