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
use std::fmt::Display;
use std::io::Error;
use std::io::Write;
use std::path::PathBuf;

use semver::Version;
use serde::ser::SerializeMap;
use serde::ser::Serializer;
use serde::Serialize;

use crate::collection::SetOrMap;
use crate::config::BuckConfig;
use crate::platform::PlatformConfig;
use crate::platform::PlatformExpr;
use crate::platform::PlatformName;
use crate::platform::PlatformPredicate;
use crate::platform::PredicateParseError;

/// Only the name of a target. Does not include package path, nor leading colon.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct Name(pub String);

impl Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RuleRef {
    pub target: String,
    platform: Option<PlatformExpr>,
}

impl From<Name> for RuleRef {
    fn from(name: Name) -> Self {
        RuleRef::new(format!(":{}", name))
    }
}

impl Ord for RuleRef {
    fn cmp(&self, other: &Self) -> Ordering {
        buildifier_cmp(&self.target, &other.target).then_with(|| self.platform.cmp(&other.platform))
    }
}

impl PartialOrd for RuleRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl RuleRef {
    pub fn new(target: String) -> Self {
        RuleRef {
            target,
            platform: None,
        }
    }

    pub fn with_platform(self, platform: Option<&PlatformExpr>) -> Self {
        RuleRef {
            target: self.target,
            platform: platform.cloned(),
        }
    }

    pub fn has_platform(&self) -> bool {
        self.platform.is_some()
    }

    /// Return true if one of the platform_configs applies to this rule. Always returns
    /// true if this dep has no platform constraint.
    pub fn filter(&self, platform_config: &PlatformConfig) -> Result<bool, PredicateParseError> {
        let res = match &self.platform {
            None => true,
            Some(cfg) => {
                let cfg = PlatformPredicate::parse(cfg)?;

                cfg.eval(platform_config)
            }
        };
        Ok(res)
    }
}

impl Serialize for RuleRef {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.target.serialize(ser)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuckPath(pub PathBuf);

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

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum Visibility {
    Public,
    Private,
}

impl Serialize for Visibility {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let buck_visibility: &[&str] = match self {
            Visibility::Public => &["PUBLIC"],
            Visibility::Private => &[],
        };
        buck_visibility.serialize(ser)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Alias {
    pub name: Name,
    /// Local target that the alias refers to -- always in the same package.
    pub actual: Name,
    pub visibility: Visibility,
}

impl Serialize for Alias {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            name,
            actual,
            visibility,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("actual", &NameAsLabel(actual))?;
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

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Common {
    pub name: Name,
    pub visibility: Visibility,
    pub licenses: BTreeSet<BuckPath>,
    pub compatible_with: Vec<RuleRef>,
}

// Rule attributes which could be platform-specific
#[derive(Debug, Default, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct PlatformRustCommon {
    pub srcs: BTreeSet<BuckPath>,
    pub mapped_srcs: BTreeMap<String, BuckPath>,
    pub rustc_flags: Vec<String>,
    pub features: BTreeSet<String>,
    pub deps: BTreeSet<RuleRef>,
    pub named_deps: BTreeMap<String, RuleRef>,
    pub env: BTreeMap<String, String>,

    // This isn't really "common" (Binaries only), but does need to be platform
    pub link_style: Option<String>,

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

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct RustCommon {
    pub common: Common,
    pub krate: String,
    pub rootmod: BuckPath,
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
/// ```
fn serialize_platforms_dict<S>(
    map: &mut S,
    platforms: &BTreeMap<PlatformName, PlatformRustCommon>,
) -> Result<(), S::Error>
where
    S: SerializeMap,
{
    #[derive(Serialize)]
    #[serde(rename = "call:dict")]
    struct Dict<T>(T);

    struct Platforms<'a>(&'a BTreeMap<PlatformName, PlatformRustCommon>);

    impl Serialize for Platforms<'_> {
        fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
            ser.collect_map(self.0.iter().map(|(name, value)| (name, Dict(value))))
        }
    }

