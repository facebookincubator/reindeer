/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as DeError;
use serde::de::MapAccess;
use serde::de::Visitor;
use serde::ser::SerializeMap;

use crate::cargo::TargetKind;
use crate::collection::SetOrMap;

#[derive(Deserialize, Debug, Serialize)]
pub struct BuildscriptFixups(pub Vec<BuildscriptFixup>);

impl<'a> IntoIterator for &'a BuildscriptFixups {
    type Item = &'a BuildscriptFixup;
    type IntoIter = std::slice::Iter<'a, BuildscriptFixup>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Default for BuildscriptFixups {
    fn default() -> Self {
        let unresolved = BuildscriptFixup::Unresolved("No build script fixups defined".to_string());
        BuildscriptFixups(vec![unresolved])
    }
}

impl Deref for BuildscriptFixups {
    type Target = [BuildscriptFixup];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum BuildscriptFixup {
    /// Unresolved build script (string with helpful message)
    Unresolved(String),
    /// Run the buildscript and extract command line args. Linker -l/-L args ignored so in
    /// practice this is just --cfg options.
    RustcFlags(RustcFlags),
    /// Generated sources - give list of generated paths which are mapped into target sources
    GenSrcs(GenSrcs),
    /// Generate a C++ library rule
    CxxLibrary(CxxLibraryFixup),
    /// Generate a prebuilt C++ library rule
    PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup),
}

impl BuildscriptFixup {
    pub fn targets(&self) -> Option<&[(TargetKind, Option<String>)]> {
        let targets = match self {
            BuildscriptFixup::RustcFlags(RustcFlags { targets, .. }) => targets,
            BuildscriptFixup::GenSrcs(GenSrcs { targets, .. }) => targets,
            BuildscriptFixup::CxxLibrary(CxxLibraryFixup { targets, .. }) => targets,
            BuildscriptFixup::PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup { targets, .. }) => {
                targets
            }
            BuildscriptFixup::Unresolved(_) => return None,
        };

        Some(&targets[..])
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RustcFlags {
    // Which targets are we generating flags for. List in the form
    // of kind and name (eg `["bin","cargo"]`). Empty means apply to main lib target.
    #[serde(default)]
    pub targets: Vec<(TargetKind, Option<String>)>,
    // Runtime environment for the gensrc program
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GenSrcs {
    // Which targets are we generating source for. List in the form
    // of kind and name (eg `["bin","cargo"]`). Empty means apply to main lib target.
    #[serde(default)]
    pub targets: Vec<(TargetKind, Option<String>)>,
    // Runtime environment for the gensrc program
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

fn set_true() -> bool {
    true
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
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

#[derive(Deserialize)]
struct Empty {}

impl Serialize for BuildscriptFixup {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let len = 1;
        let mut map = serializer.serialize_map(Some(len))?;
        match self {
            BuildscriptFixup::Unresolved(msg) => map.serialize_entry("unresolved", msg)?,
            BuildscriptFixup::RustcFlags(rustc_flags) => {
                map.serialize_entry("rustc_flags", rustc_flags)?
            }
            BuildscriptFixup::GenSrcs(gen_srcs) => map.serialize_entry("gen_srcs", gen_srcs)?,
            BuildscriptFixup::CxxLibrary(cxxlib) => map.serialize_entry("cxx_library", cxxlib)?,
            BuildscriptFixup::PrebuiltCxxLibrary(prebuilt_lib) => {
                map.serialize_entry("prebuilt_prebcxx_library", prebuilt_lib)?
            }
        }
        map.end()
    }
}

struct BuildscriptFixupVisitor<'de>(PhantomData<&'de ()>);

impl<'de> Visitor<'de> for BuildscriptFixupVisitor<'de> {
    type Value = BuildscriptFixup;

    fn expecting(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "BuildscriptFixup enum")
    }

    fn visit_map<M>(self, mut access: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let res = if let Some(key) = access.next_key::<String>()? {
            let res = match key.as_str() {
                "unresolved" => BuildscriptFixup::Unresolved(access.next_value()?),
                "rustc_flags" => BuildscriptFixup::RustcFlags(access.next_value()?),
                "gen_srcs" => BuildscriptFixup::GenSrcs(access.next_value()?),
                "cxx_library" => BuildscriptFixup::CxxLibrary(access.next_value()?),
                "prebuilt_cxx_library" => {
                    BuildscriptFixup::PrebuiltCxxLibrary(access.next_value()?)
                }
                other => {
                    // other keys are unit, which map to an empty map
                    let _ = access.next_value::<Empty>()?;
                    self.visit_str(other)?
                }
            };
            Ok(res)
        } else {
            return Err(M::Error::custom("Empty BuildscriptFixup map"));
        };

        if let Some(key) = access.next_key::<String>()? {
            Err(M::Error::custom(format!(
                "Extra BuildscriptFixup map entry: {}",
                key
            )))
        } else {
            res
        }
    }
}

impl<'de> Deserialize<'de> for BuildscriptFixup {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(BuildscriptFixupVisitor(PhantomData))
    }
}
