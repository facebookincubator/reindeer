/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Definitions of Buck-related types
//!
//! Model Buck rules in a rough way. Can definitely be improved.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::io::Write;
use std::path::PathBuf;

use semver::Version;
use serde::Deserialize;
use serde::Serialize;
use serde::ser::SerializeMap;
use serde::ser::SerializeSeq;
use serde::ser::Serializer;
use serde_starlark::FunctionCall;

use crate::collection::Select;
use crate::collection::SetOrMap;
use crate::config::BuckConfig;
use crate::platform::PlatformName;
use crate::subtarget::Subtarget;

/// Only the name of a target. Does not include package path, nor leading colon.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct Name(pub String);

impl Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

impl Debug for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        // More compact than a derived Debug impl
        let Name(name) = self;
        write!(formatter, "Name({name:?})")
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuleRef {
    pub target: String,
}

impl From<Name> for RuleRef {
    fn from(name: Name) -> Self {
        RuleRef::new(format!(":{}", name))
    }
}

impl Ord for RuleRef {
    fn cmp(&self, other: &Self) -> Ordering {
        buildifier_cmp(&self.target, &other.target)
    }
}

impl PartialOrd for RuleRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl RuleRef {
    pub fn new(target: String) -> Self {
        RuleRef { target }
    }
}

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct BuckPath(pub PathBuf);

impl BuckPath {
    pub fn is_empty(&self) -> bool {
        self.0.as_os_str().is_empty()
    }
}

impl Debug for BuckPath {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // More compact than a derived Debug impl
        let BuckPath(path) = self;
        write!(f, "BuckPath({path:?})")
    }
}

impl Serialize for BuckPath {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        // Even on Windows we want to use forward slash paths
        match self.0.as_path().to_str() {
            Some(s) => s.replace('\\', "/").serialize(ser),
            None => Err(serde::ser::Error::custom(
                "path contains invalid UTF-8 characters",
            )),
        }
    }
}

impl Display for BuckPath {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0.to_string_lossy().replace('\\', "/"), formatter)
    }
}

impl Ord for BuckPath {
    fn cmp(&self, other: &Self) -> Ordering {
        let this = self.0.to_string_lossy();
        let other = other.0.to_string_lossy();
        buildifier_cmp(&this, &other)
    }
}

impl PartialOrd for BuckPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum StringOrPath {
    String(String),
    Path(BuckPath),
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(untagged)]
pub enum SubtargetOrPath {
    Subtarget(Subtarget),
    Path(BuckPath),
}

impl SubtargetOrPath {
    fn is_subtarget(&self) -> bool {
        matches!(self, SubtargetOrPath::Subtarget(_))
    }

