/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Global third-party config

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use monostate::MustBe;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Deserializer;
use serde::de::MapAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;

use crate::platform::PlatformConfig;
use crate::platform::PlatformName;
use crate::universe::UniverseConfig;
use crate::universe::UniverseName;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Path the config was read from
    #[serde(skip)]
    pub config_dir: PathBuf,

    /// Path to the Cargo.toml we are buckifying
    #[serde(default)]
    pub manifest_path: Option<PathBuf>,

    /// Where to write the output.
    ///
    /// (Default = "", i.e. current directory)
    #[serde(default)]
    pub third_party_dir: Option<PathBuf>,

    /// Try to compute a precise list of sources rather than using globbing
    #[serde(default)]
    pub precise_srcs: bool,

    /// List of glob patterns for filenames likely to contain license terms
    #[serde(default)]
    pub license_patterns: BTreeSet<String>,

    /// Generate fixup file templates when missing
    #[serde(default)]
    pub fixup_templates: bool,

    /// Fail buckify if there are unresolved fixups
    #[serde(default)]
    pub unresolved_fixup_error: bool,

    ///Provide additional information to resolve unresolved fixup errors
    #[serde(default)]
    pub unresolved_fixup_error_message: Option<String>,

    /// Include root package as top-level public target in Buck file
    #[serde(default)]
    pub include_top_level: bool,

    /// Include workspace members in the generated BUCK file.
    ///
    /// Generlly workspace members are located outside the third party directory.
    /// So this is probably only relevant if you are keeping a workspace
    /// Cargo.toml and its generated BUCK file in the same directory.
    #[serde(default)]
    pub include_workspace_members: bool,

    /// Use strict glob matching
    #[serde(default)]
    pub strict_globs: bool,

    #[serde(default)]
    pub cargo: CargoConfig,

    #[serde(default)]
    pub buck: BuckConfig,

    #[serde(default, deserialize_with = "deserialize_vendor_config")]
    pub vendor: VendorConfig,

    #[serde(default = "default_platforms")]
    pub platform: HashMap<PlatformName, PlatformConfig>,

    #[serde(default = "default_universes")]
    pub universe: BTreeMap<UniverseName, UniverseConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoConfig {
    /// Path to cargo executable. If set, then relative to this file
    #[serde(default)]
    pub cargo: Option<PathBuf>,
    /// Path to rustc executable. If set, then relative to this file
    pub rustc: Option<PathBuf>,
    /// Support Cargo's unstable "artifact dependencies" functionality, RFC 3028.
    #[serde(default)]
    pub bindeps: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuckConfig {
    /// Name of BUCK file
    #[serde(default)]
    pub file_name: StringWithDefault<MustBe!("BUCK")>,

    /// Banner for the top of all generated bzl files, namely BUCK and METADATA.bzl
    #[serde(default)]
    pub generated_file_header:
        StringWithDefault<MustBe!("# \x40generated by `reindeer buckify`\n")>,

    /// Front matter for the generated BUCK file
    #[serde(default)]
    pub buckfile_imports: StringWithDefault<MustBe!("")>,

    /// Rule name for alias
    #[serde(default)]
    pub alias: StringWithDefault<MustBe!("alias")>,
    /// Rule name for filegroup
    #[serde(default)]
    pub filegroup: StringWithDefault<MustBe!("filegroup")>,
    /// Rule name for http_archive
    #[serde(default)]
    pub http_archive: StringWithDefault<MustBe!("http_archive")>,
    #[serde(default)]
    pub extract_archive: StringWithDefault<MustBe!("extract_archive")>,
    /// Rule name for git_fetch
    #[serde(default)]
    pub git_fetch: StringWithDefault<MustBe!("git_fetch")>,
    /// Rule name for rust_library
    #[serde(default)]
    pub rust_library: StringWithDefault<MustBe!("rust_library")>,
    /// Rule name for rust_binary
    #[serde(default)]
    pub rust_binary: StringWithDefault<MustBe!("rust_binary")>,
    /// Rule name for cxx_library
    #[serde(default)]
    pub cxx_library: StringWithDefault<MustBe!("cxx_library")>,
    /// Rule name for prebuilt_cxx_library
    #[serde(default)]
    pub prebuilt_cxx_library: StringWithDefault<MustBe!("prebuilt_cxx_library")>,
    /// Rule name for the rust_binary of a build script
    pub buildscript_binary: Option<String>,
    /// Rule name for a build script invocation
    #[serde(default)]
    pub buildscript_genrule: StringWithDefault<MustBe!("buildscript_run")>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum VendorConfig {
    Off,
    LocalRegistry,
    Source(VendorSourceConfig),
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorSourceConfig {
    /// List of .gitignore files to use to filter checksum files, relative to
    /// this config file.
    #[serde(default)]
    pub gitignore_checksum_exclude: HashSet<PathBuf>,
    /// Set of globs to remove from Cargo's checksun files in vendored dirs
    #[serde(default)]
    pub checksum_exclude: HashSet<String>,
}

impl Default for VendorConfig {
    fn default() -> Self {
        VendorConfig::Source(Default::default())
    }
}

#[derive(Clone)]
pub struct StringWithDefault<T> {
    pub value: String,
    pub is_default: bool,
    default: PhantomData<T>,
}

impl<T: Default + Serialize> Default for StringWithDefault<T> {
    fn default() -> Self {
        struct DefaultValue<T>(T);
        impl<T: Serialize> Display for DefaultValue<T> {
            fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                self.0.serialize(formatter)
            }
        }

        let mut value = String::new();
        write!(value, "{}", DefaultValue(T::default())).unwrap();
        StringWithDefault {
            value,
            is_default: true,
            default: PhantomData,
        }
    }
}

impl<T> Deref for StringWithDefault<T> {
    type Target = String;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> AsRef<Path> for StringWithDefault<T> {
    fn as_ref(&self) -> &Path {
        self.value.as_ref()
    }
}

impl<T> Display for StringWithDefault<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.value, formatter)
    }
}

