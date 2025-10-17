/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::iter;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use foldhash::HashSet;
use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeSeed;
use serde::de::Error as _;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;
use serde::de::value::SeqAccessDeserializer;
use serde::de::value::StringDeserializer;
use strum::IntoEnumIterator as _;
use walkdir::WalkDir;

use crate::buck::RuleRef;
use crate::cargo::Manifest;
use crate::fixups::buildscript::BuildscriptFixups;
use crate::fixups::buildscript::CxxLibraryFixup;
use crate::fixups::buildscript::ExportedHeaders;
use crate::fixups::buildscript::PrebuiltCxxLibraryFixup;
use crate::glob::TrackedGlobSet;
use crate::path::relative_path;
use crate::platform::PlatformExpr;
use crate::unused::UnusedFixups;

/// Top-level fixup config file (correspondins to a fixups.toml)
#[derive(Debug, Default)]
pub struct FixupConfigFile {
    /// Directory that this config was deserialized from.
    pub fixup_dir: PathBuf,
    /// Source text that this config was deserialized from.
    toml: String,

    /// Common config
    pub base: FixupConfig,

    /// Platform-specific configs
    pub platform_fixup: Vec<FixupConfig>,
}

impl FixupConfigFile {
    pub(super) fn load(
        fixup_dir: PathBuf,
        package: &Manifest,
        public: bool,
    ) -> anyhow::Result<Self> {
        let fixup_path = fixup_dir.join("fixups.toml");

        let Ok(content) = fs::read_to_string(&fixup_path) else {
            log::debug!("no fixups at {}", fixup_path.display());
            return Ok(FixupConfigFile {
                fixup_dir,
                ..FixupConfigFile::default()
            });
        };

        log::debug!("read fixups from {}", fixup_path.display());
        let visitor = FixupConfigFileVisitor {
            fixup_dir,
            toml: content.clone(),
        };
        let fixup_config = toml::Deserializer::parse(&content)
            .and_then(|de| de.deserialize_map(visitor))
            .with_context(|| format!("Failed to parse {}", fixup_path.display()))?;

        for fixup in iter::once(&fixup_config.base).chain(&fixup_config.platform_fixup) {
            if fixup.visibility.is_some() && !public {
                return Err(anyhow::Error::msg(
                    "only public packages can have a fixup `visibility`.",
                )
                .context(format!("package {package} is private.")));
            }

            match fixup {
                FixupConfig {
                    // serde(skip)
                    platform: _,
                    used: _,
                    // Fields that NOT allowed to be platform-specific:
                    visibility: None,
                    omit_targets: None,
                    precise_srcs: None,
                    python_ext: None,
                    export_sources: None,
                    compatible_with: None,
                    target_compatible_with: None,
                    // Fields that are allowed to be platform-specific:
                    extra_srcs: _,
                    omit_srcs: _,
                    rustc_flags: _,
                    rustc_flags_select: _,
                    cfgs: _,
                    features: _,
                    omit_features: _,
                    extra_deps: _,
                    omit_deps: _,
                    cargo_env: _,
                    overlay: _,
                    link_style: _,
                    preferred_linkage: _,
                    linker_flags: _,
                    env: _,
                    buildscript: _,
                    cxx_library: _,
                    prebuilt_cxx_library: _,
                    extra_mapped_srcs: _,
                } => {}
                _ => {
                    if fixup.platform.uses_cfg_other_than_version() {
                        return Err(anyhow::Error::msg(
                            "the following fixups are not allowed to be platform-specific: \
                            visibility, \
                            omit_targets, \
                            precise_srcs, \
                            python_ext, \
                            export_sources, \
                            compatible_with, \
                            target_compatible_with",
                        ));
                    }
                }
            }
        }

        Ok(fixup_config)
    }

