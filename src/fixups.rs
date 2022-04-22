/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Per-package configuration information

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt, fs, iter,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use globset::{GlobBuilder, GlobSetBuilder};

use walkdir::WalkDir;

use crate::{
    buck::{
        self, BuildscriptGenrule, BuildscriptGenruleFilter, BuildscriptGenruleSrcs, Common, Rule,
        RuleRef, RustBinary,
    },
    buckify::{normalize_dotdot, relative_path},
    cargo::{Manifest, ManifestTarget},
    collection::SetOrMap,
    config::Config,
    index::{Index, ResolvedDep},
    platform::{platform_names_for_expr, PlatformExpr, PlatformPredicate},
    Paths,
};

mod buildscript;
mod config;

use buildscript::{
    BuildscriptFixup, CxxLibraryFixup, GenSrcs, PrebuiltCxxLibraryFixup, RustcFlags,
};
use config::FixupConfigFile;

/// Fixups for a specific package & target
pub struct Fixups<'meta> {
    config: &'meta Config,
    cell_dir: PathBuf,
    third_party_dir: PathBuf,
    index: &'meta Index<'meta>,
    package: &'meta Manifest,
    target: &'meta ManifestTarget,
    fixup_dir: PathBuf,
    fixup_config: FixupConfigFile,
    manifest_dir: &'meta Path,
}

impl<'meta> fmt::Debug for Fixups<'meta> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Fixups")
            .field("package", &self.package.to_string())
            .field("target", &self.target)
            .field("third_party_dir", &self.third_party_dir)
            .field("fixup_dir", &self.fixup_dir)
            .field("manifest_dir", &self.manifest_dir)
            .field("fixup_config", &self.fixup_config)
            .finish()
    }
}

impl<'meta> Fixups<'meta> {
    /// Get any fixups needed for a specific package target
    pub fn new(
        config: &'meta Config,
        paths: &Paths,
        index: &'meta Index,
        package: &'meta Manifest,
        target: &'meta ManifestTarget,
    ) -> Result<Self> {
        let fixup_dir = paths.third_party_dir.join("fixups").join(&package.name);
        let fixup_path = fixup_dir.join("fixups.toml");

        let fixup_config: FixupConfigFile = if let Ok(file) = fs::read(&fixup_path) {
            log::debug!("read fixups from {}", fixup_path.display());
            toml::de::from_slice(&file)
                .context(format!("Failed to parse {}", fixup_path.display()))?
        } else {
            log::debug!("no fixups at {}", fixup_path.display());
            let fixup = FixupConfigFile::template(&paths.third_party_dir, index, package, target);
            if config.fixup_templates && target.kind_custom_build() {
                log::info!(
                    "Writing template for {} to {}",
                    package,
                    relative_path(&paths.third_party_dir, &fixup_path).display()
                );
                log::debug!(
                    "fixup template: {:#?}, path {}",
                    fixup,
                    fixup_path.display()
                );

                let file = toml::ser::to_string_pretty(&fixup)?;
                fs::create_dir_all(&fixup_path.parent().unwrap())?;
                fs::write(&fixup_path, file)?;
            }
            fixup
        };

        Ok(Fixups {
            third_party_dir: paths.third_party_dir.to_path_buf(),
            cell_dir: paths.cell_dir.to_path_buf(),
            manifest_dir: package.manifest_dir(),
            index,
            package,
            target,
            fixup_dir,
            fixup_config,
            config,
        })
    }

    // Return true if the script applies to this target.
    // There's a few cases:
    // - if the script doesn't specify a target, then it applies to the main "lib" target of the package
    // - otherwise it applies to the matching kind and (optionally) name (all names if not specified)
    fn target_match(&self, script: &BuildscriptFixup) -> bool {
        let default = self.package.dependency_target();

        match (script.targets(), default) {
            (Some([]), Some(default)) | (None, Some(default)) => self.target == default,
            (Some([]), None) | (None, None) => false,
            (Some(tgts), _) => tgts.iter().any(|(kind, name)| {
                self.target.kind.contains(kind)
                    && name.as_ref().map_or(true, |name| &self.target.name == name)
            }),
        }
    }

    pub fn python_ext(&self) -> Option<&str> {
        self.fixup_config.python_ext.as_deref()
    }