    map.serialize_entry("platform", &Platforms(platforms))
}

#[derive(Debug, Clone, Eq, PartialEq)]
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
                        },
                    krate,
                    rootmod,
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
        map.serialize_entry("crate_root", rootmod)?;
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
        if let Some(linkable_alias) = linkable_alias {
            map.serialize_entry("linkable_alias", linkable_alias)?;
        }
        if !mapped_srcs.is_empty() {
            map.serialize_entry("mapped_srcs", mapped_srcs)?;
        }
        if !named_deps.is_empty() {
            map.serialize_entry("named_deps", named_deps)?;
        }
        if !platform.is_empty() {
            serialize_platforms_dict(&mut map, platform)?;
        }
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
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
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
                        },
                    krate,
                    rootmod,
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
        map.serialize_entry("crate_root", rootmod)?;
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
        if !mapped_srcs.is_empty() {
            map.serialize_entry("mapped_srcs", mapped_srcs)?;
        }
        if !named_deps.is_empty() {
            map.serialize_entry("named_deps", named_deps)?;
        }
        if !platform.is_empty() {
            serialize_platforms_dict(&mut map, platform)?;
        }
        if let Some(preferred_linkage) = preferred_linkage {
            map.serialize_entry("preferred_linkage", preferred_linkage)?;
        }
        if !rustc_flags.is_empty() {
            map.serialize_entry("rustc_flags", rustc_flags)?;
        }
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuildscriptGenrule {
    pub name: Name,
    pub buildscript_rule: Name,
    pub package_name: String,
    pub version: Version,
    pub features: BTreeSet<String>,
    pub cfgs: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub path_env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuildscriptGenruleArgs {
    pub base: BuildscriptGenrule,
    pub outfile: String,
}

impl Serialize for BuildscriptGenruleArgs {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            base:
                BuildscriptGenrule {
                    name,
                    buildscript_rule,
                    package_name,
                    version,
                    features,
                    cfgs,
                    env,
                    path_env,
                },
            outfile,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("package_name", package_name)?;
        map.serialize_entry("buildscript_rule", &NameAsLabel(buildscript_rule))?;
        map.serialize_entry("cfgs", cfgs)?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        map.serialize_entry("features", features)?;
        map.serialize_entry("outfile", outfile)?;
        if !path_env.is_empty() {
            map.serialize_entry("path_env", path_env)?;
        }
        map.serialize_entry("version", version)?;
        map.end()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BuildscriptGenruleSrcs {
    pub base: BuildscriptGenrule,
    pub files: BTreeSet<String>,
    pub srcs: BTreeSet<BuckPath>,
}

impl Serialize for BuildscriptGenruleSrcs {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let Self {
            base:
                BuildscriptGenrule {
                    name,
                    buildscript_rule,
                    package_name,
                    version,
                    features,
                    cfgs,
                    env,
                    path_env,
                },
            files,
            srcs,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("package_name", package_name)?;
        if !srcs.is_empty() {
            map.serialize_entry("srcs", srcs)?;
        }
        map.serialize_entry("buildscript_rule", &NameAsLabel(buildscript_rule))?;
        map.serialize_entry("cfgs", cfgs)?;
        if !env.is_empty() {
            map.serialize_entry("env", env)?;
        }
        map.serialize_entry("features", features)?;
        map.serialize_entry("files", files)?;
        if !path_env.is_empty() {
            map.serialize_entry("path_env", path_env)?;
        }
        map.serialize_entry("version", version)?;
        map.end()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CxxLibrary {
    pub common: Common,
    pub srcs: BTreeSet<BuckPath>,
    pub headers: BTreeSet<BuckPath>,
    pub exported_headers: SetOrMap<BuckPath>,
    pub compiler_flags: Vec<String>,
    pub preprocessor_flags: Vec<String>,
    pub header_namespace: Option<String>,
    pub include_directories: Vec<BuckPath>,
    pub deps: BTreeSet<RuleRef>,
    pub preferred_linkage: Option<String>,
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
        if !include_directories.is_empty() {
            map.serialize_entry("include_directories", include_directories)?;
        }
        if !licenses.is_empty() {
            map.serialize_entry("licenses", licenses)?;
        }
        map.serialize_entry("preferred_linkage", preferred_linkage)?;
        if !preprocessor_flags.is_empty() {
            map.serialize_entry("preprocessor_flags", preprocessor_flags)?;
        }
        map.serialize_entry("visibility", visibility)?;
        if !deps.is_empty() {
            map.serialize_entry("deps", deps)?;
        }
        map.end()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PrebuiltCxxLibrary {
    pub common: Common,
    pub static_lib: BuckPath,
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
        map.serialize_entry("visibility", visibility)?;
        map.end()
    }
}

#[derive(Debug)]
pub enum Rule {
    Alias(Alias),
    Binary(RustBinary),
    Library(RustLibrary),
    BuildscriptGenruleSrcs(BuildscriptGenruleSrcs),
    BuildscriptGenruleArgs(BuildscriptGenruleArgs),
    CxxLibrary(CxxLibrary),
    PrebuiltCxxLibrary(PrebuiltCxxLibrary),
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

fn rule_sort_key(rule: &Rule) -> (&Name, usize) {
    match rule {
        // Make the alias rule come before the actual rule. Note that aliases
        // emitted by reindeer are always to a target within the same package.
        Rule::Alias(Alias { actual, .. }) => (actual, 0),
        Rule::Binary(_)
        | Rule::Library(_)
        | Rule::BuildscriptGenruleSrcs(_)
        | Rule::BuildscriptGenruleArgs(_)
        | Rule::CxxLibrary(_)
        | Rule::PrebuiltCxxLibrary(_) => (rule.get_name(), 1),
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
            | Rule::BuildscriptGenruleSrcs(BuildscriptGenruleSrcs {
                base: BuildscriptGenrule { name, .. },
                ..
            })
            | Rule::BuildscriptGenruleArgs(BuildscriptGenruleArgs {
                base: BuildscriptGenrule { name, .. },
                ..
            })
            | Rule::CxxLibrary(CxxLibrary {
                common: Common { name, .. },
                ..
            })
            | Rule::PrebuiltCxxLibrary(PrebuiltCxxLibrary {
                common: Common { name, .. },
                ..
            }) => name,
        }
    }

    pub fn render(&self, config: &BuckConfig, out: &mut impl Write) -> Result<(), Error> {
        match self {
            Rule::Alias(alias) => {
                out.write_all(serde_starlark::function_call(&config.alias, &alias)?.as_bytes())?;
            }
            Rule::Binary(bin) => {
                out.write_all(
                    serde_starlark::function_call(&config.rust_binary, &bin)?.as_bytes(),
                )?;
            }
            Rule::Library(lib) => {
                out.write_all(
                    serde_starlark::function_call(&config.rust_library, &lib)?.as_bytes(),
                )?;
            }
            Rule::BuildscriptGenruleArgs(lib) => {
                out.write_all(
                    serde_starlark::function_call(&config.buildscript_genrule_args, &lib)?
                        .as_bytes(),
                )?;
            }
            Rule::BuildscriptGenruleSrcs(lib) => {
                out.write_all(
                    serde_starlark::function_call(&config.buildscript_genrule_srcs, &lib)?
                        .as_bytes(),
                )?;
            }
            Rule::CxxLibrary(lib) => {
                out.write_all(
                    serde_starlark::function_call(&config.cxx_library, &lib)?.as_bytes(),
                )?;
            }
            Rule::PrebuiltCxxLibrary(lib) => {
                out.write_all(
                    serde_starlark::function_call(&config.prebuilt_cxx_library, &lib)?.as_bytes(),
                )?;
            }
        };
        out.write_all(b"\n")
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
) -> Result<(), Error> {
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
