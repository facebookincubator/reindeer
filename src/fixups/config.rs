/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::Error as _;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::SeqAccessDeserializer;
use strum::IntoEnumIterator as _;
use walkdir::WalkDir;

use crate::buckify::relative_path;
use crate::fixups::buildscript::BuildscriptFixups;
use crate::fixups::buildscript::CxxLibraryFixup;
use crate::fixups::buildscript::PrebuiltCxxLibraryFixup;
use crate::glob::SerializableGlobSet as GlobSet;
use crate::platform::PlatformExpr;

/// Top-level fixup config file (correspondins to a fixups.toml)
#[derive(Debug, Default)]
pub struct FixupConfigFile {
    /// Limit an exposed crate's `alias`'s `visibility` to this.
    /// This only has an effect for top-level crates. Exposed crates
    /// by default get `visibility = ["PUBLIC"]`. Sometimes you want to
    /// discourage use of some crate by limiting its visibility.
    pub custom_visibility: Option<CustomVisibility>,

    /// Omit a target
    pub omit_targets: BTreeSet<String>,

    /// Skip precise srcs detection and fallback to `**/*.rs`.
    /// Overrides the global config `precise_srcs` for this crate.
    /// This is useful for pathologically large crates where
    /// src detection dominates buckification (e.g. the `windows` crate).
    pub precise_srcs: Option<bool>,

    /// If the crate is generating a cdylib which is intended to be
    /// a Python extension module, set this to give the module name.
    /// This is passed as a `python_ext` parameter on the `rust_library`
    /// rule so it can be mapped to the right underlying rule.
    pub python_ext: Option<String>,

    /// Make the crate sources available through a `filegroup`.
    /// This is useful for manually handling build scripts.
    pub export_sources: Option<ExportSources>,

    /// Common config
    pub base: FixupConfig,

    /// Platform-specific configs
    pub platform_fixup: BTreeMap<PlatformExpr, FixupConfig>,
}

impl FixupConfigFile {
    pub fn base(&self, version: &semver::Version) -> Option<&FixupConfig> {
        if self.base.version_applies(version) {
            Some(&self.base)
        } else {
            None
        }
    }