    pub fn omit_target(&self) -> bool {
        self.fixup_config.omit_targets.contains(&self.target.name)
    }

    fn buildscript_target(&self) -> Option<&ManifestTarget> {
        self.package
            .targets
            .iter()
            .find(|tgt| tgt.kind_custom_build())
    }

    fn buildscript_rustc_flags_rulename(&self) -> String {
        format!(
            "{}-args",
            self.buildscript_rule_name().expect("no buildscript")
        )
    }

    fn buildscript_gen_srcs_rulename(&self, file: Option<&str>) -> String {
        if let Some(file) = file {
            format!(
                "{}-srcs={}",
                self.buildscript_rule_name().expect("no buildscript"),
                file
            )
        } else {
            format!(
                "{}-srcs",
                self.buildscript_rule_name().expect("no buildscript")
            )
        }
    }

    fn buildscript_rule_name(&self) -> Option<String> {
        self.buildscript_target()
            .map(|tgt| format!("{}-{}", self.package, tgt.name))
    }

    /// Return buildscript-related rules
    /// The rules may be platform specific, but they're emitted unconditionally. (The
    /// dependencies referencing them are conditional).
    pub fn emit_buildscript_rules(
        &self,
        buildscript: RustBinary,
        config: &'meta Config,
    ) -> Result<Vec<Rule>> {
        let mut res = Vec::new();

        let rel_manifest = relative_path(&self.third_party_dir, self.manifest_dir);
        let rel_fixup = relative_path(&self.third_party_dir, &self.fixup_dir);

        let buildscript_rule_name = match self.buildscript_rule_name() {
            None => {
                log::warn!(
                    "Package {} doesn't have a build script to fix up",
                    self.package
                );
                return Ok(res);
            }
            Some(name) => name,
        };

        // Generate features extracting them from the buildscript RustBinary. The assumption
        // that fixups are already per-platform if necessary so there's no need for platform-specific
        // rule attributes.
        let mut features = BTreeSet::new();
        for (plat, _fixup) in self.fixup_config.configs(&self.package.version) {
            match plat {
                None => features.extend(buildscript.common.base.features.iter().cloned()),
                Some(expr) => {
                    let platnames = platform_names_for_expr(self.config, expr)?;
                    for platname in platnames {
                        if let Some(platattr) = buildscript.common.platform.get(platname) {
                            features.extend(platattr.features.iter().cloned())
                        }
                    }
                }
            }
        }

        // Flat list of buildscript fixups.
        let fixes = self
            .fixup_config
            .configs(&self.package.version)
            .flat_map(|(_platform, fixup)| fixup.buildscript.iter());

        for fix in fixes {
            match fix {
                // Just emit a build rule for it, but don't otherwise do anything
                BuildscriptFixup::Build => res.push(Rule::Binary(buildscript.clone())),

                // Build and run it, and filter the output for --cfg options
                // for the main target's rustc command line
                BuildscriptFixup::RustcFlags(RustcFlags { env, path_env, .. }) => {
                    // Emit the build script itself
                    res.push(Rule::Binary(buildscript.clone()));

                    // Emit rule to get its stdout and filter it into args
                    res.push(Rule::BuildscriptGenruleFilter(BuildscriptGenruleFilter {
                        base: BuildscriptGenrule {
                            name: self.buildscript_rustc_flags_rulename(),

                            buildscript_rule: RuleRef::local(buildscript_rule_name.clone()),
                            package_name: self.package.name.clone(),
                            version: self.package.version.clone(),
                            features: features.clone(),
                            cfgs: vec![],
                            env: env.clone(),
                            path_env: path_env.clone(),
                        },
                        outfile: "args.txt".to_string(),
                    }))
                }

                // Generated source files - given a list, set up rules to extract them from
                // the buildscript.
                BuildscriptFixup::GenSrcs(GenSrcs {
                    input_srcs, // input file globs
                    files,      // output files
                    mapped,     // outputs mapped to a different path
                    env,        // env set while running
                    path_env,   // env pointing to pathnames set while running
                    ..
                }) => {
                    // Emit the build script itself
                    res.push(Rule::Binary(buildscript.clone()));

                    let srcs = self
                        .manifestwalk(
                            input_srcs.iter().map(String::as_str),
                            iter::empty::<&str>(),
                            self.config.strict_globs,
                        )
                        .context("generated file inputs")?
                        .collect();

                    // Emit rules to extract generated sources
                    res.push(Rule::BuildscriptGenruleSrcs(BuildscriptGenruleSrcs {
                        base: BuildscriptGenrule {
                            name: self.buildscript_gen_srcs_rulename(None),

                            buildscript_rule: RuleRef::local(buildscript_rule_name.clone()),
                            package_name: self.package.name.clone(),
                            version: self.package.version.clone(),
                            features: features.clone(),
                            cfgs: vec![],
                            env: env.clone(),
                            path_env: path_env.clone(),
                        },
                        files: files
                            .clone()
                            .into_iter()
                            .chain(mapped.keys().cloned())
                            .collect(),
                        srcs,
                    }))
                }

                // Emit a C++ library build rule (elsewhere - add a dependency to it)
                BuildscriptFixup::CxxLibrary(CxxLibraryFixup {
                    name,
                    srcs,
                    headers,
                    exported_headers,
                    public,
                    include_paths,
                    fixup_include_paths,
                    exclude,
                    compiler_flags,
                    preprocessor_flags,
                    header_namespace,
                    deps,
                    compatible_with,
                    ..
                }) => {
                    let rule = buck::CxxLibrary {
                        common: Common {
                            name: if *public {
                                format!(
                                    "{}-{}",
                                    self.index
                                        .public_alias(self.package)
                                        .unwrap_or(self.package.name.as_str()),
                                    name
                                )
                            } else {
                                format!("{}-{}", self.package, name)
                            },
                            public: *public,
                            licenses: Default::default(),
                            compatible_with: compatible_with
                                .iter()
                                .cloned()
                                .map(RuleRef::abs)
                                .collect(),
                        },
                        // Just collect the sources, excluding things in the exclude list
                        srcs: self
                            .manifestwalk(srcs, exclude, self.config.strict_globs)
                            .context("C++ sources")?
                            .collect(),
                        // Collect the nominated headers, plus everything in the fixup include
                        // path(s).
                        headers: self::globwalk(
                            &self.third_party_dir,
                            &[&rel_manifest, &rel_fixup],
                            headers
                                .iter()
                                .map(|path| format!("{}/{}", rel_manifest.display(), path))
                                .chain(itertools::concat(fixup_include_paths.iter().map(|path| {
                                    vec!["asm", "h"]
                                        .iter()
                                        .map(|ext| {
                                            format!(
                                                "{}/**/*.{}",
                                                rel_fixup.join(path).display(),
                                                ext
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                }))),
                            exclude
                                .iter()
                                .map(|path| format!("{}/{}", rel_manifest.display(), path)),
                            false,
                        )
                        .context("C++ headers")?
                        .collect(),
                        exported_headers: match exported_headers {
                            SetOrMap::Set(exported_headers) => SetOrMap::Set(
                                self.manifestwalk(
                                    exported_headers,
                                    exclude,
                                    self.config.strict_globs,
                                )
                                .context("C++ exported headers")?
                                .collect(),
                            ),
                            SetOrMap::Map(exported_headers) => {
                                let rel_manifest =
                                    relative_path(&self.third_party_dir, self.manifest_dir);
                                SetOrMap::Map(
                                    exported_headers
                                        .iter()
                                        .map(|(name, path)| (name.clone(), rel_manifest.join(path)))
                                        .collect(),
                                )
                            }
                        },
                        include_directories: fixup_include_paths
                            .iter()
                            .map(|path| rel_fixup.join(path))
                            .chain(include_paths.iter().map(|path| rel_manifest.join(path)))
                            .collect(),
                        compiler_flags: compiler_flags.clone(),
                        preprocessor_flags: preprocessor_flags.clone(),
                        header_namespace: header_namespace.clone(),
                        deps: deps.iter().cloned().map(RuleRef::abs).collect(),
                        preferred_linkage: Some("static".to_string()),
                    };

                    res.push(Rule::CxxLibrary(rule));
                }

                // Emit a prebuilt C++ library rule for each static library (elsewhere - add dependencies to them)
                BuildscriptFixup::PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup {
                    name,
                    static_libs,
                    public,
                    compatible_with,
                    ..
                }) => {
                    let libs = self
                        .manifestwalk(static_libs, Vec::<String>::new(), self.config.strict_globs)
                        .context("Static libraries")?;
                    for static_lib in libs {
                        let rule = buck::PrebuiltCxxLibrary {
                            common: Common {
                                name: if *public {
                                    format!(
                                        "{}-{}-{}",
                                        self.index
                                            .public_alias(self.package)
                                            .unwrap_or(self.package.name.as_str()),
                                        name,
                                        static_lib.file_name().unwrap().to_string_lossy()
                                    )
                                } else {
                                    format!(
                                        "{}-{}-{}",
                                        self.package,
                                        name,
                                        static_lib.file_name().unwrap().to_string_lossy()
                                    )
                                },
                                public: *public,
                                licenses: Default::default(),
                                compatible_with: compatible_with
                                    .iter()
                                    .cloned()
                                    .map(RuleRef::abs)
                                    .collect(),
                            },
                            static_lib: static_lib.clone(),
                        };
                        res.push(Rule::PrebuiltCxxLibrary(rule));
                    }
                }

                // Complain and omit
                BuildscriptFixup::Unresolved(msg) => {
                    let unresolved_package_msg = format!(
                        "{} has a build script, but I don't know what to do with it: {}",
                        self.package, msg
                    );
                    if config.unresolved_fixup_error {
                        log::error!("{}", unresolved_package_msg);
                        return Err(anyhow!(
                            "Unresolved fix up errors, please fix them and rerun buckify."
                        ));
                    } else {
                        log::warn!("{}", unresolved_package_msg);
                    }
                }
            }
        }

        Ok(res)
    }

    /// Return the set of features to enable, which is the union of the cargo-reso=lved ones
    /// and additional ones defined in the fixup.
    pub fn compute_features(&self) -> Vec<(Option<PlatformExpr>, BTreeSet<String>)> {
        let mut ret = vec![];

        let mut omits = HashMap::new();
        let mut all_omits = HashSet::new();
        // Pre-compute the list of all filtered features. If a platform filters a feature
        // added by the base, we need to filter it from the base and add it to all other platforms.
        for (platform, fixup) in self.fixup_config.configs(&self.package.version) {
            let platform_omits = omits.entry(platform).or_insert_with(HashSet::new);
            platform_omits.extend(fixup.omit_features.iter().map(String::as_str));
            all_omits.extend(fixup.omit_features.iter().map(String::as_str));
        }

        let resolved_features: Vec<_> = self.index.resolved_features(self.package).collect();
        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut set = BTreeSet::new();

            for feature in resolved_features.iter() {
                if platform.is_none() {
                    if !all_omits.contains(feature) {
                        set.insert(feature.to_string());
                    }
                } else {
                    // If something omits this, check if this platform omits it.
                    if all_omits.contains(feature) {
                        if let Some(platform_omits) = omits.get(&platform) {
                            if platform_omits.contains(feature) {
                                continue;
                            }
                        }
                        set.insert(feature.to_string());
                    }
                }
            }

            set.extend(config.features.clone());

            if !set.is_empty() {
                ret.push((platform.cloned(), set));
            }
        }

        ret
    }

    fn buildscript_rustc_flags(&self) -> Vec<(Option<PlatformExpr>, Vec<String>)> {
        let mut ret = vec![];
        if self.buildscript_target().is_none() {
            return ret; // no buildscript
        }

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut flags = vec![];

            for buildscript in &config.buildscript {
                if !self.target_match(buildscript) {
                    continue;
                }
                if let BuildscriptFixup::RustcFlags(_) = buildscript {
                    flags.push(format!(
                        "@$(location :{})",
                        self.buildscript_rustc_flags_rulename()
                    ))
                }
            }

            if !flags.is_empty() {
                ret.push((platform.cloned(), flags));
            }
        }

        ret
    }