    fn is_path(&self) -> bool {
        matches!(self, SubtargetOrPath::Path(_))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Visibility {
    Public,
    Private,
    Custom(Vec<String>),
}

impl Serialize for Visibility {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Visibility::Public => ["PUBLIC"].as_slice().serialize(ser),
            Visibility::Private => (&[] as &[&str]).serialize(ser),
            Visibility::Custom(custom_visiblity) => custom_visiblity.serialize(ser),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Alias {
    pub name: Name,
    /// Local target that the alias refers to -- always in the same package.
    pub actual: Name,
    pub platforms: Option<BTreeSet<PlatformName>>,
    pub visibility: Visibility,
}

impl Serialize for Alias {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            actual,
            platforms,
            visibility,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("actual", &NameAsLabel(actual))?;
        if let Some(platforms) = platforms {
            map.serialize_entry("platforms", platforms)?;
        }
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

struct NameAsLabel<'a>(&'a Name);

impl Serialize for NameAsLabel<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(&format_args!(":{}", self.0))
    }
}

#[derive(Debug, PartialEq)]
pub struct Filegroup {
    pub name: Name,
    pub srcs: BTreeMap<BuckPath, SubtargetOrPath>,
    pub visibility: Visibility,
}

impl Serialize for Filegroup {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            srcs,
            visibility,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        // Pretty useless to have a filegroup without srcs,
        // but that's legit in both buck1 and buck2.
        if !srcs.is_empty() {
            map.serialize_entry("srcs", srcs)?;
        }
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(PartialEq, Debug)]
pub struct HttpArchive {
    pub name: Name,
    pub sha256: String,
    pub strip_prefix: String,
    pub sub_targets: BTreeSet<BuckPath>,
    pub urls: Vec<String>,
    pub visibility: Visibility,
    pub sort_key: Name,
}

impl Serialize for HttpArchive {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            sha256,
            strip_prefix,
            sub_targets,
            urls,
            visibility,
            sort_key: _,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("sha256", sha256)?;
        map.serialize_entry("strip_prefix", strip_prefix)?;
        if !sub_targets.is_empty() {
            map.serialize_entry("sub_targets", sub_targets)?;
        }
        map.serialize_entry("urls", urls)?;
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(Debug, PartialEq)]
pub struct ExtractArchive {
    pub name: Name,
    pub src: BuckPath,
    pub strip_prefix: String,
    pub sub_targets: BTreeSet<BuckPath>,
    pub visibility: Visibility,
    pub sort_key: Name,
}

impl Serialize for ExtractArchive {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            src,
            strip_prefix,
            sub_targets,
            visibility,
            sort_key: _,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("src", src)?;
        map.serialize_entry("strip_prefix", strip_prefix)?;
        if !sub_targets.is_empty() {
            map.serialize_entry("sub_targets", sub_targets)?;
        }
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(Debug, PartialEq)]
pub struct GitFetch {
    pub name: Name,
    pub repo: String,
    pub rev: String,
    pub sub_targets: BTreeSet<BuckPath>,
    pub visibility: Visibility,
}

impl Serialize for GitFetch {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            repo,
            rev,
            sub_targets,
            visibility,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("repo", repo)?;
        map.serialize_entry("rev", rev)?;
        if !sub_targets.is_empty() {
            map.serialize_entry("sub_targets", sub_targets)?;
        }
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(Debug, PartialEq)]
pub struct Common {
    pub name: Name,
    pub visibility: Visibility,
    pub licenses: BTreeSet<BuckPath>,
    pub compatible_with: Vec<RuleRef>,
    pub target_compatible_with: Vec<RuleRef>,
}

// Rule attributes which could be platform-specific
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PlatformRustCommon {
    pub srcs: BTreeSet<BuckPath>,
    pub mapped_srcs: BTreeMap<SubtargetOrPath, BuckPath>,
    pub rustc_flags: Select<Vec<String>>,
    pub features: BTreeSet<String>,
    pub deps: BTreeSet<RuleRef>,
    pub named_deps: BTreeMap<String, RuleRef>,
    pub env: BTreeMap<String, StringOrPath>,

    // This isn't really "common" (Binaries only), but does need to be platform
    pub link_style: Option<String>,
    pub linker_flags: Vec<String>,

    pub preferred_linkage: Option<String>,
}

impl Serialize for PlatformRustCommon {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            srcs,
            mapped_srcs,
            rustc_flags,
            features,
            deps,
            named_deps,
            env,
            link_style,
            linker_flags,
            preferred_linkage,
        } = self;
        let mut map = ser.serialize_map(None)?;
        if !srcs.is_empty() {
            map.serialize_entry("srcs", srcs)?;
        }
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        if !features.is_empty() {
            map.serialize_entry("features", features)?;
        }
        if let Some(link_style) = link_style {
            map.serialize_entry("link_style", link_style)?;
        }
        if !linker_flags.is_empty() {
            map.serialize_entry("linker_flags", linker_flags)?;
        }
        if !mapped_srcs.is_empty() {
            map.serialize_entry("mapped_srcs", mapped_srcs)?;
        }
        if !named_deps.is_empty() {
            map.serialize_entry("named_deps", named_deps)?;
        }
        if let Some(preferred_linkage) = preferred_linkage {
            map.serialize_entry("preferred_linkage", preferred_linkage)?;
        }
        if !rustc_flags.is_empty() {
            map.serialize_entry("rustc_flags", rustc_flags)?;
        }
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug)]
pub struct RustCommon {
    pub common: Common,
    pub krate: String,
    pub crate_root: BuckPath,
    pub edition: crate::cargo::Edition,
    // Platform-dependent
    pub base: PlatformRustCommon,
    // Platform-specific
    pub platform: BTreeMap<PlatformName, PlatformRustCommon>,
}

