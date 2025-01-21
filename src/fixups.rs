/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Per-package configuration information

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;

use crate::Paths;
use crate::buck;
use crate::buck::Alias;
use crate::buck::BuckPath;
use crate::buck::BuildscriptGenrule;
use crate::buck::Common;
use crate::buck::Name;
use crate::buck::Rule;
use crate::buck::RuleRef;
use crate::buck::RustBinary;
use crate::buck::StringOrPath;
use crate::buck::Subtarget;
use crate::buck::SubtargetOrPath;
use crate::buck::Visibility;
use crate::buckify::normalize_path;
use crate::buckify::relative_path;
use crate::buckify::short_name_for_git_repo;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::NodeDepKind;
use crate::cargo::Source;
use crate::collection::SetOrMap;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::glob::Globs;
use crate::glob::NO_EXCLUDE;
use crate::glob::SerializableGlobSet as GlobSet;
use crate::index::Index;
use crate::index::ResolvedDep;
use crate::platform::PlatformExpr;
use crate::platform::PlatformPredicate;
use crate::platform::platform_names_for_expr;

mod buildscript;
mod config;

use buildscript::BuildscriptFixup;
use buildscript::CxxLibraryFixup;
use buildscript::GenSrcs;
use buildscript::PrebuiltCxxLibraryFixup;
use buildscript::RustcFlags;
use config::CargoEnv;
pub use config::ExportSources;
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
        will_use_rules: bool,
    ) -> anyhow::Result<Self> {
        let fixup_dir = paths.third_party_dir.join("fixups").join(&package.name);
        let fixup_path = fixup_dir.join("fixups.toml");

        let fixup_config: FixupConfigFile = if let Ok(file) = fs::read_to_string(&fixup_path) {
            log::debug!("read fixups from {}", fixup_path.display());
            toml::from_str(&file).context(format!("Failed to parse {}", fixup_path.display()))?
        } else {
            log::debug!("no fixups at {}", fixup_path.display());
            let fixup = FixupConfigFile::template(&paths.third_party_dir, target);
            // will_use_rules: avoid writing for e.g. `include_workspace_members = false`
            if config.fixup_templates && target.kind_custom_build() && will_use_rules {
                log::debug!(
                    "Writing template for {} to {}",
                    package,
                    relative_path(&paths.third_party_dir, &fixup_path).display()
                );
                log::debug!(
                    "fixup template: {:#?}, path {}",
                    fixup,
                    fixup_path.display()
                );

                let file = toml::to_string_pretty(&fixup)?;
                fs::create_dir_all(fixup_path.parent().unwrap())?;
                fs::write(&fixup_path, file)?;
            }
            fixup
        };

        if fixup_config.custom_visibility.is_some() && !index.is_public_package_name(&package.name)
        {
            return Err(anyhow!(
                "only public packages can have a fixup `visibility`."
            ))
            .with_context(|| format!("package {package} is private."));
        }

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

    fn subtarget_or_path(
        &self,
        relative_to_manifest_dir: &Path,
    ) -> anyhow::Result<SubtargetOrPath> {
        if matches!(self.config.vendor, VendorConfig::Source(_))
            || matches!(self.package.source, Source::Local)
        {
            // Path to vendored file looks like "vendor/foo-1.0.0/src/lib.rs"
            let manifest_dir = relative_path(&self.third_party_dir, self.manifest_dir);
            let path = manifest_dir.join(relative_to_manifest_dir);
            Ok(SubtargetOrPath::Path(BuckPath(path)))
        } else if let Source::Git { .. } = &self.package.source {
            // This is unsupported because git_fetch doesn't support subtargets.
            Err(anyhow!(
                "git dependencies in non-vendor mode cannot have subtargets"
            ))
            .with_context(|| format!("package {} is a git depedency", self.package))
        } else {
            // Subtarget inside an http_archive: ":foo-1.0.0.crate[src/lib.rs]"
            Ok(SubtargetOrPath::Subtarget(Subtarget {
                target: Name(format!(
                    "{}-{}.crate",
                    self.package.name, self.package.version
                )),
                relative: BuckPath(relative_to_manifest_dir.to_owned()),
            }))
        }
    }

    pub fn public_visibility(&self) -> Visibility {
        match self.fixup_config.custom_visibility.as_deref() {
            Some(visibility) => Visibility::Custom(visibility.to_vec()),
            None => Visibility::Public,
        }
    }

    pub fn python_ext(&self) -> Option<&str> {
        self.fixup_config.python_ext.as_deref()
    }

    pub fn omit_target(&self) -> bool {
        self.fixup_config.omit_targets.contains(&self.target.name)
    }

    pub fn export_sources(&self) -> Option<&ExportSources> {
        self.fixup_config.export_sources.as_ref()
    }

    pub fn precise_srcs(&self) -> bool {
        self.fixup_config
            .precise_srcs
            .unwrap_or(self.config.precise_srcs)
    }

    fn buildscript_target(&self) -> Option<&ManifestTarget> {
        self.package
            .targets
            .iter()
            .find(|tgt| tgt.kind_custom_build())
    }

    pub fn buildscript_genrule_name(&self) -> Name {
        let mut name = self.buildscript_rule_name().expect("no buildscript");
        // Cargo names the build script target after the filename of the build
        // script's crate root. In the overwhelmingly common case of a build
        // script located in build.rs, this is build-script-build, and we name
        // the genrule build-script-run. Some crates such as typenum have a
        // build/main.rs such that the build script target ends up being named
        // build-script-main, and in this case we use build-script-main-run for
        // the genrule.
        if name.0.ends_with("-build-script-build") {
            name.0.truncate(name.0.len() - 6);
        }
        name.0.push_str("-run");
        name
    }

    fn buildscript_rule_name(&self) -> Option<Name> {
        self.buildscript_target()
            .map(|tgt| Name(format!("{}-{}", self.package, tgt.name)))
    }

    /// Return buildscript-related rules
    /// The rules may be platform specific, but they're emitted unconditionally. (The
    /// dependencies referencing them are conditional).
    pub fn emit_buildscript_rules(
        &self,
        buildscript: RustBinary,
        config: &'meta Config,
    ) -> anyhow::Result<Vec<Rule>> {
        let mut res = Vec::new();

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
                None => features.extend(
                    buildscript
                        .common
                        .base
                        .features
                        .unwrap_ref()
                        .iter()
                        .cloned(),
                ),
                Some(expr) => {
                    let platnames = platform_names_for_expr(self.config, expr)?;
                    for platname in platnames {
                        if let Some(platattr) = buildscript.common.platform.get(platname) {
                            features.extend(platattr.features.unwrap_ref().iter().cloned())
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

        let mut buildscript_run = None;
        let default_genrule = || BuildscriptGenrule {
            name: self.buildscript_genrule_name(),
            buildscript_rule: buildscript_rule_name.clone(),
            package_name: self.package.name.clone(),
            version: self.package.version.clone(),
            features: buck::Selectable::Value(features.clone()),
            env: BTreeMap::new(),
        };

        for fix in fixes {
            if !matches!(self.config.vendor, VendorConfig::Source(_)) {
                if let Source::Git { repo, .. } = &self.package.source {
                    // Cxx_library fixups only work if the sources are vendored
                    // or from an http_archive. They do not work with sources
                    // from git_fetch, because we do not currently have a way to
                    // generate subtargets referring to the git repo contents:
                    // e.g. "briansmith/ring[crypto/crypto.c]"
                    if let Some(unsupported_fixup_kind) = match fix {
                        BuildscriptFixup::CxxLibrary(_) => Some("buildscript.cxx_library"),
                        BuildscriptFixup::PrebuiltCxxLibrary(_) => {
                            Some("buildscript.prebuilt_cxx_library")
                        }
                        _ => None,
                    } {
                        bail!(
                            "{} fixup is not supported in vendor=false mode for crates that come from a git repo: {}",
                            unsupported_fixup_kind,
                            repo,
                        );
                    }
                }
            }

            match fix {
                // Build and run it, and filter the output for --cfg options
                // for the main target's rustc command line
                BuildscriptFixup::RustcFlags(RustcFlags { env, .. }) => {
                    // Emit the build script itself
                    res.push(Rule::BuildscriptBinary(buildscript.clone()));

                    // Emit rule to get its stdout and filter it into args
                    let buildscript_run = buildscript_run.get_or_insert_with(default_genrule);
                    buildscript_run.env.extend(env.clone());
                }

                // Generated source files - given a list, set up rules to extract them from
                // the buildscript.
                BuildscriptFixup::GenSrcs(GenSrcs { env, .. }) => {
                    // Emit the build script itself
                    res.push(Rule::BuildscriptBinary(buildscript.clone()));

                    // Emit rules to extract generated sources
                    let buildscript_run = buildscript_run.get_or_insert_with(default_genrule);
                    buildscript_run.env.extend(env.clone());
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
                    preferred_linkage,
                    undefined_symbols,
                    ..
                }) => {
                    let actual = Name(format!(
                        "{}-{}",
                        self.index.private_rule_name(self.package),
                        name,
                    ));

                    if *public {
                        let rule = Rule::Alias(Alias {
                            name: Name(format!(
                                "{}-{}",
                                self.index.public_rule_name(self.package),
                                name,
                            )),
                            actual: actual.clone(),
                            visibility: self.public_visibility(),
                        });
                        res.push(rule);
                    }

                    let rule = buck::CxxLibrary {
                        common: Common {
                            name: actual,
                            visibility: Visibility::Private,
                            licenses: Default::default(),
                            compatible_with: compatible_with
                                .iter()
                                .cloned()
                                .map(RuleRef::new)
                                .collect(),
                        },
                        // Just collect the sources, excluding things in the exclude list
                        srcs: {
                            let mut globs = Globs::new(srcs, exclude).context("C++ sources")?;
                            let srcs = globs
                                .walk(self.manifest_dir)
                                .map(|path| self.subtarget_or_path(&path))
                                .collect::<anyhow::Result<_>>()?;
                            if self.config.strict_globs {
                                globs.check_all_globs_used()?;
                            }
                            srcs
                        },
                        // Collect the nominated headers, plus everything in the fixup include
                        // path(s).
                        headers: {
                            let mut globs = Globs::new(headers, exclude)?;
                            let mut headers = BTreeSet::new();
                            for path in globs.walk(self.manifest_dir) {
                                headers.insert(self.subtarget_or_path(&path)?);
                            }

                            let mut globs = Globs::new(["**/*.asm", "**/*.h"], NO_EXCLUDE)?;
                            for fixup_include_path in fixup_include_paths {
                                for path in globs.walk(self.fixup_dir.join(fixup_include_path)) {
                                    headers.insert(SubtargetOrPath::Path(BuckPath(
                                        rel_fixup.join(fixup_include_path).join(path),
                                    )));
                                }
                            }

                            headers
                        },
                        exported_headers: match exported_headers {
                            SetOrMap::Set(exported_headers) => {
                                let mut exported_header_globs =
                                    Globs::new(exported_headers, exclude)
                                        .context("C++ exported headers")?;
                                let exported_headers = exported_header_globs
                                    .walk(self.manifest_dir)
                                    .map(|path| self.subtarget_or_path(&path))
                                    .collect::<anyhow::Result<_>>()?;
                                if self.config.strict_globs {
                                    exported_header_globs.check_all_globs_used()?;
                                }
                                SetOrMap::Set(exported_headers)
                            }
                            SetOrMap::Map(exported_headers) => SetOrMap::Map(
                                exported_headers
                                    .iter()
                                    .map(|(name, path)| {
                                        Ok((name.clone(), self.subtarget_or_path(Path::new(path))?))
                                    })
                                    .collect::<anyhow::Result<_>>()?,
                            ),
                        },
                        include_directories: fixup_include_paths
                            .iter()
                            .map(|path| Ok(SubtargetOrPath::Path(BuckPath(rel_fixup.join(path)))))
                            .chain(
                                include_paths
                                    .iter()
                                    .map(|path| self.subtarget_or_path(path)),
                            )
                            .collect::<anyhow::Result<_>>()?,
                        compiler_flags: compiler_flags.clone(),
                        preprocessor_flags: preprocessor_flags.clone(),
                        header_namespace: header_namespace.clone(),
                        deps: deps.iter().cloned().map(RuleRef::new).collect(),
                        preferred_linkage: preferred_linkage.clone(),
                        undefined_symbols: undefined_symbols.clone(),
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
                    let mut static_lib_globs =
                        Globs::new(static_libs, NO_EXCLUDE).context("Static libraries")?;
                    for static_lib in static_lib_globs.walk(self.manifest_dir) {
                        let actual = Name(format!(
                            "{}-{}-{}",
                            self.index.private_rule_name(self.package),
                            name,
                            static_lib.file_name().unwrap().to_string_lossy(),
                        ));

                        if *public {
                            let rule = Rule::Alias(Alias {
                                name: Name(format!(
                                    "{}-{}-{}",
                                    self.index.public_rule_name(self.package),
                                    name,
                                    static_lib.file_name().unwrap().to_string_lossy(),
                                )),
                                actual: actual.clone(),
                                visibility: self.public_visibility(),
                            });
                            res.push(rule);
                        }

                        let rule = buck::PrebuiltCxxLibrary {
                            common: Common {
                                name: actual,
                                visibility: Visibility::Private,
                                licenses: Default::default(),
                                compatible_with: compatible_with
                                    .iter()
                                    .cloned()
                                    .map(RuleRef::new)
                                    .collect(),
                            },
                            static_lib: self.subtarget_or_path(&static_lib)?,
                        };
                        res.push(Rule::PrebuiltCxxLibrary(rule));
                    }
                    if self.config.strict_globs {
                        static_lib_globs.check_all_globs_used()?;
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
                            "Unresolved fix up errors, fix them and rerun buckify."
                        ));
                    } else {
                        log::warn!("{}", unresolved_package_msg);
                    }
                }
            }
        }

        if let Some(buildscript_run) = buildscript_run {
            res.push(Rule::BuildscriptGenrule(buildscript_run));
        }

        Ok(res)
    }

    /// Return the set of features to enable, which is the union of the cargo-resolved ones
    /// and additional ones defined in the fixup.
    pub fn compute_features(
        &self,
    ) -> anyhow::Result<HashMap<Option<PlatformExpr>, BTreeSet<String>>> {
        let mut ret = HashMap::new();

        let mut platform_omits = HashMap::new();
        for (platform, fixup) in self.fixup_config.configs(&self.package.version) {
            // Group by feature which platforms omit it.
            for feature in &fixup.omit_features {
                platform_omits
                    .entry(feature.as_str())
                    .or_insert_with(HashSet::new)
                    .insert(platform);
            }

            if !fixup.features.is_empty() {
                ret.entry(platform.cloned())
                    .or_insert_with(BTreeSet::new)
                    .extend(fixup.features.clone());
            }
        }

        for feature in self.index.resolved_features(self.package) {
            let Some(omitted_platforms) = platform_omits.get(feature) else {
                // Feature is unconditionally included on all platforms.
                ret.entry(None)
                    .or_insert_with(BTreeSet::new)
                    .insert(feature.to_owned());
                continue;
            };

            if omitted_platforms.contains(&None) {
                // Feature is unconditionally omitted on all platforms.
                continue;
            }

            let mut excludes = vec![];
            for platform in omitted_platforms {
                if let Some(platform_expr) = platform {
                    // If a platform filters a feature added by the base,
                    // we need to filter it from the base and add it to all
                    // other platforms. Create a predicate that excludes all
                    // filtered platforms. This will be the "all other
                    // platforms".
                    let platform_pred = PlatformPredicate::parse(platform_expr)?;
                    excludes.push(PlatformPredicate::Not(Box::new(platform_pred)));
                }
            }

            assert!(!excludes.is_empty());

            let platform_pred = PlatformPredicate::All(excludes);
            let platform_expr: PlatformExpr = format!("cfg({})", platform_pred).into();
            ret.entry(Some(platform_expr))
                .or_insert_with(BTreeSet::new)
                .insert(feature.to_owned());
        }

        Ok(ret)
    }

    fn buildscript_rustc_flags(
        &self,
    ) -> Vec<(
        Option<PlatformExpr>,
        (Vec<String>, BTreeMap<String, Vec<String>>),
    )> {
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
                        "@$(location :{}[rustc_flags])",
                        self.buildscript_genrule_name()
                    ))
                }
            }

            if !flags.is_empty() {
                ret.push((platform.cloned(), (flags, Default::default())));
            }
        }

        ret
    }

    /// Return extra command-line options, with platform annotation if needed
    pub fn compute_cmdline(
        &self,
    ) -> Vec<(
        Option<PlatformExpr>,
        (Vec<String>, BTreeMap<String, Vec<String>>),
    )> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut flags = vec![];

            flags.extend(config.rustc_flags.clone());
            flags.extend(config.cfgs.iter().map(|cfg| format!("--cfg={}", cfg)));
            let flags_select = config.rustc_flags_select.clone();

            if !flags.is_empty() || !flags_select.is_empty() {
                ret.push((platform.cloned(), (flags, flags_select)));
            }
        }

        ret.extend(self.buildscript_rustc_flags());

        ret
    }

    /// Generate the set of deps for the target. This could just return the unmodified
    /// depenedencies, or it could add/remove them. This returns the Buck rule reference
    /// and the corresponding package if there is one (so the caller can limit its enumeration
    /// to only targets which were actually used).
    pub fn compute_deps(
        &self,
    ) -> anyhow::Result<
        Vec<(
            Option<&'meta Manifest>,
            RuleRef,
            Option<&'meta str>,
            &'meta NodeDepKind,
        )>,
    > {
        let mut ret = vec![];

        let mut omits = HashMap::new();
        let mut all_omits = HashSet::new();
        // Pre-compute the list of all filtered dependencies. If a platform filters a dependency
        // added by the base, we need to filter it from the base and add it to all other platforms.
        for (platform, fixup) in self.fixup_config.configs(&self.package.version) {
            let platform_omits = omits.entry(platform).or_insert_with(HashSet::new);
            platform_omits.extend(fixup.omit_deps.iter().map(String::as_str));
            all_omits.extend(fixup.omit_deps.iter().map(String::as_str));
        }

        for ResolvedDep {
            package,
            platform,
            rename,
            dep_kind,
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
                rename,
                self.target.kind()
            );

            // Only use the rename if it isn't the same as the target anyway.
            let tgtname = package
                .dependency_target()
                .map(|tgt| tgt.name.replace('-', "_"));

            let original_rename = rename;

            let rename = match tgtname {
                Some(ref tgtname) if tgtname == rename => None,
                Some(_) | None => Some(rename),
            };

            if omits
                .get(&None)
                .is_some_and(|omits| omits.contains(original_rename))
            {
                // Dependency is unconditionally omitted on all platforms.
                continue;
            } else if all_omits.contains(original_rename) {
                // If the dependency is for a particular platform and that has it excluded,
                // skip it.
                if let Some(platform_omits) = omits.get(&platform.as_ref()) {
                    if platform_omits.contains(original_rename) {
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
                            if platform_omits.contains(original_rename) {
                                let platform_pred = PlatformPredicate::parse(platform_expr)?;
                                excludes.push(PlatformPredicate::Not(Box::new(platform_pred)));
                            }
                        }
                    }

                    let platform_pred = PlatformPredicate::All(excludes);
                    let platform_expr: PlatformExpr = format!("cfg({})", platform_pred).into();
                    ret.push((
                        Some(package),
                        RuleRef::from(self.index.private_rule_name(package))
                            .with_platform(Some(&platform_expr)),
                        rename,
                        dep_kind,
                    ));

                    // Since we've already added the platform-excluding rule, skip the generic rule
                    // adding below.
                    continue;
                }
            }

            // No filtering involved? Just insert it like normal.
            ret.push((
                Some(package),
                RuleRef::from(self.index.private_rule_name(package))
                    .with_platform(platform.as_ref()),
                rename,
                dep_kind,
            ))
        }

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            ret.extend(config.extra_deps.iter().map(|dep| {
                (
                    None,
                    RuleRef::new(dep.to_string()).with_platform(platform),
                    None,
                    &NodeDepKind::ORDINARY,
                )
            }));
            for buildscript in &config.buildscript {
                if !self.target_match(buildscript) {
                    continue;
                }
                if let BuildscriptFixup::CxxLibrary(CxxLibraryFixup {
                    add_dep: true,
                    name,
                    ..
                }) = buildscript
                {
                    ret.push((
                        None,
                        RuleRef::new(format!(
                            ":{}-{}",
                            self.index.private_rule_name(self.package),
                            name
                        ))
                        .with_platform(platform),
                        None,
                        &NodeDepKind::ORDINARY,
                    ));
                }
                if let BuildscriptFixup::PrebuiltCxxLibrary(PrebuiltCxxLibraryFixup {
                    add_dep: true,
                    name,
                    static_libs,
                    ..
                }) = buildscript
                {
                    let mut static_lib_globs =
                        Globs::new(static_libs, NO_EXCLUDE).context("Prebuilt C++ libraries")?;
                    for static_lib in static_lib_globs.walk(self.manifest_dir) {
                        ret.push((
                            None,
                            RuleRef::new(format!(
                                ":{}-{}-{}",
                                self.index.private_rule_name(self.package),
                                name,
                                static_lib.file_name().unwrap().to_string_lossy(),
                            ))
                            .with_platform(platform),
                            None,
                            &NodeDepKind::ORDINARY,
                        ));
                    }
                }
            }
        }

        Ok(ret)
    }

    /// Additional environment
    pub fn compute_env(
        &self,
    ) -> anyhow::Result<Vec<(Option<PlatformExpr>, BTreeMap<String, StringOrPath>)>> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut map: BTreeMap<String, StringOrPath> = config
                .env
                .iter()
                .map(|(k, v)| (k.clone(), StringOrPath::String(v.clone())))
                .collect();

            for cargo_env in config.cargo_env.iter() {
                let v = match cargo_env {
                    CargoEnv::CARGO_MANIFEST_DIR => {
                        if matches!(self.config.vendor, VendorConfig::Source(_))
                            || matches!(self.package.source, Source::Local)
                        {
                            StringOrPath::Path(BuckPath(relative_path(
                                &self.third_party_dir,
                                self.manifest_dir,
                            )))
                        } else if let VendorConfig::LocalRegistry = self.config.vendor {
                            StringOrPath::String(format!(
                                "{}-{}.crate",
                                self.package.name, self.package.version,
                            ))
                        } else if let Source::Git { repo, .. } = &self.package.source {
                            let short_name = short_name_for_git_repo(repo)?;
                            StringOrPath::String(short_name.to_owned())
                        } else {
                            StringOrPath::String(format!(
                                "{}-{}.crate",
                                self.package.name, self.package.version,
                            ))
                        }
                    }
                    CargoEnv::CARGO_PKG_AUTHORS => {
                        StringOrPath::String(self.package.authors.join(":"))
                    }
                    CargoEnv::CARGO_PKG_DESCRIPTION => {
                        StringOrPath::String(self.package.description.clone().unwrap_or_default())
                    }
                    CargoEnv::CARGO_PKG_REPOSITORY => {
                        StringOrPath::String(self.package.repository.clone().unwrap_or_default())
                    }
                    CargoEnv::CARGO_PKG_VERSION => {
                        StringOrPath::String(self.package.version.to_string())
                    }
                    CargoEnv::CARGO_PKG_VERSION_MAJOR => {
                        StringOrPath::String(self.package.version.major.to_string())
                    }
                    CargoEnv::CARGO_PKG_VERSION_MINOR => {
                        StringOrPath::String(self.package.version.minor.to_string())
                    }
                    CargoEnv::CARGO_PKG_VERSION_PATCH => {
                        StringOrPath::String(self.package.version.patch.to_string())
                    }
                    CargoEnv::CARGO_PKG_NAME => StringOrPath::String(self.package.name.clone()),
                };
                map.insert(cargo_env.to_string(), v);
            }

            if !map.is_empty() {
                ret.push((platform.cloned(), map));
            }
        }

        Ok(ret)
    }

    /// Given a glob for the srcs, walk the filesystem to get the full set.
    /// `srcs` is the normal source glob rooted at the package's manifest dir.
    pub fn compute_srcs(
        &self,
        srcs: Vec<PathBuf>,
    ) -> anyhow::Result<Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)>> {
        let mut ret: Vec<(Option<PlatformExpr>, BTreeSet<PathBuf>)> = vec![];

        // This function is only used in vendoring mode, so it's guaranteed that
        // manifest_dir is a subdirectory of third_party_dir.
        assert!(
            matches!(self.config.vendor, VendorConfig::Source(_))
                || matches!(self.package.source, Source::Local)
        );
        let manifest_rel = relative_path(&self.third_party_dir, self.manifest_dir);

        let srcs_globs: Vec<String> = srcs
            .iter()
            .map(|src| src.to_string_lossy().into_owned())
            .collect();

        log::debug!(
            "pkg {}, srcs {:?}, manifest_rel {}",
            self.package,
            srcs_globs,
            manifest_rel.display()
        );

        // Do any platforms have an overlay or platform-specific mapped srcs or
        // omitted sources? If so, the srcs are per-platform.
        let needs_per_platform_srcs =
            self.fixup_config
                .configs(&self.package.version)
                .any(|(platform, config)| {
                    platform.is_some()
                        && (config.overlay.is_some()
                            || !config.extra_mapped_srcs.is_empty()
                            || !config.omit_srcs.is_empty())
                });

        let mut common_files = HashSet::new();
        let mut srcs_globs = Globs::new(srcs_globs, NO_EXCLUDE).context("Srcs")?;
        for path in srcs_globs.walk(self.manifest_dir) {
            common_files.insert(manifest_rel.join(path));
        }
        if self.config.strict_globs {
            // Do not check srcs_globs.check_all_globs_used(). Base sources are
            // not required because they are either computed precisely or a
            // random guess of globs.
        }
        if let Some(base) = self.fixup_config.base(&self.package.version) {
            common_files.extend(self.compute_extra_srcs(&base.extra_srcs)?);
        }

        let no_omit_srcs;
        let (common_overlay_files, common_omit_srcs) =
            match self.fixup_config.base(&self.package.version) {
                Some(base) => (
                    base.overlay_and_mapped_files(&self.fixup_dir)?,
                    &base.omit_srcs,
                ),
                None => {
                    no_omit_srcs = GlobSet::default();
                    (HashSet::default(), &no_omit_srcs)
                }
            };

        if !needs_per_platform_srcs {
            let mut set = BTreeSet::new();

            for file in &common_files {
                let path_in_crate = relative_path(&manifest_rel, file);
                if !common_overlay_files.contains(&path_in_crate)
                    && !common_omit_srcs.is_match(&path_in_crate)
                {
                    set.insert(file.clone());
                }
            }

            ret.push((None, set));
        }

        for (platform, config) in self.fixup_config.platform_configs(&self.package.version) {
            let mut set = BTreeSet::new();

            let mut files = HashSet::new();
            files.extend(self.compute_extra_srcs(&config.extra_srcs)?);

            let mut overlay_files = config.overlay_and_mapped_files(&self.fixup_dir)?;

            // If any platform has its own overlay, then we need to treat all sources
            // as platform-specific to handle any collisions.
            if needs_per_platform_srcs {
                overlay_files.extend(common_overlay_files.clone());
                files.extend(common_files.clone());
            }

            for file in files {
                let path_in_crate = relative_path(&manifest_rel, &file);
                if !overlay_files.contains(&path_in_crate)
                    && !common_omit_srcs.is_match(&path_in_crate)
                    && !config.omit_srcs.is_match(&path_in_crate)
                {
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

    fn compute_extra_srcs(&self, globs: &[String]) -> anyhow::Result<HashSet<PathBuf>> {
        let mut extra_srcs = HashSet::new();
        let mut unmatched_globs = Vec::new();

        for glob in globs {
            // The extra_srcs are allowed to be located outside this crate's
            // manifest dir, i.e. starting with "../". For example libstd refers
            // to files from portable-simd and stdarch, which are located in
            // sibling directories. Thus doing a WalkDir over manifest_dir is
            // not sufficient; here we pick the right directory to walk for this
            // glob.
            let mut dir_containing_extra_srcs = self.manifest_dir.to_owned();
            let mut rest_of_glob = Path::new(glob).components();
            while let Some(component) = rest_of_glob.as_path().components().next() {
                if component.as_os_str().to_string_lossy().contains('*') {
                    // Ready to do globby stuff.
                    break;
                } else {
                    rest_of_glob.next().unwrap();
                    dir_containing_extra_srcs.push(component);
                }
            }

            let len_before = extra_srcs.len();
            let mut insert = |absolute_path: &Path| {
                let tp_rel_path = relative_path(&self.third_party_dir, absolute_path);
                extra_srcs.insert(normalize_path(&tp_rel_path));
            };

            let rest_of_glob = rest_of_glob.as_path();
            if rest_of_glob.as_os_str().is_empty() {
                // None of the components contained glob so this extra_src
                // refers to a specific file.
                if dir_containing_extra_srcs.is_file() {
                    insert(&dir_containing_extra_srcs);
                }
            } else {
                let glob = rest_of_glob.to_string_lossy();
                for path in Globs::new([glob], NO_EXCLUDE)?.walk(&dir_containing_extra_srcs) {
                    insert(&dir_containing_extra_srcs.join(path));
                }
            }

            if extra_srcs.len() == len_before {
                unmatched_globs.push(glob);
            }
        }

        if unmatched_globs.is_empty() {
            Ok(extra_srcs)
        } else {
            bail!("Unmatched globs in extra_srcs: {:?}", unmatched_globs);
        }
    }

    pub fn compute_mapped_srcs(
        &self,
        mapped_manifest_dir: &Path,
    ) -> anyhow::Result<Vec<(Option<PlatformExpr>, BTreeMap<SubtargetOrPath, BuckPath>)>> {
        let mut ret = vec![];

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            let mut map = BTreeMap::new();

            for (k, v) in &config.extra_mapped_srcs {
                map.insert(
                    // If the mapped source is target-like, take it as-is since
                    // we have nothing to resolve or find.
                    if k.starts_with(':') || k.contains("//") {
                        SubtargetOrPath::Path(BuckPath(PathBuf::from(k)))
                    } else {
                        self.subtarget_or_path(Path::new(k))?
                    },
                    BuckPath(mapped_manifest_dir.join(v)),
                );
            }

            if let Some(overlay) = &config.overlay {
                let overlay_dir = self.fixup_dir.join(overlay);
                let relative_overlay_dir = relative_path(&self.third_party_dir, &overlay_dir);
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
                        SubtargetOrPath::Path(BuckPath(relative_overlay_dir.join(&file))),
                        BuckPath(mapped_manifest_dir.join(&file)),
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
    pub fn compute_gen_srcs(&self) -> Vec<(Option<PlatformExpr>, ())> {
        let mut ret = vec![];

        if self.buildscript_rule_name().is_none() {
            // No generated sources
            return ret;
        }

        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            if config
                .buildscript
                .iter()
                .any(|fix| self.target_match(fix) && matches!(fix, BuildscriptFixup::GenSrcs(_)))
            {
                ret.push((platform.cloned(), ()));
            }
        }

        ret
    }

    /// Compute link_style (how dependencies should be linked)
    pub fn compute_link_style(&self) -> Vec<(Option<PlatformExpr>, String)> {
        let mut ret = Vec::new();
        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            if let Some(link_style) = config.link_style.as_ref() {
                ret.push((platform.cloned(), link_style.clone()));
            }
        }

        ret
    }

    /// Compute preferred_linkage (how dependents should link you)
    pub fn compute_preferred_linkage(&self) -> Vec<(Option<PlatformExpr>, String)> {
        let mut ret = Vec::new();
        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            if let Some(preferred_linkage) = config.preferred_linkage.as_ref() {
                ret.push((platform.cloned(), preferred_linkage.clone()));
            }
        }

        ret
    }

    /// Compute linker_flags (extra flags for the linker)
    pub fn compute_linker_flags(&self) -> Vec<(Option<PlatformExpr>, Vec<String>)> {
        let mut ret = Vec::new();
        for (platform, config) in self.fixup_config.configs(&self.package.version) {
            if !config.linker_flags.is_empty() {
                ret.push((platform.cloned(), config.linker_flags.clone()));
            }
        }

        ret
    }
}