    pub fn platform_configs<'a>(
        &'a self,
        version: &'a semver::Version,
    ) -> impl Iterator<Item = (&'a PlatformExpr, &'a FixupConfig)> + 'a {
        self.platform_fixup
            .iter()
            .filter(move |(_, cfg)| cfg.version_applies(version))
    }

    pub fn configs<'a>(
        &'a self,
        version: &'a semver::Version,
    ) -> impl Iterator<Item = (Option<&'a PlatformExpr>, &'a FixupConfig)> + 'a {
        self.base(version)
            .into_iter()
            .map(|base| (None, base))
            .chain(
                self.platform_configs(version)
                    .map(|(plat, cfg)| (Some(plat), cfg)),
            )
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSources {
    /// Suffix for the rule name
    pub name: String,
    /// Src globs rooted in manifest dir for package
    pub srcs: Vec<String>,
    /// Globs to exclude from srcs, rooted in manifest dir for package
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Visibility for the rule
    pub visibility: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FixupConfig {
    /// Versions this fixup applies to,
    pub version: Option<semver::VersionReq>,
    /// Extra src globs, rooted in manifest dir for package
    #[serde(default)]
    pub extra_srcs: Vec<String>,
    /// Globs to exclude from srcs, rooted in manifest dir for package
    #[serde(default)]
    pub omit_srcs: GlobSet,
    /// Extra flags for rustc
    #[serde(default)]
    pub rustc_flags: Vec<String>,
    /// Select logic for rustc_flags
    #[serde(default)]
    pub rustc_flags_select: BTreeMap<String, Vec<String>>,
    /// Extra configs
    #[serde(default)]
    pub cfgs: BTreeSet<String>,
    /// Extra features
    #[serde(default)]
    pub features: BTreeSet<String>,
    /// Features to forcably omit. This doesn't change dependency
    /// resolution, just what the targets are compiled with.
    #[serde(default)]
    pub omit_features: BTreeSet<String>,
    /// Additional Buck dependencies
    #[serde(default)]
    pub extra_deps: BTreeSet<String>,
    /// Omit Cargo dependencies - just bare crate name
    #[serde(default)]
    pub omit_deps: BTreeSet<String>,
    /// Add Cargo environment.
    /// `true` means add all Cargo environment variables.
    /// `false` means add none.
    /// A list of environment variables names adds only those.
    #[serde(default)]
    pub cargo_env: CargoEnvs,
    /// Path relative to fixups_dir with overlay filesystem
    /// Files in overlay logically add to or replace files in
    /// manifest dir, and therefore have the same directory
    /// structure.
    pub overlay: Option<PathBuf>,
    /// Rust binary link style (how dependencies should be linked)
    pub link_style: Option<String>,
    /// Rust library preferred linkage (how dependents should link you)
    pub preferred_linkage: Option<String>,
    /// Extra flags for linker
    #[serde(default)]
    pub linker_flags: Vec<String>,

    // Table/map-like values must come after everything else
    /// Additional env variables
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How to handle a build-script, if present
    #[serde(default)]
    pub buildscript: BuildscriptFixups,
    // Compile and link some C++ source code
    #[serde(default)]
    pub cxx_library: Vec<CxxLibraryFixup>,
    // Link .o or .obj or .lib file
    #[serde(default)]
    pub prebuilt_cxx_library: Vec<PrebuiltCxxLibraryFixup>,
    /// Extra mapped srcs
    #[serde(default)]
    pub extra_mapped_srcs: BTreeMap<String, PathBuf>,
}

impl FixupConfig {
    /// Return set of overlay files, relative to the overlay dir (and therefore
    /// relative to manifest dir).
    pub fn overlay_files(&self, fixup_dir: &Path) -> anyhow::Result<HashSet<PathBuf>> {
        let files = match &self.overlay {
            Some(overlay) => {
                let overlay_dir = fixup_dir.join(overlay);
                WalkDir::new(&overlay_dir)
                    .into_iter()
                    .filter_map(|ent| ent.ok())
                    .filter(|ent| ent.path().is_file())
                    .map(|ent| relative_path(&overlay_dir, ent.path()))
                    .collect()
            }
            None => HashSet::new(),
        };

        Ok(files)
    }

    /// Returns set of files that are provided either by an overlay or by mapped
    /// srcs.
    pub fn overlay_and_mapped_files(&self, fixup_dir: &Path) -> anyhow::Result<HashSet<PathBuf>> {
        let mut files = self.overlay_files(fixup_dir)?;
        files.extend(self.extra_mapped_srcs.values().map(PathBuf::clone));
        Ok(files)
    }

    /// Return true if config applies to given version
    pub fn version_applies(&self, ver: &semver::Version) -> bool {
        self.version.as_ref().is_none_or(|req| req.matches(ver))
    }
}

/// `cargo_env` selection.
///
/// Deserializes from `true`, `false` or `["CARGO_MANIFEST_DIR", ...]`.
#[derive(Debug, Clone)]
pub enum CargoEnvs {
    All,
    Some(BTreeSet<CargoEnv>),
}

impl Default for CargoEnvs {
    fn default() -> Self {
        CargoEnvs::Some(BTreeSet::new())
    }
}

/// Supported Cargo environment variable names.
// https://github.com/rust-lang/cargo/blob/0.72.1/src/cargo/core/compiler/compilation.rs#L315-L350
#[derive(
    Debug,
    Clone,
    Copy,
    PartialOrd,
    Ord,
    PartialEq,
    Eq,
    Deserialize,
    strum::Display,
    strum::EnumIter
)]
#[allow(non_camel_case_types)]
pub enum CargoEnv {
    CARGO_CRATE_NAME,
    CARGO_MANIFEST_DIR,
    CARGO_MANIFEST_LINKS,
    CARGO_PKG_AUTHORS,
    CARGO_PKG_DESCRIPTION,
    CARGO_PKG_NAME,
    CARGO_PKG_REPOSITORY,
    CARGO_PKG_VERSION,
    CARGO_PKG_VERSION_MAJOR,
    CARGO_PKG_VERSION_MINOR,
    CARGO_PKG_VERSION_PATCH,
    CARGO_PKG_VERSION_PRE,
}