/// Serialize as:
///
/// ```bzl
/// platforms = {
///     "linux-x86_64": dict(
///         srcs = [...],
///         preferred_linkage = "...",
///         deps = [...],
///     ),
/// }
/// ```
///
/// If we didn't do this, it would come out as follows instead, and `buildifier`
/// would refuse to sort the keys, or sort/normalize the contents of the srcs
/// and deps attributes.
///
/// ```bzl
/// platforms = {
///     "linux-x86_64": {
///         "srcs": [...],
///         "preferred_linkage": [...],
///         "deps": [...],
///     },
/// }
///
/// Even though we do not run `buildifier` anymore, this style is preferred
/// because we want to consistently write fields with buck meaning as keywords
/// (e.g. `field = value`) rather than as maps with arbitrary keys
/// (e.g. `"key": value`).
/// ```
fn serialize_platforms_dict<S, V>(
    map: &mut S,
    platforms: &BTreeMap<PlatformName, V>,
) -> Result<(), S::Error>
where
    S: SerializeMap,
    V: Serialize + Default + PartialEq,
{
    struct Platforms<'a, V>(&'a BTreeMap<PlatformName, V>);

    impl<V> Serialize for Platforms<'_, V>
    where
        V: Serialize + Default + PartialEq,
    {
        fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
            let mut map = ser.serialize_map(None)?;
            for (name, value) in self.0 {
                if *value != V::default() {
                    map.serialize_entry(name, &FunctionCall::new("dict", value))?;
                }
            }
            map.end()
        }
    }

    if platforms.values().any(|value| *value != V::default()) {
        map.serialize_entry("platform", &Platforms(platforms))?;
    }

    Ok(())
}

#[derive(Debug)]
pub struct RustLibrary {
    pub common: RustCommon,
    pub proc_macro: bool,
    pub dlopen_enable: bool,
    pub python_ext: Option<String>,
    pub linkable_alias: Option<String>,
}

impl Serialize for RustLibrary {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            common:
                RustCommon {
                    common:
                        Common {
                            name,
                            visibility,
                            licenses,
                            compatible_with,
                            target_compatible_with,
                        },
                    krate,
                    crate_root,
                    edition,
                    base:
                        PlatformRustCommon {
                            srcs,
                            mapped_srcs,
                            rustc_flags,
                            features,
                            deps,
                            named_deps,
                            env,
                            link_style,
                            linker_flags,
                            preferred_linkage,
                        },
                    platform,
                },
            proc_macro,
            dlopen_enable,
            python_ext,
            linkable_alias,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        if !srcs.is_empty() {
            map.serialize_entry("srcs", srcs)?;
        }
        if !compatible_with.is_empty() {
            map.serialize_entry("compatible_with", compatible_with)?;
        }
        map.serialize_entry("crate", krate)?;
        map.serialize_entry("crate_root", crate_root)?;
        if *dlopen_enable {
            map.serialize_entry("dlopen_enable", &true)?;
        }
        map.serialize_entry("edition", edition)?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        if !features.is_empty() {
            map.serialize_entry("features", features)?;
        }
        if !licenses.is_empty() {
            map.serialize_entry("licenses", licenses)?;
        }
        if let Some(link_style) = link_style {
            map.serialize_entry("link_style", link_style)?;
        }
        if !linker_flags.is_empty() {
            map.serialize_entry("linker_flags", linker_flags)?;
        }
        if let Some(linkable_alias) = linkable_alias {
            map.serialize_entry("linkable_alias", linkable_alias)?;
        }
        if !mapped_srcs.is_empty() {
            map.serialize_entry("mapped_srcs", mapped_srcs)?;
        }
        if !named_deps.is_empty() {
            map.serialize_entry("named_deps", named_deps)?;
        }
        serialize_platforms_dict(&mut map, platform)?;
        if let Some(preferred_linkage) = preferred_linkage {
            map.serialize_entry("preferred_linkage", preferred_linkage)?;
        }
        if *proc_macro {
            map.serialize_entry("proc_macro", &true)?;
        }
        if let Some(python_ext) = python_ext {
            map.serialize_entry("python_ext", python_ext)?;
        }
        if !rustc_flags.is_empty() {
            map.serialize_entry("rustc_flags", rustc_flags)?;
        }
        if !target_compatible_with.is_empty() {
            map.serialize_entry("target_compatible_with", target_compatible_with)?;
        }
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug)]
pub struct RustBinary {
    pub common: RustCommon,
}

