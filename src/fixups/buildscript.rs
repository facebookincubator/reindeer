/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Deserializer;
use serde::de::DeserializeSeed;
use serde::de::Error as DeError;
use serde::de::Expected;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Unexpected;
use serde::de::Visitor;

use crate::cargo::TargetKind;
use crate::collection::SetOrMap;

#[derive(Debug)]
pub struct BuildscriptFixups {
    pub run: Option<BuildscriptRun>,
    pub libs: Vec<BuildscriptLib>,
    // True whenever this BuildscriptFixups has been deserialized from a
    // `[buildscript...]` section or `buildscript = []` key in a fixups.toml
    // file. False whenever this BuildscriptFixups was initialized by omission
    // of buildscript key in a fixups.toml, or there was not even a fixups.toml.
    pub defaulted_to_empty: bool,
}

impl Default for BuildscriptFixups {
    fn default() -> Self {
        BuildscriptFixups {
            run: None,
            libs: Vec::new(),
            defaulted_to_empty: true,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum BuildscriptLib {
    /// Generate a C++ library rule
    CxxLibrary(CxxLibraryFixup),
    /// Generate a prebuilt C++ library rule
    PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup),
}

impl BuildscriptLib {
    pub fn targets(&self) -> &[(TargetKind, Option<String>)] {
        match self {
            BuildscriptLib::CxxLibrary(CxxLibraryFixup { targets, .. }) => targets,
            BuildscriptLib::PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup { targets, .. }) => targets,
        }
    }
}

/// Run the buildscript and extract rustc command line flags + generated sources.
///
/// Linker `-l`/`-L` flags are ignored so in practice the flags are just for `--cfg`.
#[derive(Default, Debug, Clone, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuildscriptRun {
    // Runtime environment for the build script. Not provided to build script
    // compilation.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl BuildscriptRun {
    fn legacy_merge(&mut self, second: Self) {
        let Self { env } = second;
        self.env.extend(env);
    }
}

fn set_true() -> bool {
    true
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CxxLibraryFixup {
    pub name: String,      // rule basename
    pub srcs: Vec<String>, // src globs
    // Which targets are we a dependency for. List in the form
    // of kind and name (eg `["bin","cargo"]`). Empty means apply to main lib target.
    #[serde(default)]
    pub targets: Vec<(TargetKind, Option<String>)>,
    #[serde(default)]
    pub headers: Vec<String>, // header globs
    #[serde(default)]
    pub exported_headers: SetOrMap<String>, // exported header globs
    #[serde(default = "set_true")]
    pub add_dep: bool, // add to dependencies
    #[serde(default)]
    pub public: bool, // make public
    #[serde(default)]
    pub include_paths: Vec<PathBuf>,
    #[serde(default)]
    pub fixup_include_paths: Vec<PathBuf>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub compiler_flags: Vec<String>,
    #[serde(default)]
    pub preprocessor_flags: Vec<String>,
    pub header_namespace: Option<String>,
    #[serde(default)]
    pub deps: Vec<String>,
    #[serde(default)]
    pub compatible_with: Vec<String>,
    /// Cxx library preferred linkage (how dependents should link you)
    pub preferred_linkage: Option<String>,
    /// Whether to allow undefined symbols during compilation (e.g. when a rust library
    /// and cxx library depend on each other for symbols)
    #[serde(default)]
    pub undefined_symbols: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrebuiltCxxLibraryFixup {
    pub name: String,             // rule basename
    pub static_libs: Vec<String>, // static lib globs
    #[serde(default = "set_true")]
    pub add_dep: bool, // add to dependencies
    // Which targets are we a dependency for. List in the form
    // of kind and name (eg `["bin","cargo"]`). Empty means apply to main lib target.
    #[serde(default)]
    pub targets: Vec<(TargetKind, Option<String>)>,
    #[serde(default)]
    pub public: bool, // make public
    #[serde(default)]
    pub compatible_with: Vec<String>,
}

enum LegacyBuildscriptRunOrLib {
    Run(BuildscriptRun),
    Lib(BuildscriptLib),
}

struct LegacyBuildscriptRunOrLibVisitor;

impl<'de> Visitor<'de> for LegacyBuildscriptRunOrLibVisitor {
    type Value = LegacyBuildscriptRunOrLib;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("[buildscript.gen_srcs] or [buildscript.rustc_flags] or [buildscript.cxx_library] or [buildscript.prebuilt_cxx_library]")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let Some(key) = map.next_key::<String>()? else {
            return Err(M::Error::custom(format!(
                "unexpected empty table, expected {}",
                &self as &dyn Expected,
            )));
        };

        let run_or_lib = match key.as_str() {
            "gen_srcs" | "rustc_flags" => LegacyBuildscriptRunOrLib::Run(map.next_value()?),
            "cxx_library" => {
                LegacyBuildscriptRunOrLib::Lib(BuildscriptLib::CxxLibrary(map.next_value()?))
            }
            "prebuilt_cxx_library" => LegacyBuildscriptRunOrLib::Lib(
                BuildscriptLib::PrebuiltCxxLibrary(map.next_value()?),
            ),
            _ => return Err(M::Error::invalid_value(Unexpected::Str(&key), &self)),
        };

        if let Some(key) = map.next_key::<String>()? {
            return Err(M::Error::custom(format!(
                "unexpected second value {key:?} in the same buildscript table, expected {}",
                &self as &dyn Expected,
            )));
        }

        Ok(run_or_lib)
    }
}

impl<'de> Deserialize<'de> for LegacyBuildscriptRunOrLib {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(LegacyBuildscriptRunOrLibVisitor)
    }
}