    /// Return extra command-line options, with platform annotation if needed
    pub fn compute_cmdline(&self) -> Vec<(Option<PlatformExpr>, Vec<String>)> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut flags = vec![];

            flags.extend(config.rustc_flags.clone());
            flags.extend(config.cfgs.iter().map(|cfg| format!("--cfg={}", cfg)));

            if !flags.is_empty() {
                ret.push((platform.cloned(), flags));
            }
        }

        ret.extend(self.buildscript_rustc_flags());

        ret
    }

    /// Generate the set of deps for the target. This could just return the unmodified
    /// depenedencies, or it could add/remove them. This returns the Buck rule reference
    /// and the corresponding package if there is one (so the caller can limit its enumeration
    /// to only targets which were actually used).
    pub fn compute_deps(&self) -> Result<Vec<(Option<&'meta Manifest>, RuleRef, Option<String>)>> {
        let mut ret = vec![];

        let mut omits = HashMap::new();
        let mut all_omits = HashSet::new();
        // Pre-compute the list of all filtered dependencies. If a platform filters a dependency
        // added by the base, we need to filter it from the base and add it to all other platforms.
        for (platform, fixup) in self.fixup_config.configs(&self.package.version) {
            let platform_omits = omits.entry(platform).or_insert_with(HashSet::new);
            platform_omits.extend(fixup.filter_deps.iter().map(String::as_str));
            all_omits.extend(fixup.filter_deps.iter().map(String::as_str));
        }

        for ResolvedDep {
            alias,
            package,
            platform,
        } in self
            .index
            .resolved_deps_for_target(self.package, self.target)
        {
            log::debug!(
                "Resolved deps for {}/{} ({:?}) - {} as {} ({:?})",
                self.package,
                self.target.name,
                self.target.kind(),
                package,
                alias,
                self.target.kind()
            );

            // Only use the alias if it isn't the same as the target anyway
            let tgtname = package
                .dependency_target()
                .map(|tgt| tgt.name.replace('-', "_"));

            let original_alias = alias;

            let alias = match tgtname {
                Some(ref tgtname) if tgtname == alias => None,
                Some(_) | None => Some(alias.to_string()),
            };

            if all_omits.contains(original_alias) {
                // If the dependency is for a particular platform and that has it excluded,
                // skip it.
                if let Some(platform_omits) = omits.get(&platform.as_ref()) {
                    if platform_omits.contains(original_alias) {
                        continue;
                    }
                }

                // If it's a default dependency, but certain specific platforms filter it,
                // produce a new rule that excludes those platforms.
                if platform.is_none() {
                    // Create a new predicate that excludes all filtered platforms.
                    let mut excludes = vec![];
                    for (platform_expr, platform_omits) in &omits {
                        if let Some(platform_expr) = platform_expr {
                            if platform_omits.contains(original_alias) {
                                let platform_pred = PlatformPredicate::parse(platform_expr)?;
                                excludes.push(PlatformPredicate::Not(Box::new(platform_pred)));
                            }
                        }
                    }

                    let platform_pred = PlatformPredicate::All(excludes);
                    let platform_expr: PlatformExpr = format!("cfg({})", platform_pred).into();
                    ret.push((
                        Some(package),
                        RuleRef::local(self.index.rule_name(package))
                            .with_platform(Some(&platform_expr)),
                        alias.clone(),
                    ));

                    // Since we've already added the platform-excluding rule, skip the generic rule
                    // adding below.
                    continue;
                }
            }

            // No filtering involved? Just insert it like normal.
            ret.push((
                Some(package),
                RuleRef::local(self.index.rule_name(package)).with_platform(platform.as_ref()),
                alias,
            ))
        }

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            ret.extend(config.extra_deps.iter().map(|dep| {
                (
                    None,
                    RuleRef::abs(dep.to_string()).with_platform(platform),
                    None,
                )
            }));
            for buildscript in &config.buildscript {
                if !self.target_match(buildscript) {
                    continue;
                }
                if let BuildscriptFixup::CxxLibrary(CxxLibraryFixup {
                    add_dep: true,
                    name,
                    public,
                    ..
                }) = buildscript
                {
                    let dep_name = if *public {
                        format!(
                            "{}-{}",
                            self.index
                                .public_alias(self.package)
                                .unwrap_or(self.package.name.as_str()),
                            name,
                        )
                    } else {
                        format!("{}-{}", self.package, name)
                    };
                    ret.push((None, RuleRef::local(dep_name).with_platform(platform), None))
                }
                if let BuildscriptFixup::PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup {
                    add_dep: true,
                    name,
                    static_libs,
                    public,
                    ..
                }) = buildscript
                {
                    let libs = self
                        .manifestwalk(static_libs, Vec::<String>::new(), self.config.strict_globs)
                        .context("Prebuilt C++ libraries")?;
                    for static_lib in libs {
                        let dep_name = if *public {
                            format!(
                                "{}-{}-{}",
                                self.index
                                    .public_alias(self.package)
                                    .unwrap_or(self.package.name.as_str()),
                                name,
                                static_lib.file_name().unwrap().to_string_lossy()
                            )
                        } else {
                            format!(
                                "{}-{}-{}",
                                self.package,
                                name,
                                static_lib.file_name().unwrap().to_string_lossy()
                            )
                        };
                        ret.push((None, RuleRef::local(dep_name).with_platform(platform), None))
                    }
                }
            }
        }

        Ok(ret)
    }

    /// Additional environment
    pub fn compute_env(&self) -> Vec<(Option<PlatformExpr>, BTreeMap<String, String>)> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut map = config.env.clone();

            if config.cargo_env {
                map.extend(vec![
                    (
                        "CARGO_MANIFEST_DIR".to_string(),
                        relative_path(&self.cell_dir, self.manifest_dir)
                            .display()
                            .to_string(),
                    ),
                    (
                        "CARGO_PKG_DESCRIPTION".to_string(),
                        self.package.description.clone().unwrap_or_default(),
                    ),
                    (
                        "CARGO_PKG_VERSION".to_string(),
                        self.package.version.to_string(),
                    ),
                    (
                        "CARGO_PKG_VERSION_MAJOR".to_string(),
                        self.package.version.major.to_string(),
                    ),
                    (
                        "CARGO_PKG_VERSION_MINOR".to_string(),
                        self.package.version.minor.to_string(),
                    ),
                    (
                        "CARGO_PKG_VERSION_PATCH".to_string(),
                        self.package.version.patch.to_string(),
                    ),
                    ("CARGO_PKG_NAME".to_string(), self.package.name.clone()),
                ]);
            }

            if !map.is_empty() {
                ret.push((platform.cloned(), map));
            }
        }
        ret
    }

    /// Given a glob for the srcs, walk the filesystem to get the full set. `srcglob` is
    /// the normal source glob rooted at the package's manifest dir.
    pub fn compute_srcs(
        &self,
        srcs: Vec<PathBuf>,
    ) -> Result<Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)>> {
        let mut ret: Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)> = vec![];

        let manifest_rel = relative_path(&self.third_party_dir, self.manifest_dir);

        let tp_rel_extra_src_fn = |srcs: &[String]| {
            srcs.iter()
                .map(|src| self.manifest_dir.join(src))
                .map(|src| relative_path(&self.third_party_dir, &src))
                .map(|src| normalize_dotdot(&src))
                .map(|src| src.display().to_string())
                .collect::<Vec<String>>()
        };
        let tp_rel_srcs = srcs
            .iter()
            .map(|src| relative_path(&self.third_party_dir, src))
            .map(|src| src.display().to_string())
            .collect::<Vec<String>>();
        let tp_rel_extra_srcs: Vec<_> = self
            .fixup_config
            .base(&self.package.version)
            .into_iter()
            .flat_map(|base| tp_rel_extra_src_fn(&base.extra_srcs))
            .collect();

        log::info!(
            "pkg {}, srcs {:?}, manifest_rel {}",
            self.package,
            srcs,
            manifest_rel.display()
        );

        // Do any platforms have an overlay? If so, the srcs are per-platform
        let has_platform_overlay = self
            .fixup_config
            .configs(&self.package.version)
            .filter(|(platform, _)| platform.is_some())
            .any(|(_, config)| config.overlay.is_some());

        // List of directories that srcs may be found in
        let mut common_src_dirs: Vec<PathBuf> = vec![self.manifest_dir.to_path_buf()];
        if let Some(base) = self.fixup_config.base(&self.package.version) {
            common_src_dirs.extend(
                base.src_referenced_dirs
                    .iter()
                    .map(|path| self.manifest_dir.join(&path))
                    .map(|path| normalize_dotdot(&path)),
            );
        }

        let mut common_files = HashSet::new();
        // Base sources are not required because they're either computed
        // precisely or a random guess of globs.
        common_files.extend(
            self::globwalk(
                &self.third_party_dir,
                common_src_dirs.iter(),
                tp_rel_srcs.iter(),
                None::<&str>,
                false, /* strict_globs */
            )
            .context("Srcs")?,
        );

        // Fixup-specified extra srcs are required (to avoid errors)
        common_files.extend(
            self::globwalk(
                &self.third_party_dir,
                common_src_dirs.iter(),
                tp_rel_extra_srcs.iter(),
                None::<&str>,
                self.config.strict_globs,
            )
            .context("Extra srcs")?,
        );

        let common_overlay_files = match self.fixup_config.base(&self.package.version) {
            Some(base) => base.overlay_files(&self.fixup_dir)?,
            None => HashSet::default(),
        };
        if !has_platform_overlay {
            let mut set = BTreeSet::new();

            for file in &common_files {
                if !common_overlay_files.contains(&relative_path(&manifest_rel, file)) {
                    set.insert(file.clone());
                }
            }

            ret.push((None, set));
        }

        for (platform, config) in self.fixup_config.platform_configs(&self.package.version) {
            let mut set = BTreeSet::new();

            // Platform-specific sources
            let mut platform_src_dirs: Vec<PathBuf> = vec![self.manifest_dir.to_path_buf()];
            platform_src_dirs.extend(
                config
                    .src_referenced_dirs
                    .iter()
                    .map(|path| self.manifest_dir.join(path))
                    .map(|path| normalize_dotdot(&path))
                    .collect::<Vec<PathBuf>>(),
            );
            let mut files: HashSet<_> = self::globwalk(
                &self.third_party_dir,
                platform_src_dirs.iter(),
                tp_rel_extra_src_fn(&config.extra_srcs),
                None::<&str>,
                self.config.strict_globs,
            )
            .context("Platform-specific extra srcs")?
            .collect();

            let mut overlay_files = config.overlay_files(&self.fixup_dir)?;

            // If any platform has its own overlay, then we need to treat all sources
            // as platform-specific to handle any collisions.
            if has_platform_overlay {
                overlay_files.extend(common_overlay_files.clone());
                files.extend(common_files.clone());
            }

            for file in files {
                if !overlay_files.contains(&relative_path(&manifest_rel, &file)) {
                    set.insert(file);
                }
            }

            if !set.is_empty() {
                ret.push((Some(platform.clone()), set));
            }
        }

        log::debug!(
            "pkg {}, srcs {:?}, manifest_rel {} => {:#?}",
            self.package,
            srcs,
            manifest_rel.display(),
            ret
        );

        Ok(ret)
    }

    pub fn compute_mapped_srcs(
        &self,
    ) -> Result<Vec<(Option<PlatformExpr>, BTreeMap<PathBuf, PathBuf>)>> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut map = BTreeMap::new();

            for (k, v) in &config.extra_mapped_srcs {
                map.insert(
                    relative_path(&self.third_party_dir, &self.manifest_dir.join(k)),
                    relative_path(&self.third_party_dir, &self.manifest_dir.join(v)),
                );
            }

            if let Some(overlay) = &config.overlay {
                let overlay_dir = self.fixup_dir.join(overlay);
                let overlay_files = config.overlay_files(&self.fixup_dir)?;

                log::debug!(
                    "pkg {} target {} overlay_dir {} overlay_files {:?}",
                    self.package,
                    self.target.name,
                    overlay_dir.display(),
                    overlay_files
                );

                for file in overlay_files {
                    map.insert(
                        relative_path(&self.third_party_dir, &overlay_dir.join(&file)),
                        relative_path(&self.third_party_dir, &self.manifest_dir.join(&file)),
                    );
                }
            }
            if !map.is_empty() {
                ret.push((platform.cloned(), map));
            }
        }

        Ok(ret)
    }

    /// Return mapping from rules of generated source to local name.
    pub fn compute_gen_srcs(
        &self,
        srcdir: &Path,
    ) -> Vec<(Option<PlatformExpr>, BTreeMap<RuleRef, PathBuf>)> {
        let mut ret = vec![];

        if self.buildscript_rule_name().is_none() {
            // No generated sources
            return ret;
        }

        let srcdir = relative_path(&self.third_party_dir, &self.manifest_dir.join(srcdir));

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut map = BTreeMap::new();

            for fix in &config.buildscript {
                if !self.target_match(fix) {
                    continue;
                }
                if let BuildscriptFixup::GenSrcs(GenSrcs { files, mapped, .. }) = fix {
                    for file in files {
                        map.insert(
                            RuleRef::local(self.buildscript_gen_srcs_rulename(Some(file))),
                            srcdir.join(file),
                        );
                    }
                    for (file, path) in mapped {
                        map.insert(
                            RuleRef::local(self.buildscript_gen_srcs_rulename(Some(file))),
                            srcdir.join(path),
                        );
                    }
                }
            }
            if !map.is_empty() {
                ret.push((platform.cloned(), map));
            }
        }

        ret
    }

    /// Compute linkage
    pub fn compute_link_style(&self) -> Vec<(Option<PlatformExpr>, String)> {
        let mut ret = Vec::new();
        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            if let Some(link_style) = config.link_style.as_ref() {
                ret.push((platform.cloned(), link_style.clone()));
            }
        }

        ret
    }

    /// Walk from package manifest dir matching a set of globs (which can be literal names).
    /// The globs are relative to the manifest dir. Results are relative to third_party_dir.
    pub fn manifestwalk<'a>(
        &'a self,
        globs: impl IntoIterator<Item = impl AsRef<str> + 'a> + 'a,
        excepts: impl IntoIterator<Item = impl AsRef<str> + 'a> + 'a,
        strict_match: bool,
    ) -> Result<impl Iterator<Item = PathBuf> + 'a> {
        let rel_manifest = relative_path(&self.third_party_dir, self.manifest_dir);

        let rel_glob = move |glob: &str| {
            if rel_manifest.components().next().is_none() {
                glob.to_string()
            } else {
                format!("{}/{}", rel_manifest.display(), glob)
            }
        };

        self::globwalk(
            &self.third_party_dir,
            iter::once(self.manifest_dir),
            globs.into_iter().map({
                let rel_glob = rel_glob.clone();
                move |glob| rel_glob(glob.as_ref())
            }),
            excepts.into_iter().map(move |glob| rel_glob(glob.as_ref())),
            strict_match,
        )
    }
}