impl Serialize for RustBinary {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            common:
                RustCommon {
                    common:
                        Common {
                            name,
                            visibility,
                            licenses,
                            compatible_with,
                            target_compatible_with,
                        },
                    krate,
                    crate_root,
                    edition,
                    base:
                        PlatformRustCommon {
                            srcs,
                            mapped_srcs,
                            rustc_flags,
                            features,
                            deps,
                            named_deps,
                            env,
                            link_style,
                            linker_flags,
                            preferred_linkage,
                        },
                    platform,
                },
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        if !srcs.is_empty() {
            map.serialize_entry("srcs", srcs)?;
        }
        if !compatible_with.is_empty() {
            map.serialize_entry("compatible_with", compatible_with)?;
        }
        map.serialize_entry("crate", krate)?;
        map.serialize_entry("crate_root", crate_root)?;
        map.serialize_entry("edition", edition)?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        if !features.is_empty() {
            map.serialize_entry("features", features)?;
        }
        if !licenses.is_empty() {
            map.serialize_entry("licenses", licenses)?;
        }
        if let Some(link_style) = link_style {
            map.serialize_entry("link_style", link_style)?;
        }
        if !linker_flags.is_empty() {
            map.serialize_entry("linker_flags", linker_flags)?;
        }
        if !mapped_srcs.is_empty() {
            map.serialize_entry("mapped_srcs", mapped_srcs)?;
        }
        if !named_deps.is_empty() {
            map.serialize_entry("named_deps", named_deps)?;
        }
        serialize_platforms_dict(&mut map, platform)?;
        if let Some(preferred_linkage) = preferred_linkage {
            map.serialize_entry("preferred_linkage", preferred_linkage)?;
        }
        if !rustc_flags.is_empty() {
            map.serialize_entry("rustc_flags", rustc_flags)?;
        }
        if !target_compatible_with.is_empty() {
            map.serialize_entry("target_compatible_with", target_compatible_with)?;
        }
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug)]
pub struct BuildscriptGenrule {
    pub name: Name,
    pub buildscript_rule: Name,
    pub package_name: String,
    pub version: Version,
    pub local_manifest_dir: Option<BuckPath>,
    pub manifest_dir: Option<Subtarget>,
    // Platform-dependent
    pub base: PlatformBuildscriptGenrule,
    // Platform-specific
    pub platform: BTreeMap<PlatformName, PlatformBuildscriptGenrule>,
}

impl Serialize for BuildscriptGenrule {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            buildscript_rule,
            package_name,
            version,
            local_manifest_dir,
            manifest_dir,
            base: PlatformBuildscriptGenrule { features, env },
            platform,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("package_name", package_name)?;
        map.serialize_entry("buildscript_rule", &NameAsLabel(buildscript_rule))?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        if !features.is_empty() {
            map.serialize_entry("features", features)?;
        }
        if let Some(local_manifest_dir) = local_manifest_dir {
            map.serialize_entry("local_manifest_dir", local_manifest_dir)?;
        }
        if let Some(manifest_dir) = manifest_dir {
            map.serialize_entry("manifest_dir", manifest_dir)?;
        }
        serialize_platforms_dict(&mut map, platform)?;
        map.serialize_entry("version", version)?;
        map.end()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlatformBuildscriptGenrule {
    pub features: BTreeSet<String>,
    pub env: BTreeMap<String, StringOrPath>,
}

impl Serialize for PlatformBuildscriptGenrule {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self { features, env } = self;
        let mut map = ser.serialize_map(None)?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        if !features.is_empty() {
            map.serialize_entry("features", features)?;
        }
        map.end()
    }
}