struct BuildscriptRunVisitor;

impl<'de> Visitor<'de> for BuildscriptRunVisitor {
    type Value = Option<BuildscriptRun>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("[buildscript.run] or `buildscript.run = true`")
    }

    fn visit_bool<E>(self, boolean: bool) -> Result<Self::Value, E> {
        Ok(boolean.then(BuildscriptRun::default))
    }

    fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let de = serde::de::value::MapAccessDeserializer::new(map);
        BuildscriptRun::deserialize(de).map(Some)
    }
}

impl<'de> DeserializeSeed<'de> for BuildscriptRunVisitor {
    type Value = Option<BuildscriptRun>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

struct BuildscriptLibVisitor;

impl<'de> Visitor<'de> for BuildscriptLibVisitor {
    type Value = BuildscriptLib;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("[[buildscript.cxx_library]] or [[buildscript.prebuilt_cxx_library]]")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let Some(key) = map.next_key::<String>()? else {
            return Err(M::Error::custom(format!(
                "unexpected empty table, expected {}",
                &self as &dyn Expected,
            )));
        };

        let run_or_lib = match key.as_str() {
            "cxx_library" => BuildscriptLib::CxxLibrary(map.next_value()?),
            "prebuilt_cxx_library" => BuildscriptLib::PrebuiltCxxLibrary(map.next_value()?),
            _ => return Err(M::Error::invalid_value(Unexpected::Str(&key), &self)),
        };

        if let Some(key) = map.next_key::<String>()? {
            return Err(M::Error::custom(format!(
                "unexpected second value {key:?} in the same buildscript table, expected {}",
                &self as &dyn Expected,
            )));
        }

        Ok(run_or_lib)
    }
}

impl<'de> Deserialize<'de> for BuildscriptLib {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BuildscriptLibVisitor)
    }
}

struct BuildscriptFixupsVisitor;

impl<'de> Visitor<'de> for BuildscriptFixupsVisitor {
    type Value = BuildscriptFixups;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a TOML array: [[buildscript]], or a TOML table: [buildscript.run] or [[buildscript.cxx_library]]")
    }

    // Legacy representation. To be deleted.
    //
    //     buildscript = []
    //
    //     [[buildscript]]
    //     [buildscript.rustc_flags]
    //
    //     [[buildscript]]
    //     [buildscript.gen_srcs]
    //
    //     [[buildscript]]
    //     [buildscript.cxx_library]
    //
    //     [[buildscript]]
    //     [buildscript.prebuilt_cxx_library]
    //
    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut run = None;
        let mut libs = Vec::new();

        while let Some(fixup) = seq.next_element()? {
            match fixup {
                LegacyBuildscriptRunOrLib::Run(legacy_run) => {
                    run.get_or_insert_with(BuildscriptRun::default)
                        .legacy_merge(legacy_run);
                }
                LegacyBuildscriptRunOrLib::Lib(lib) => libs.push(lib),
            }
        }

        Ok(BuildscriptFixups {
            run,
            libs,
            defaulted_to_empty: false,
        })
    }

    // Preferred representation.
    //
    //     [buildscript.run]
    //
    //     [[buildscript.cxx_library]]
    //
    //     [[buildscript.prebuilt_cxx_library]]
    //
    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut run = None;
        let mut libs = Vec::new();

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "run" => {
                    if run.is_some() {
                        return Err(M::Error::duplicate_field("run"));
                    }
                    run = map.next_value_seed(BuildscriptRunVisitor)?;
                }
                "cxx_library" => {
                    let entries: Vec<CxxLibraryFixup> = map.next_value()?;
                    libs.extend(entries.into_iter().map(BuildscriptLib::CxxLibrary));
                }
                "prebuilt_cxx_library" => {
                    let entries: Vec<PrebuiltCxxLibraryFixup> = map.next_value()?;
                    libs.extend(entries.into_iter().map(BuildscriptLib::PrebuiltCxxLibrary));
                }
                _ => return Err(M::Error::invalid_value(Unexpected::Str(&key), &self)),
            }
        }

        Ok(BuildscriptFixups {
            run,
            libs,
            defaulted_to_empty: false,
        })
    }
}

impl<'de> Deserialize<'de> for BuildscriptFixups {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BuildscriptFixupsVisitor)
    }
}