impl<T> Debug for StringWithDefault<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_default {
            formatter.pad(&format!("Default ({:?})", &self.value))
        } else {
            Debug::fmt(&self.value, formatter)
        }
    }
}

impl<'de, T> Deserialize<'de> for StringWithDefault<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

impl<T> From<String> for StringWithDefault<T> {
    fn from(value: String) -> Self {
        StringWithDefault {
            value,
            is_default: false,
            default: PhantomData,
        }
    }
}

fn default_platforms() -> HashMap<PlatformName, PlatformConfig> {
    const DEFAULT_PLATFORMS_TOML: &str = include_str!("default_platforms.toml");

    #[derive(Deserialize)]
    struct DefaultConfig {
        platform: HashMap<PlatformName, PlatformConfig>,
    }

    toml::from_str::<DefaultConfig>(DEFAULT_PLATFORMS_TOML)
        .unwrap()
        .platform
}

fn default_universes() -> BTreeMap<UniverseName, UniverseConfig> {
    let mut map = BTreeMap::new();
    map.insert(Default::default(), Default::default());
    map
}

fn deserialize_vendor_config<'de, D>(deserializer: D) -> Result<VendorConfig, D::Error>
where
    D: Deserializer<'de>,
{
    struct VendorConfigVisitor;

    impl<'de> Visitor<'de> for VendorConfigVisitor {
        type Value = VendorConfig;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("[vendor] section, or `vendor = false`")
        }

        fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            // `vendor = true`: default configuration with vendoring.
            // `vendor = false`: do not vendor.
            Ok(if value {
                VendorConfig::default()
            } else {
                VendorConfig::Off
            })
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if v == "local-registry" {
                Ok(VendorConfig::LocalRegistry)
            } else {
                Err(E::custom("unknown vendor type"))
            }
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            VendorSourceConfig::deserialize(MapAccessDeserializer::new(map))
                .map(VendorConfig::Source)
        }
    }

    deserializer.deserialize_any(VendorConfigVisitor)
}

pub fn read_config(reindeer_toml: &Path) -> anyhow::Result<Config> {
    let dir = reindeer_toml
        .parent()
        .context("Invalid path to reindeer.toml")?;
    let mut config = try_read_config(reindeer_toml)?;

    config.config_dir = dir.to_path_buf();

    if config.buck.buckfile_imports.is_default {
        // Fill in some prelude imports so Reindeer generates working targets
        // out of the box.
        let mut buckfile_imports = String::new();

        if config.buck.buildscript_genrule.is_default {
            buckfile_imports
                .push_str("load(\"@prelude//rust:cargo_buildscript.bzl\", \"buildscript_run\")\n");
        }

        if config.buck.rust_library.is_default && config.buck.rust_binary.is_default {
            buckfile_imports.push_str("load(\"@prelude//rust:cargo_package.bzl\", \"cargo\")\n");
            config.buck.rust_library = "cargo.rust_library".to_owned().into();
            config.buck.rust_binary = "cargo.rust_binary".to_owned().into();
        }

        config.buck.buckfile_imports = buckfile_imports.into();
    }

    Ok(config)
}

fn try_read_config(path: &Path) -> anyhow::Result<Config> {
    let file = match fs::read_to_string(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let empty_config = toml::Table::new();
            return Ok(Config::deserialize(empty_config).unwrap());
        }
        Err(err) => return Err(err).context(format!("Failed to read config {}", path.display())),
    };

    let config: Config =
        toml::from_str(&file).context(format!("Failed to parse {}", path.display()))?;

    log::debug!("Read config {:#?}", config);

    Ok(config)
}