#[derive(Debug, PartialEq)]
pub struct CxxLibrary {
    pub common: Common,
    pub srcs: BTreeSet<SubtargetOrPath>,
    pub headers: BTreeSet<SubtargetOrPath>,
    pub exported_headers: SetOrMap<SubtargetOrPath>,
    pub compiler_flags: Vec<String>,
    pub preprocessor_flags: Vec<String>,
    pub header_namespace: Option<String>,
    pub include_directories: Vec<SubtargetOrPath>,
    pub deps: BTreeSet<RuleRef>,
    pub preferred_linkage: Option<String>,
    pub undefined_symbols: bool,
}

impl Serialize for CxxLibrary {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            common:
                Common {
                    name,
                    visibility,
                    licenses,
                    compatible_with,
                    target_compatible_with,
                },
            srcs,
            headers,
            exported_headers,
            compiler_flags,
            preprocessor_flags,
            header_namespace,
            include_directories,
            deps,
            preferred_linkage,
            undefined_symbols,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("srcs", srcs)?;
        map.serialize_entry("headers", headers)?;
        if let Some(header_namespace) = header_namespace {
            map.serialize_entry("header_namespace", header_namespace)?;
        }
        if !exported_headers.is_empty() {
            map.serialize_entry("exported_headers", exported_headers)?;
        }
        if !compatible_with.is_empty() {
            map.serialize_entry("compatible_with", compatible_with)?;
        }
        if !compiler_flags.is_empty() {
            map.serialize_entry("compiler_flags", compiler_flags)?;
        }
        if include_directories.iter().any(SubtargetOrPath::is_path) {
            map.serialize_entry(
                "include_directories",
                &IncludeDirectories {
                    include_directories,
                },
            )?;
        }
        if !licenses.is_empty() {
            map.serialize_entry("licenses", licenses)?;
        }
        if let Some(preferred_linkage) = preferred_linkage {
            map.serialize_entry("preferred_linkage", preferred_linkage)?;
        }
        if !preprocessor_flags.is_empty()
            || include_directories
                .iter()
                .any(SubtargetOrPath::is_subtarget)
        {
            map.serialize_entry(
                "preprocessor_flags",
                &PreprocessorFlags {
                    include_directories,
                    preprocessor_flags,
                },
            )?;
        }
        if !target_compatible_with.is_empty() {
            map.serialize_entry("target_compatible_with", target_compatible_with)?;
        }
        if *undefined_symbols {
            map.serialize_entry("undefined_symbols", undefined_symbols)?;
        }
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

struct IncludeDirectories<'a> {
    include_directories: &'a [SubtargetOrPath],
}

impl<'a> Serialize for IncludeDirectories<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let len = self
            .include_directories
            .iter()
            .filter(|dir| dir.is_path())
            .count();
        let mut array = serializer.serialize_seq(Some(len))?;

        for element in self.include_directories {
            match element {
                SubtargetOrPath::Subtarget(_) => {
                    // serialized under "preprocessor_flags" because "include_directories"
                    // does not support $(location ...) macros.
                }
                SubtargetOrPath::Path(path) => array.serialize_element(path)?,
            }
        }

        array.end()
    }
}

struct PreprocessorFlags<'a> {
    include_directories: &'a [SubtargetOrPath],
    preprocessor_flags: &'a [String],
}

impl<'a> Serialize for PreprocessorFlags<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let len = self
            .include_directories
            .iter()
            .filter(|dir| dir.is_subtarget())
            .count()
            + self.preprocessor_flags.len();
        let mut array = serializer.serialize_seq(Some(len))?;

        for element in self.include_directories {
            // Cannot just use `array.serialize_element(format!("-I{element}"))`:
            // the usual serialization of Subtarget as ":target[relative]" is not
            // appropriate for a directory. Use "$(location :target)/relative".
            match element {
                SubtargetOrPath::Subtarget(subtarget) => {
                    array.serialize_element(&format!(
                        "-I$(location :{})/{}",
                        subtarget.target, subtarget.relative,
                    ))?;
                }
                SubtargetOrPath::Path(_) => {
                    // serialized under "include_directories"
                }
            }
        }

        for element in self.preprocessor_flags {
            array.serialize_element(element)?;
        }

        array.end()
    }
}

