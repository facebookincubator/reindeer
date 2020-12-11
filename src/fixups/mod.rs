/*
 * Copyright (c) Facebook, Inc. and its affiliates.
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

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobSetBuilder};

use walkdir::WalkDir;

use crate::{
    buck::{
        self, BuildscriptGenrule, BuildscriptGenruleFilter, BuildscriptGenruleSrcs, Common, Rule,
        RuleRef, RustBinary,
    },
    buckify::relative_path,
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
            let fixup =
                FixupConfigFile::template(&paths.third_party_dir, &index, &package, &target);
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
            manifest_dir: package.manifest_dir(),
            index,
            package,
            target,
            fixup_dir,
            fixup_config,
            config,
        })
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
    pub fn emit_buildscript_rules(&self, buildscript: RustBinary) -> Result<Vec<Rule>> {
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
        for (plat, _fixup) in self.fixup_config.configs() {
            match plat {
                None => features.extend(buildscript.common.base.features.iter().cloned()),
                Some(expr) => {
                    let platnames = platform_names_for_expr(&self.config, expr)?;
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
            .configs()
            .flat_map(|(_platform, fixup)| fixup.buildscript.iter());

        for fix in fixes {
            match fix {
                // Just emit a build rule for it, but don't otherwise do anything
                BuildscriptFixup::Build => res.push(Rule::Binary(buildscript.clone())),

                // Build and run it, and filter the output for --cfg options
                // for the main target's rustc command line
                BuildscriptFixup::RustcFlags(RustcFlags { env }) => {
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
                }) => {
                    // Emit the build script itself
                    res.push(Rule::Binary(buildscript.clone()));

                    let srcs = self
                        .manifestwalk(input_srcs.iter().map(String::as_str), iter::empty::<&str>())?
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
                    add_dep: _,
                }) => {
                    let rule = buck::CxxLibrary {
                        common: Common {
                            name: if *public {
                                format!(
                                    "{}-{}",
                                    self.index
                                        .public_alias(&self.package)
                                        .unwrap_or(self.package.name.as_str()),
                                    name
                                )
                            } else {
                                format!("{}-{}", self.package, name)
                            },
                            public: *public,
                            licenses: Default::default(),
                        },
                        // Just collect the sources, excluding things in the exclude list
                        srcs: self.manifestwalk(srcs, exclude)?.collect(),
                        // Collect the nominated headers, plus everything in the fixup include
                        // path(s).
                        headers: self::globwalk(
                            &self.third_party_dir,
                            &[&rel_manifest, &rel_fixup],
                            headers
                                .iter()
                                .map(|path| format!("{}/{}", rel_manifest.display(), path))
                                .chain(fixup_include_paths.iter().map(|path| {
                                    format!("{}/**/*.h", rel_fixup.join(path).display())
                                })),
                            exclude
                                .iter()
                                .map(|path| format!("{}/{}", rel_manifest.display(), path)),
                        )?
                        .collect(),
                        exported_headers: match exported_headers {
                            SetOrMap::Set(exported_headers) => SetOrMap::Set(
                                self.manifestwalk(exported_headers, exclude)?.collect(),
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
                    add_dep: _,
                }) => {
                    let libs = self.manifestwalk(static_libs, Vec::<String>::new())?;
                    for static_lib in libs {
                        let rule = buck::PrebuiltCxxLibrary {
                            common: Common {
                                name: if *public {
                                    format!(
                                        "{}-{}-{}",
                                        self.index
                                            .public_alias(&self.package)
                                            .unwrap_or_else(|| self.package.name.as_str()),
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
                            },
                            static_lib: static_lib.clone(),
                        };
                        res.push(Rule::PrebuiltCxxLibrary(rule));
                    }
                }

                // Complain and omit
                BuildscriptFixup::Unresolved(msg) => {
                    log::warn!(
                        "{} has a build script, but I don't know what to do with it: {}",
                        self.package,
                        msg
                    );
                }
            }
        }

        Ok(res)
    }

    /// Return the set of features to enable, which is the union of the cargo-reso=lved ones
    /// and additional ones defined in the fixup.
    pub fn compute_features(&self) -> Vec<(Option<PlatformExpr>, BTreeSet<String>)> {
        let mut ret = vec![];

        // All omit_features from all configs remove from the
        // base (platform-independent) features.
        let mut omits = HashSet::new();
        for (_plat, fixup) in self.fixup_config.configs() {
            omits.extend(fixup.omit_features.iter().map(String::as_str));
        }

        for (platform, config) in self.fixup_config.configs() {
            let mut set = BTreeSet::new();

            if platform.is_none() {
                for feature in self.index.resolved_features(self.package) {
                    if !omits.contains(feature) {
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

        for (platform, config) in self.fixup_config.configs() {
            let mut flags = vec![];

            for buildscript in &config.buildscript {
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

        for (platform, config) in self.fixup_config.configs() {
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
        for (platform, fixup) in self.fixup_config.configs() {
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
                .map(|tgt| tgt.name.replace("-", "_"));

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

        for (platform, config) in self.fixup_config.configs() {
            ret.extend(config.extra_deps.iter().map(|dep| {
                (
                    None,
                    RuleRef::abs(dep.to_string()).with_platform(platform),
                    None,
                )
            }));
            for buildscript in &config.buildscript {
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
                                .public_alias(&self.package)
                                .unwrap_or_else(|| self.package.name.as_str()),
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
                }) = buildscript
                {
                    let libs = self
                        .manifestwalk(static_libs, Vec::<String>::new())
                        .unwrap();
                    for static_lib in libs {
                        let dep_name = if *public {
                            format!(
                                "{}-{}-{}",
                                self.index
                                    .public_alias(&self.package)
                                    .unwrap_or_else(|| self.package.name.as_str()),
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

        for (platform, config) in self.fixup_config.configs() {
            let mut map = config.env.clone();

            if config.cargo_env {
                map.extend(vec![
                    (
                        "CARGO_MANIFEST_DIR".to_string(),
                        // XXX FIXME - make this relative to the Buck cell root
                        relative_path(&self.third_party_dir, self.manifest_dir)
                            .display()
                            .to_string(),
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
        srcs: Vec<String>,
    ) -> Result<Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)>> {
        let mut ret: Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)> = vec![];

        let manifest_rel = relative_path(&self.third_party_dir, self.manifest_dir);

        log::info!(
            "pkg {}, srcs {:?}, manifest_rel {}",
            self.package,
            srcs,
            manifest_rel.display()
        );

        // Do any platforms have an overlay? If so, the srcs are per-platform
        let has_platform_overlay = self
            .fixup_config
            .configs()
            .filter(|(platform, _)| platform.is_some())
            .any(|(_, config)| config.overlay.is_some());

        let common_files: HashSet<_> = self
            .manifestwalk(
                self.fixup_config
                    .base()
                    .extra_srcs
                    .iter()
                    .chain(srcs.iter())
                    .map(String::as_str),
                None::<&str>,
            )?
            .collect();

        let common_overlay_files = self.fixup_config.base().overlay_files(&self.fixup_dir)?;
        if !has_platform_overlay {
            let mut set = BTreeSet::new();

            for file in &common_files {
                if !common_overlay_files.contains(&relative_path(&manifest_rel, file)) {
                    set.insert(file.clone());
                }
            }

            ret.push((None, set));
        }

        for (platform, config) in self.fixup_config.platform_configs() {
            let mut set = BTreeSet::new();

            // Platform-specific sources
            let mut files: HashSet<_> = self
                .manifestwalk(config.extra_srcs.iter().map(String::as_str), None::<&str>)?
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

        for (platform, config) in self.fixup_config.configs() {
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

        if self.package.dependency_target() != Some(&self.target)
            || self.buildscript_rule_name().is_none()
        {
            // No generated sources
            return ret;
        }

        let srcdir = relative_path(&self.third_party_dir, &self.manifest_dir.join(srcdir));

        for (platform, config) in self.fixup_config.configs() {
            let mut map = BTreeMap::new();

            for fix in &config.buildscript {
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

    /// Walk from package manifest dir matching a set of globs (which can be literal names).
    /// The globs are relative to the manifest dir. Results are relative to third_party_dir.
    pub fn manifestwalk<'a>(
        &'a self,
        globs: impl IntoIterator<Item = impl AsRef<str> + 'a> + 'a,
        excepts: impl IntoIterator<Item = impl AsRef<str> + 'a> + 'a,
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
        )
    }
}

/// Walk from package manifest dir matching a set of globs (which can be literal names).
/// Parameters:
/// `base` - base directory for everything. All globs and dirs are relative to this, as are resulting paths
/// `dirs` - starting directories for walk, relative to base.
/// `globs` - set of globs to match
/// `exclude` - globs to exclude from output
fn globwalk<'a>(
    base: impl Into<PathBuf> + 'a,
    dirs: impl IntoIterator<Item = impl AsRef<Path> + 'a> + 'a,
    globs: impl IntoIterator<Item = impl AsRef<str>>,
    excepts: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<impl Iterator<Item = PathBuf> + 'a> {
    let base = base.into();

    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        let fullglob = format!("{}/{}", base.display(), glob.as_ref());
        builder.add(
            GlobBuilder::new(&fullglob)
                .literal_separator(true)
                .build()?,
        );
    }
    let globs = builder.build()?;

    let mut builder = GlobSetBuilder::new();
    for except in excepts {
        let fullexcept = format!("{}/{}", base.display(), except.as_ref());
        builder.add(
            GlobBuilder::new(&fullexcept)
                .literal_separator(true)
                .build()?,
        );
    }
    let excepts = builder.build()?;

    let res = dirs
        .into_iter()
        .flat_map({
            let base = base.clone();
            move |dir| WalkDir::new(base.join(dir.as_ref())).into_iter()
        })
        .filter_map(|res| res.ok())
        .filter(move |entry| globs.is_match(entry.path()) && !excepts.is_match(entry.path()))
        .map(move |entry| relative_path(&base, entry.path()));

    Ok(res)
}
