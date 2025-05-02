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
use serde::de::Error as _;
use serde::de::MapAccess;
use serde::de::Visitor;

use crate::cargo::TargetKind;
use crate::collection::SetOrMap;

#[derive(Debug)]
pub struct BuildscriptFixups {
    pub build: BuildscriptBuild,
    pub run: Option<BuildscriptRun>,
    // False whenever this BuildscriptFixups has been deserialized from a
    // `[buildscript.run]` section or `buildscript.run = true` key in a
    // fixups.toml file. True whenever this BuildscriptFixups was initialized by
    // omission of buildscript key in a fixups.toml, or there was not even a
    // fixups.toml.
    pub defaulted_to_empty: bool,
}

impl Default for BuildscriptFixups {
    fn default() -> Self {
        BuildscriptFixups {
            build: BuildscriptBuild::default(),
            run: None,
            defaulted_to_empty: true,
        }
    }
}

#[derive(Default, Debug, Clone, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuildscriptBuild {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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

struct BuildscriptFixupsVisitor;

impl<'de> Visitor<'de> for BuildscriptFixupsVisitor {
    type Value = BuildscriptFixups;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a table [buildscript.run]")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut build = None;
        let mut run = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "build" => {
                    if build.is_some() {
                        return Err(M::Error::duplicate_field("build"));
                    }
                    build = Some(map.next_value()?);
                }
                "run" => {
                    if run.is_some() {
                        return Err(M::Error::duplicate_field("run"));
                    }
                    run = map.next_value_seed(BuildscriptRunVisitor)?;
                }
                _ => return Err(M::Error::unknown_field(&key, &["build", "run"])),
            }
        }

        Ok(BuildscriptFixups {
            build: build.unwrap_or_else(BuildscriptBuild::default),
            run,
            defaulted_to_empty: false,
        })
    }
}

impl<'de> Deserialize<'de> for BuildscriptFixups {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BuildscriptFixupsVisitor)
    }
}