#[derive(Debug, PartialEq)]
pub struct PrebuiltCxxLibrary {
    pub common: Common,
    pub static_lib: SubtargetOrPath,
}

impl Serialize for PrebuiltCxxLibrary {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            common:
                Common {
                    name,
                    visibility,
                    licenses,
                    compatible_with,
                    target_compatible_with,
                },
            static_lib,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        if !compatible_with.is_empty() {
            map.serialize_entry("compatible_with", compatible_with)?;
        }
        if !licenses.is_empty() {
            map.serialize_entry("licenses", licenses)?;
        }
        map.serialize_entry("static_lib", static_lib)?;
        if !target_compatible_with.is_empty() {
            map.serialize_entry("target_compatible_with", target_compatible_with)?;
        }
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(Debug)]
pub enum Rule {
    Alias(Alias),
    Filegroup(Filegroup),
    ExtractArchive(ExtractArchive),
    HttpArchive(HttpArchive),
    GitFetch(GitFetch),
    Binary(RustBinary),
    Library(RustLibrary),
    BuildscriptBinary(RustBinary),
    BuildscriptGenrule(BuildscriptGenrule),
    CxxLibrary(CxxLibrary),
    PrebuiltCxxLibrary(PrebuiltCxxLibrary),
    RootPackage(RustLibrary),
}

impl Eq for Rule {}

impl PartialEq for Rule {
    fn eq(&self, other: &Self) -> bool {
        self.get_name().eq(other.get_name())
    }
}

impl PartialOrd for Rule {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn rule_sort_key(rule: &Rule) -> impl Ord + '_ {
    #[derive(Ord, PartialOrd, Eq, PartialEq)]
    enum RuleSortKey<'a> {
        // Git_fetch targets go above all other targets. In general a single
        // repository can be used as the source of multiple crates.
        GitFetch(&'a Name),
        Other(&'a Name, usize),
        // Root package goes last since it's an uninteresting list of
        // deps that looks awkward anywhere else.
        RootPackage,
    }

    match rule {
        // Make the alias rule come before the actual rule. Note that aliases
        // emitted by reindeer are always to a target within the same package.
        Rule::Alias(Alias { actual, .. }) => RuleSortKey::Other(actual, 0),
        Rule::ExtractArchive(ExtractArchive { sort_key, .. }) => RuleSortKey::Other(sort_key, 1),
        Rule::HttpArchive(HttpArchive { sort_key, .. }) => RuleSortKey::Other(sort_key, 1),
        Rule::GitFetch(GitFetch { name, .. }) => RuleSortKey::GitFetch(name),
        Rule::Filegroup(_)
        | Rule::Binary(_)
        | Rule::Library(_)
        | Rule::BuildscriptBinary(_)
        | Rule::BuildscriptGenrule(_)
        | Rule::CxxLibrary(_)
        | Rule::PrebuiltCxxLibrary(_) => RuleSortKey::Other(rule.get_name(), 2),
        Rule::RootPackage(_) => RuleSortKey::RootPackage,
    }
}

impl Ord for Rule {
    fn cmp(&self, other: &Self) -> Ordering {
        rule_sort_key(self).cmp(&rule_sort_key(other))
    }
}

impl Rule {
    pub fn get_name(&self) -> &Name {
        match self {
            Rule::Alias(Alias { name, .. })
            | Rule::Filegroup(Filegroup { name, .. })
            | Rule::HttpArchive(HttpArchive { name, .. })
            | Rule::ExtractArchive(ExtractArchive { name, .. })
            | Rule::GitFetch(GitFetch { name, .. })
            | Rule::Binary(RustBinary {
                common:
                    RustCommon {
                        common: Common { name, .. },
                        ..
                    },
                ..
            })
            | Rule::Library(RustLibrary {
                common:
                    RustCommon {
                        common: Common { name, .. },
                        ..
                    },
                ..
            })
            | Rule::BuildscriptBinary(RustBinary {
                common:
                    RustCommon {
                        common: Common { name, .. },
                        ..
                    },
                ..
            })
            | Rule::BuildscriptGenrule(BuildscriptGenrule { name, .. })
            | Rule::CxxLibrary(CxxLibrary {
                common: Common { name, .. },
                ..
            })
            | Rule::PrebuiltCxxLibrary(PrebuiltCxxLibrary {
                common: Common { name, .. },
                ..
            })
            | Rule::RootPackage(RustLibrary {
                common:
                    RustCommon {
                        common: Common { name, .. },
                        ..
                    },
                ..
            }) => name,
        }
    }