    pub fn collect_unused(&self, pkg: &str, unused: &mut UnusedFixups) {
        for fixup in iter::once(&self.base).chain(&self.platform_fixup) {
            if let Some(span) = &fixup.buildscript.span {
                if fixup.used.load(Ordering::Relaxed)
                    && !fixup.buildscript.used.load(Ordering::Relaxed)
                {
                    unused.buildscripts.insert(
                        (pkg.to_owned(), span.start),
                        self.toml[..span.start].split('\n').count(),
                    );
                }
            }
        }

        let mut collect =
            |globset: &TrackedGlobSet| globset.collect_unused_globs(unused, pkg, &self.toml);

        for fixup in iter::once(&self.base).chain(&self.platform_fixup) {
            if fixup.used.load(Ordering::Relaxed) {
                if let Some(export_sources) = &fixup.export_sources {
                    collect(&export_sources.srcs);
                    collect(&export_sources.exclude)
                }
                collect(&fixup.extra_srcs);
                collect(&fixup.omit_srcs);
                for cxx_library in &fixup.cxx_library {
                    collect(&cxx_library.srcs);
                    collect(&cxx_library.headers);
                    if let ExportedHeaders::Set(exported_headers) = &cxx_library.exported_headers {
                        collect(exported_headers);
                    }
                    collect(&cxx_library.exclude);
                }
                for prebuilt_cxx_library in &fixup.prebuilt_cxx_library {
                    collect(&prebuilt_cxx_library.static_libs);
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportSources {
    /// Suffix for the rule name
    pub name: String,
    /// Src globs rooted in manifest dir for package
    pub srcs: TrackedGlobSet,
    /// Globs to exclude from srcs, rooted in manifest dir for package
    #[serde(default)]
    pub exclude: TrackedGlobSet,
    /// Visibility for the rule
    pub visibility: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FixupConfig {
    /// Cfg expression determining what platforms this fixup applies to.
    #[serde(skip)]
    pub platform: PlatformExpr,
    /// Whether there exists any crate version that this fixup has been applied to.
    #[serde(skip)]
    pub used: AtomicBool,

    /// Limit an exposed crate's `alias`'s `visibility` to this.
    /// This only has an effect for top-level crates. Exposed crates
    /// by default get `visibility = ["PUBLIC"]`. Sometimes you want to
    /// discourage use of some crate by limiting its visibility.
    pub visibility: Option<Vec<String>>,

    /// Omit a target
    pub omit_targets: Option<BTreeSet<String>>,

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

    /// Platform compatibility. The same value will apply to the library and all
    /// binary targets contained in the same package.
    pub compatible_with: Option<Vec<RuleRef>>,
    pub target_compatible_with: Option<Vec<RuleRef>>,

    /// Extra src globs, rooted in manifest dir for package
    #[serde(default)]
    pub extra_srcs: TrackedGlobSet,
    /// Globs to exclude from srcs, rooted in manifest dir for package
    #[serde(default)]
    pub omit_srcs: TrackedGlobSet,
    /// Extra flags for rustc
    #[serde(default)]
    pub rustc_flags: Vec<String>,
    /// Select logic for rustc_flags
    #[serde(default)]
    pub rustc_flags_select: Vec<BTreeMap<String, Vec<String>>>,
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
    pub cargo_env: Option<CargoEnvs>,
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
    pub fn overlay_files(&self, fixup_dir: &Path) -> HashSet<PathBuf> {
        match &self.overlay {
            Some(overlay) => {
                let overlay_dir = fixup_dir.join(overlay);
                WalkDir::new(&overlay_dir)
                    .into_iter()
                    .filter_map(|ent| ent.ok())
                    .filter(|ent| ent.path().is_file())
                    .map(|ent| relative_path(&overlay_dir, ent.path()))
                    .collect()
            }
            None => HashSet::default(),
        }
    }

    /// Returns set of files that are provided either by an overlay or by mapped
    /// srcs.
    pub fn overlay_and_mapped_files(&self, fixup_dir: &Path) -> HashSet<PathBuf> {
        let mut files = self.overlay_files(fixup_dir);
        files.extend(self.extra_mapped_srcs.values().map(PathBuf::clone));
        files
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
    Hash,
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

pub enum CargoEnvPurpose {
    BuildOnly,
    RunOnly,
    BuildAndRun,
}

impl CargoEnv {
    pub fn purpose(&self) -> CargoEnvPurpose {
        match self {
            // Set for compilation only, not build script execution
            CargoEnv::CARGO_CRATE_NAME => CargoEnvPurpose::BuildOnly,
            // For execution, controlled by prelude//rust/tools/buildscript_run.py
            CargoEnv::CARGO_MANIFEST_DIR => CargoEnvPurpose::BuildOnly,
            // Controlled by prelude//rust/cargo_buildscript.bzl
            CargoEnv::CARGO_PKG_NAME | CargoEnv::CARGO_PKG_VERSION => CargoEnvPurpose::BuildOnly,
            // Set for build script execution only, not compilation
            CargoEnv::CARGO_MANIFEST_LINKS => CargoEnvPurpose::RunOnly,
            // Set for both
            CargoEnv::CARGO_PKG_AUTHORS
            | CargoEnv::CARGO_PKG_DESCRIPTION
            | CargoEnv::CARGO_PKG_REPOSITORY
            | CargoEnv::CARGO_PKG_VERSION_MAJOR
            | CargoEnv::CARGO_PKG_VERSION_MINOR
            | CargoEnv::CARGO_PKG_VERSION_PATCH
            | CargoEnv::CARGO_PKG_VERSION_PRE => CargoEnvPurpose::BuildAndRun,
        }
    }
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
        Ok(CargoEnvs::Some(Deserialize::deserialize(de)?))
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

struct FixupConfigFileVisitor {
    fixup_dir: PathBuf,
    toml: String,
}

impl<'de> Visitor<'de> for FixupConfigFileVisitor {
    type Value = FixupConfigFile;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("struct FixupConfigFile")
    }

    fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        struct FixupsMap<M> {
            map: M,
            platform_fixup: Vec<FixupConfig>,
        }

        impl<'de, M> MapAccess<'de> for &mut FixupsMap<M>
        where
            M: MapAccess<'de>,
        {
            type Error = M::Error;

            fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
            where
                K: DeserializeSeed<'de>,
            {
                while let Some(field) = self.map.next_key::<String>()? {
                    if field.starts_with("cfg(") {
                        self.platform_fixup.push(FixupConfig {
                            platform: PlatformExpr::parse(&field).map_err(M::Error::custom)?,
                            ..self.map.next_value()?
                        });
                    } else {
                        return seed.deserialize(StringDeserializer::new(field)).map(Some);
                    }
                }
                Ok(None)
            }

            fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
            where
                V: DeserializeSeed<'de>,
            {
                self.map.next_value_seed(seed)
            }
        }

        let mut fields = FixupsMap {
            map,
            platform_fixup: Vec::new(),
        };

        let de = MapAccessDeserializer::new(&mut fields);
        let base = FixupConfig::deserialize(de)?;

        Ok(FixupConfigFile {
            fixup_dir: self.fixup_dir,
            toml: self.toml,
            base,
            platform_fixup: fields.platform_fixup,
        })
    }
}
