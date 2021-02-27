/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Global third-party config

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::platform::{PlatformConfig, PlatformName};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Path the config was read from
    #[serde(skip)]
    pub config_path: PathBuf,

    /// Default flags applied to all rules
    #[serde(default)]
    pub rustc_flags: Vec<String>,

    /// Platform-specific rustc flags
    #[serde(default)]
    pub platform_rustc_flags: BTreeMap<PlatformName, Vec<String>>,

    /// Try to compute a precise list of sources rather than using globbing
    #[serde(default)]
    pub precise_srcs: bool,

    /// List of glob patterns for filenames likely to contain license terms
    #[serde(default)]
    pub license_patterns: BTreeSet<String>,

    /// Generate fixup file templates when missing
    #[serde(default)]
    pub fixup_templates: bool,

    /// Path to buildifier (if relative, relative to here)
    #[serde(default)]
    pub buildifier_path: Option<PathBuf>,

    /// Include root package as top-level public target in Buck file
    #[serde(default)]
    pub include_top_level: bool,

    /// Placeholder for backwards compat - now `include_top_level` implies
    /// public.
    #[serde(rename = "public_top_level", default)]
    _public_top_level: bool,

    /// Include extra top-level targets for things like
    /// binary and cdylib-only packages
    #[serde(default)]
    pub extra_top_levels: bool,

    /// Emit metadata for each crate into Buck rules
    #[serde(default)]
    pub emit_metadata: bool,

    #[serde(default)]
    pub cargo: CargoConfig,

    #[serde(default)]
    pub buck: BuckConfig,

    #[serde(default)]
    pub vendor: VendorConfig,

    #[serde(default)]
    pub audit: AuditConfig,

    #[serde(default)]
    pub platform: HashMap<PlatformName, PlatformConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoConfig {
    /// Path to cargo executable. If set, then relative to this file
    #[serde(default)]
    pub cargo: Option<PathBuf>,
    /// Always version vendor directories (requires cargo with --versioned-dirs)
    #[serde(default)]
    pub versioned_dirs: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuckConfig {
    /// Name of BUCK file
    #[serde(default = "default_buck_file_name")]
    pub file_name: String,
    /// Rust targets filename (.bzl with set of all target names - not generated if omitted)
    #[serde(default)]
    pub targets_name: Option<String>,
    /// Banner for the top of all generated bzl files, namely BUCK and METADATA.bzl
    #[serde(default)]
    pub generated_file_header: String,
    /// Front matter for the generated BUCK file
    #[serde(default)]
    pub buckfile_imports: String,

    /// Rule name for rust_library
    #[serde(default = "default_rust_library")]
    pub rust_library: String,
    /// Rule name for rust_binary
    #[serde(default = "default_rust_binary")]
    pub rust_binary: String,
    /// Rule name for cxx_library
    #[serde(default = "default_cxx_library")]
    pub cxx_library: String,
    /// Rule name for prebuilt_cxx_library
    #[serde(default = "default_prebuilt_cxx_library")]
    pub prebuilt_cxx_library: String,
    /// Rust name for buildscript_genrule producing args
    #[serde(default = "default_buildscript_genrule_args")]
    pub buildscript_genrule_args: String,
    /// Rust name for buildscript_genrule producing arfs
    #[serde(default = "default_buildscript_genrule_srcs")]
    pub buildscript_genrule_srcs: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorConfig {
    /// List of .gitignore files to use to filter checksum files, relative to
    /// this config file.
    #[serde(default)]
    pub gitignore_checksum_exclude: HashSet<PathBuf>,
    /// Set of globs to remove from Cargo's checksun files in vendored dirs
    #[serde(default)]
    pub checksum_exclude: HashSet<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// List of package names to never attempt to autofix
    #[serde(default)]
    pub never_autofix: HashSet<String>,
}

fn default_buck_file_name() -> String {
    BuckConfig::default().file_name
}

fn default_rust_library() -> String {
    BuckConfig::default().rust_library
}

fn default_rust_binary() -> String {
    BuckConfig::default().rust_binary
}

fn default_cxx_library() -> String {
    BuckConfig::default().cxx_library
}

fn default_prebuilt_cxx_library() -> String {
    BuckConfig::default().prebuilt_cxx_library
}

fn default_buildscript_genrule_args() -> String {
    BuckConfig::default().buildscript_genrule_args
}

fn default_buildscript_genrule_srcs() -> String {
    BuckConfig::default().buildscript_genrule_srcs
}

impl Default for BuckConfig {
    fn default() -> Self {
        BuckConfig {
            file_name: "BUCK".to_string(),
            targets_name: None,
            generated_file_header: String::new(),
            buckfile_imports: String::new(),

            rust_library: "rust_library".to_string(),
            rust_binary: "rust_binary".to_string(),
            cxx_library: "cxx_library".to_string(),
            prebuilt_cxx_library: "prebuilt_cxx_library".to_string(),
            buildscript_genrule_args: "buildscript_args".to_string(),
            buildscript_genrule_srcs: "buildscript_srcs".to_string(),
        }
    }
}

pub fn read_config(dir: &Path) -> Result<Config> {
    let path = dir.join("reindeer.toml");

    let file = match fs::read(&path) {
        Ok(file) => file,
        Err(ref err) if err.kind() == ErrorKind::NotFound => {
            return Ok(Config {
                config_path: dir.to_path_buf(),
                ..Config::default()
            });
        }
        Err(err) => return Err(err).context(format!("Failed to read config {}", path.display())),
    };

    let config: Config =
        toml::de::from_slice(&file).context(format!("Failed to parse {}", path.display()))?;

    log::debug!("Read config {:#?}", config);

    Ok(config)
}