    pub fn render(&self, config: &BuckConfig, out: &mut impl Write) -> anyhow::Result<()> {
        use serde_starlark::Serializer;
        let serialized = match self {
            Rule::Alias(alias) => {
                let function = match alias.platforms {
                    None => &config.alias,
                    Some(_) => &config.alias_with_platforms,
                };
                FunctionCall::new(function, alias).serialize(Serializer)
            }
            Rule::Filegroup(filegroup) => {
                FunctionCall::new(&config.filegroup, filegroup).serialize(Serializer)
            }
            Rule::ExtractArchive(compressed_crate) => {
                FunctionCall::new(&config.extract_archive, compressed_crate).serialize(Serializer)
            }
            Rule::HttpArchive(http_archive) => {
                FunctionCall::new(&config.http_archive, http_archive).serialize(Serializer)
            }
            Rule::GitFetch(git_fetch) => {
                FunctionCall::new(&config.git_fetch, git_fetch).serialize(Serializer)
            }
            Rule::Binary(bin) => FunctionCall::new(&config.rust_binary, bin).serialize(Serializer),
            Rule::Library(lib) | Rule::RootPackage(lib) => {
                FunctionCall::new(&config.rust_library, lib).serialize(Serializer)
            }
            Rule::BuildscriptBinary(bin) => {
                let buildscript_binary = config
                    .buildscript_binary
                    .as_ref()
                    .unwrap_or(&config.rust_binary);
                FunctionCall::new(buildscript_binary, bin).serialize(Serializer)
            }
            Rule::BuildscriptGenrule(lib) => {
                FunctionCall::new(&config.buildscript_genrule, lib).serialize(Serializer)
            }
            Rule::CxxLibrary(lib) => {
                FunctionCall::new(&config.cxx_library, lib).serialize(Serializer)
            }
            Rule::PrebuiltCxxLibrary(lib) => {
                FunctionCall::new(&config.prebuilt_cxx_library, lib).serialize(Serializer)
            }
        }?;
        out.write_all(serialized.as_bytes())?;
        Ok(())
    }
}

/// Buildifier's preferred sort order for sortable string arrays, regardless of
/// whether they are arrays of filepaths or labels.
///
/// See similar logic in <https://github.com/bazelbuild/buildtools/blob/5.1.0/build/rewrite.go#L590-L622>
fn buildifier_cmp(a: &str, b: &str) -> Ordering {
    let phase = |s: &str| {
        if s.starts_with(':') {
            1
        } else if s.starts_with("//") {
            2
        } else {
            0
        }
    };

    phase(a).cmp(&phase(b)).then_with(|| {
        let separators = [':', '.'];
        a.split(separators).cmp(b.split(separators))
    })
}

pub fn write_buckfile<'a>(
    config: &BuckConfig,
    rules: impl Iterator<Item = &'a Rule>,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    out.write_all(config.generated_file_header.as_bytes())?;
    if !config.generated_file_header.is_empty() {
        out.write_all(b"\n")?;
    }

    out.write_all(config.buckfile_imports.as_bytes())?;
    if !config.buckfile_imports.is_empty() {
        out.write_all(b"\n")?;
    }

    for (i, rule) in rules.enumerate() {
        if i > 0 {
            out.write_all(b"\n")?;
        }
        rule.render(config, out)?;
    }

    Ok(())
}
