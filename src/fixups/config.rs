/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::SeqAccessDeserializer;
use strum::IntoEnumIterator as _;
use walkdir::WalkDir;

use crate::buckify::relative_path;
use crate::cargo::ManifestTarget;
use crate::fixups::buildscript::BuildscriptFixup;
use crate::fixups::buildscript::BuildscriptFixups;
use crate::glob::SerializableGlobSet as GlobSet;
use crate::platform::PlatformExpr;

/// Top-level fixup config file (correspondins to a fixups.toml)
#[derive(Debug, Deserialize, Default, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixupConfigFile {
    /// Limit an exposed crate's `alias`'s `visibility` to this.
    /// This only has an effect for top-level crates. Exposed crates
    /// by default get `visibility = ["PUBLIC"]`. Sometimes you want to
    /// discourage use of some crate by limiting its visibility.
    #[serde(default, rename = "visibility")]
    pub custom_visibility: Option<Vec<String>>,

    /// Omit a target
    #[serde(default)]
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
    #[serde(flatten)]
    base: FixupConfig,

    /// Platform-specific configs
    #[serde(default)]
    platform_fixup: BTreeMap<PlatformExpr, FixupConfig>,
}

impl FixupConfigFile {
    /// Generate a template for a fixup.toml as a starting point.
    pub fn template(third_party_path: &Path, target: &ManifestTarget) -> Self {
        if !target.kind_custom_build() {
            return Default::default();
        }

        let relpath = relative_path(third_party_path, &target.src_path);
        let buildscript = vec![BuildscriptFixup::Unresolved(format!(
            "Unresolved build script at {}.",
            relpath.display()
        ))];

        FixupConfigFile {
            base: FixupConfig {
                buildscript: BuildscriptFixups(buildscript),
                ..Default::default()
            },
            ..Default::default()
        }
    }

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

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Default, Serialize)]
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
        self.version.as_ref().map_or(true, |req| req.matches(ver))
    }
}

/// `cargo_env` selection.
///
/// Deserializes from `true`, `false` or `["CARGO_MANIFEST_DIR", ...]`.
#[derive(Debug, Default)]
pub enum CargoEnvs {
    All,
    #[default]
    None,
    Some(BTreeSet<CargoEnv>),
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
    Serialize,
    strum::Display,
    strum::EnumIter
)]
#[allow(non_camel_case_types)]
pub enum CargoEnv {
    CARGO_MANIFEST_DIR,
    CARGO_PKG_AUTHORS,
    CARGO_PKG_DESCRIPTION,
    CARGO_PKG_NAME,
    CARGO_PKG_REPOSITORY,
    CARGO_PKG_VERSION,
    CARGO_PKG_VERSION_MAJOR,
    CARGO_PKG_VERSION_MINOR,
    CARGO_PKG_VERSION_PATCH,
}

impl CargoEnvs {
    pub fn iter(&self) -> Box<dyn Iterator<Item = CargoEnv> + '_> {
        match self {
            CargoEnvs::All => Box::new(CargoEnv::iter()),
            CargoEnvs::None => Box::new(std::iter::empty()),
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
            Ok(CargoEnvs::None)
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

impl Serialize for CargoEnvs {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            CargoEnvs::All => true.serialize(ser),
            CargoEnvs::None => false.serialize(ser),
            CargoEnvs::Some(envs) => envs.serialize(ser),
        }
    }
}