impl CargoEnvs {
    pub fn iter(&self) -> Box<dyn Iterator<Item = CargoEnv> + '_> {
        match self {
            CargoEnvs::All => Box::new(CargoEnv::iter()),
            CargoEnvs::Some(envs) => Box::new(envs.iter().cloned()),
        }
    }
}

struct CargoEnvsVisitor;

impl<'de> Visitor<'de> for CargoEnvsVisitor {
    type Value = CargoEnvs;

    fn expecting(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("bool or list containing any of ")?;
        let mut envs = CargoEnv::iter();
        if let Some(first) = envs.next() {
            write!(fmt, "`{}`", first)?;
            for env in envs {
                write!(fmt, ", `{}`", env)?;
            }
        }
        Ok(())
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value {
            Ok(CargoEnvs::All)
        } else {
            Ok(CargoEnvs::Some(BTreeSet::new()))
        }
    }

    fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let de = SeqAccessDeserializer::new(seq);
        Ok(CargoEnvs::Some(<_>::deserialize(de)?))
    }
}

impl<'de> Deserialize<'de> for CargoEnvs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(CargoEnvsVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum CustomVisibility {
    NoVersion(Vec<String>),
    WithVersion(HashMap<semver::VersionReq, Vec<String>>),
}

struct FixupConfigFileVisitor;

impl<'de> Visitor<'de> for FixupConfigFileVisitor {
    type Value = FixupConfigFile;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("struct FixupConfigFile")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut custom_visibility = None;
        let mut omit_targets = None;
        let mut precise_srcs = None;
        let mut python_ext = None;
        let mut export_sources = None;
        let mut base = serde_json::Map::new();
        let mut platform_fixup = BTreeMap::new();

        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "visibility" => {
                    if custom_visibility.is_some() {
                        return Err(M::Error::duplicate_field("visibility"));
                    }
                    custom_visibility = Some(map.next_value()?);
                }
                "omit_targets" => {
                    if omit_targets.is_some() {
                        return Err(M::Error::duplicate_field("omit_targets"));
                    }
                    omit_targets = Some(map.next_value()?);
                }
                "precise_srcs" => {
                    if precise_srcs.is_some() {
                        return Err(M::Error::duplicate_field("precise_srcs"));
                    }
                    precise_srcs = Some(map.next_value()?);
                }
                "python_ext" => {
                    if python_ext.is_some() {
                        return Err(M::Error::duplicate_field("python_ext"));
                    }
                    python_ext = Some(map.next_value()?);
                }
                "export_sources" => {
                    if export_sources.is_some() {
                        return Err(M::Error::duplicate_field("export_sources"));
                    }
                    export_sources = Some(map.next_value()?);
                }
                _ => {
                    if field.starts_with("cfg(") {
                        let platform_expr = PlatformExpr::from(field);
                        let fixup_config: FixupConfig = map.next_value()?;
                        platform_fixup.insert(platform_expr, fixup_config);
                    } else {
                        base.insert(field, map.next_value()?);
                    }
                }
            }
        }

        Ok(FixupConfigFile {
            custom_visibility,
            omit_targets: omit_targets.unwrap_or_else(BTreeSet::new),
            precise_srcs,
            python_ext,
            export_sources,
            base: FixupConfig::deserialize(base).map_err(M::Error::custom)?,
            platform_fixup,
        })
    }
}

impl<'de> Deserialize<'de> for FixupConfigFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(FixupConfigFileVisitor)
    }
}