/// Walk from package manifest dir matching a set of globs (which can be literal names).
/// Parameters:
/// `base` - base directory for everything. All globs and dirs are relative to this, as are resulting paths
/// `dirs` - starting directories for walk, relative to base.
/// `globs` - set of globs to match
/// `excepts` - globs to exclude from output
/// `strict_match` - require all globs to be matched (even if they're later excluded by `excepts`)
fn globwalk<'a>(
    base: impl Into<PathBuf> + 'a,
    dirs: impl IntoIterator<Item = impl AsRef<Path> + 'a> + 'a,
    globs: impl IntoIterator<Item = impl AsRef<str>>,
    excepts: impl IntoIterator<Item = impl AsRef<str>>,
    strict_match: bool,
) -> Result<impl Iterator<Item = PathBuf> + 'a> {
    let base = base.into();

    let mut fullglobs = Vec::new();

    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let fullglob = format!("{}/{}", base.display(), glob.as_ref());
        builder.add(
            GlobBuilder::new(&fullglob)
                .literal_separator(true)
                .build()?,
        );
        fullglobs.push(fullglob);
    }
    let globset = builder.build()?;

    let mut builder = GlobSetBuilder::new();
    for except in excepts {
        let fullexcept = format!("{}/{}", base.display(), except.as_ref());
        builder.add(
            GlobBuilder::new(&fullexcept)
                .literal_separator(true)
                .build()?,
        );
    }
    let exceptset = builder.build()?;

    let mut globs_used = HashSet::new();

    let res: Vec<_> = dirs
        .into_iter()
        .flat_map({
            let base = base.clone();
            move |dir| WalkDir::new(base.join(dir.as_ref())).into_iter()
        })
        .filter_map(|res| res.ok())
        .filter(|entry| !entry.file_type().is_dir())
        .filter(|entry| -> bool {
            let matches = globset.matches(entry.path());
            let found = !matches.is_empty();
            globs_used.extend(matches);
            found && !exceptset.is_match(entry.path())
        })
        .map(|entry| relative_path(&base, entry.path()))
        .collect();

    if strict_match && globs_used.len() != globset.len() {
        let mut unmatched = Vec::new();
        for (idx, fglob) in fullglobs.into_iter().enumerate() {
            if !globs_used.contains(&idx) {
                unmatched.push(fglob);
            }
        }
        bail!("Unmatched globs: {:?}", unmatched);
    }

    Ok(res.into_iter())
}
