/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Per-package configuration information

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use anyhow::bail;

use crate::Paths;
use crate::buck;
use crate::buck::Alias;
use crate::buck::BuckPath;
use crate::buck::BuildscriptGenrule;
use crate::buck::Common;
use crate::buck::Name;
use crate::buck::PlatformBuildscriptGenrule;
use crate::buck::Rule;
use crate::buck::RuleRef;
use crate::buck::RustBinary;
use crate::buck::StringOrPath;
use crate::buck::SubtargetOrPath;
use crate::buck::Visibility;
use crate::buckify::evaluate_for_platforms;
use crate::buckify::short_name_for_git_repo;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::NodeDepKind;
use crate::cargo::Source;
use crate::cargo::TargetKind;
use crate::collection::SetOrMap;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::fixups::buildscript::BuildscriptRun;
use crate::fixups::buildscript::CxxLibraryFixup;
use crate::fixups::buildscript::ExportedHeaders;
use crate::fixups::buildscript::PrebuiltCxxLibraryFixup;
use crate::fixups::config::CargoEnv;
use crate::fixups::config::CargoEnvPurpose;
use crate::fixups::config::CargoEnvs;
use crate::fixups::config::CustomVisibility;
pub use crate::fixups::config::ExportSources;
use crate::fixups::config::FixupConfig;
use crate::fixups::config::FixupConfigFile;
use crate::glob::GlobSetKind;
use crate::glob::Globs;
use crate::glob::NO_EXCLUDE;
use crate::glob::TrackedGlobSet;
use crate::index::Index;
use crate::index::ResolvedDep;
use crate::path::normalize_path;
use crate::path::normalized_extend_path;
use crate::path::relative_path;
use crate::platform::PlatformName;
use crate::subtarget::Subtarget;

mod buildscript;
mod config;

/// Fixups for a specific package & target
pub struct Fixups<'meta> {
    config: &'meta Config,
    third_party_dir: &'meta Path,
    package: &'meta Manifest,
    fixup_dir: PathBuf,
    fixup_config: Arc<FixupConfigFile>,
    manifest_dir: &'meta Path,
}

impl<'meta> fmt::Debug for Fixups<'meta> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("Fixups")
            .field("package", &self.package.to_string())
            .field("third_party_dir", &self.third_party_dir)
            .field("fixup_dir", &self.fixup_dir)
            .field("manifest_dir", &self.manifest_dir)
            .field("fixup_config", &self.fixup_config)
            .finish()
    }
}

#[derive(Debug)]
pub struct FixupsCache<'meta> {
    config: &'meta Config,
    paths: &'meta Paths,
    fixups: Mutex<BTreeMap<&'meta str, Arc<FixupConfigFile>>>,
}

impl<'meta> FixupsCache<'meta> {
    pub fn new(config: &'meta Config, paths: &'meta Paths) -> Self {
        FixupsCache {
            config,
            paths,
            fixups: Mutex::new(BTreeMap::new()),
        }
    }

    /// Get fixups.toml for a specific package.
    pub fn get(&self, package: &'meta Manifest, public: bool) -> anyhow::Result<Fixups<'meta>> {
        let fixup_dir = self
            .paths
            .third_party_dir
            .join("fixups")
            .join(&package.name);

        let mut fixups_map = self.fixups.lock().unwrap_or_else(PoisonError::into_inner);
        let fixup_config = if let Some(arc) = fixups_map.get(package.name.as_str()) {
            Arc::clone(arc)
        } else {
            let fixup_config = FixupConfigFile::load(&fixup_dir, package, public)?;
            let arc = Arc::new(fixup_config);
            fixups_map.insert(&package.name, Arc::clone(&arc));
            arc
        };

        Ok(Fixups {
            third_party_dir: &self.paths.third_party_dir,
            manifest_dir: package.manifest_dir(),
            package,
            fixup_dir,
            fixup_config,
            config: self.config,
        })
    }

    pub fn lock(&self) -> MutexGuard<BTreeMap<&'meta str, Arc<FixupConfigFile>>> {
        self.fixups.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl<'meta> Fixups<'meta> {
    fn configs(&self, platform_name: &PlatformName) -> Vec<&FixupConfig> {
        let mut configs = Vec::new();

        if self
            .fixup_config
            .base
            .version_applies(&self.package.version)
        {
            configs.push(&self.fixup_config.base);
        }

        for (platform_expr, fixup) in &self.fixup_config.platform_fixup {
            if fixup.version_applies(&self.package.version)
                && platform_expr.eval(
                    &self.config.platform[platform_name],
                    Some(&self.package.version),
                )
            {
                configs.push(fixup);
            }
        }

        configs
    }

    // Return true if the script applies to this target.
    // There's a few cases:
    // - if the script doesn't specify a target, then it applies to the main "lib" target of the package
    // - otherwise it applies to the matching kind and (optionally) name (all names if not specified)
    fn target_match(
        &self,
        target: &ManifestTarget,
        applies_to_targets: &[(TargetKind, Option<String>)],
    ) -> bool {
        if applies_to_targets.is_empty() {
            // Applies to default target (the main "lib" target)
            let default_target = self.package.dependency_target();
            return Some(target) == default_target;
        }

        // Applies to the targets specified by the fixup
        applies_to_targets.iter().any(|(kind, name)| {
            target.kind.contains(kind) && name.as_ref().is_none_or(|name| &target.name == name)
        })
    }

    fn subtarget_or_path(
        &self,
        relative_to_manifest_dir: &Path,
    ) -> anyhow::Result<SubtargetOrPath> {
        if matches!(self.config.vendor, VendorConfig::Source(_))
            || matches!(self.package.source, Source::Local)
        {
            // Path to vendored file looks like "vendor/foo-1.0.0/src/lib.rs"
            let manifest_dir = relative_path(self.third_party_dir, self.manifest_dir);
            let path = manifest_dir.join(relative_to_manifest_dir);
            Ok(SubtargetOrPath::Path(BuckPath(path)))
        } else if let Source::Git { repo, .. } = &self.package.source {
            // Subtarget inside a git_fetch: ":serde-643d78919f35d883.git[src/lib.rs]"
            let short_name = short_name_for_git_repo(repo)?;
            Ok(SubtargetOrPath::Subtarget(Subtarget {
                target: Name(format!("{short_name}.git")),
                relative: BuckPath(relative_to_manifest_dir.to_owned()),
            }))
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
        match self.fixup_config.custom_visibility.as_ref() {
            Some(visibility) => match visibility {
                CustomVisibility::NoVersion(global) => Visibility::Custom(global.to_vec()),
                CustomVisibility::WithVersion(versioned) => versioned
                    .iter()
                    .filter(|(k, _)| k.matches(&self.package.version))
                    .map(|(_, v)| Visibility::Custom(v.to_vec()))
                    .next()
                    .unwrap_or(Visibility::Public),
            },
            None => Visibility::Public,
        }
    }

    pub fn python_ext(&self) -> Option<&str> {
        self.fixup_config.python_ext.as_deref()
    }

    pub fn omit_target(&self, target: &ManifestTarget) -> bool {
        self.fixup_config.omit_targets.contains(&target.name)
    }

    pub fn export_sources(&self) -> Option<&ExportSources> {
        self.fixup_config.export_sources.as_ref()
    }

    pub fn compatible_with(&self) -> &Vec<RuleRef> {
        &self.fixup_config.compatible_with
    }

    pub fn target_compatible_with(&self) -> &Vec<RuleRef> {
        &self.fixup_config.target_compatible_with
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
        mut buildscript_build: RustBinary,
        config: &'meta Config,
        manifest_dir: Option<SubtargetOrPath>,
        index: &Index,
        target: &ManifestTarget,
        compatible_platforms: &BTreeSet<&PlatformName>,
    ) -> anyhow::Result<Vec<Rule>> {
        let mut res = Vec::new();

        let rel_fixup = relative_path(self.third_party_dir, &self.fixup_dir);

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

        for platform_name in compatible_platforms {
            let mut has_explicit_buildscript_fixup = false;
            for fixup in self.configs(platform_name) {
                if !fixup.buildscript.defaulted_to_empty {
                    has_explicit_buildscript_fixup = true;
                    break;
                }
            }
            if !has_explicit_buildscript_fixup {
                let unresolved_package_msg = format!(
                    "{} v{} has a build script, but {} does not say what to do with it. Add `buildscript.run = false` or `buildscript.run = true`",
                    self.package.name,
                    self.package.version,
                    Path::new("fixups")
                        .join(&self.package.name)
                        .join("fixups.toml")
                        .display(),
                );
                if config.unresolved_fixup_error {
                    log::error!("{}", unresolved_package_msg);
                    bail!("Unresolved fixup errors, fix them and rerun buckify.");
                } else {
                    log::warn!("{}", unresolved_package_msg);
                    break;
                }
            }
        }

        let mut cxx_library = Vec::new();
        let mut prebuilt_cxx_library = Vec::new();
        let mut library_fixups = BTreeSet::new();
        let mut buildscript_platforms = BTreeSet::new();
        for &platform_name in compatible_platforms {
            for fixup in self.configs(platform_name) {
                if library_fixups.insert(&raw const *fixup) {
                    cxx_library.extend(&fixup.cxx_library);
                    prebuilt_cxx_library.extend(&fixup.prebuilt_cxx_library);
                }
            }

            if self.has_buildscript_for_platform(platform_name)? {
                buildscript_platforms.insert(platform_name);
            }
        }

        // Emit a C++ library build rule (elsewhere - add a dependency to it)
        for CxxLibraryFixup {
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
            target_compatible_with,
            preferred_linkage,
            undefined_symbols,
            ..
        } in cxx_library
        {
            let actual = Name(format!(
                "{}-{}",
                index.private_rule_name(self.package),
                name,
            ));

            if *public {
                let rule = Rule::Alias(Alias {
                    name: Name(format!("{}-{}", index.public_rule_name(self.package), name)),
                    actual: actual.clone(),
                    platforms: None,
                    visibility: self.public_visibility(),
                });
                res.push(rule);
            }

            let rule = buck::CxxLibrary {
                common: Common {
                    name: actual,
                    visibility: Visibility::Private,
                    licenses: Default::default(),
                    compatible_with: compatible_with.clone(),
                    target_compatible_with: target_compatible_with.clone(),
                },
                // Just collect the sources, excluding things in the exclude list
                srcs: {
                    Globs::new(srcs, exclude)
                        .walk(self.manifest_dir)
                        .map(|path| self.subtarget_or_path(&path))
                        .collect::<anyhow::Result<_>>()?
                },
                // Collect the nominated headers, plus everything in the fixup include
                // path(s).
                headers: {
                    let globs = Globs::new(headers, exclude);
                    let mut headers = BTreeSet::new();
                    for path in globs.walk(self.manifest_dir) {
                        headers.insert(self.subtarget_or_path(&path)?);
                    }

                    let globs = Globs::new(
                        GlobSetKind::from_iter(["**/*.asm", "**/*.h"]).unwrap(),
                        NO_EXCLUDE,
                    );
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
                    ExportedHeaders::Set(exported_headers) => {
                        let exported_header_globs = Globs::new(exported_headers, exclude);
                        let exported_headers = exported_header_globs
                            .walk(self.manifest_dir)
                            .map(|path| self.subtarget_or_path(&path))
                            .collect::<anyhow::Result<_>>()?;
                        SetOrMap::Set(exported_headers)
                    }
                    ExportedHeaders::Map(exported_headers) => SetOrMap::Map(
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
        for PrebuiltCxxLibraryFixup {
            name,
            static_libs,
            public,
            compatible_with,
            target_compatible_with,
            ..
        } in prebuilt_cxx_library
        {
            let static_lib_globs = Globs::new(static_libs, NO_EXCLUDE);
            for static_lib in static_lib_globs.walk(self.manifest_dir) {
                let actual = Name(format!(
                    "{}-{}-{}",
                    index.private_rule_name(self.package),
                    name,
                    static_lib.file_name().unwrap().to_string_lossy(),
                ));

                if *public {
                    let rule = Rule::Alias(Alias {
                        name: Name(format!(
                            "{}-{}-{}",
                            index.public_rule_name(self.package),
                            name,
                            static_lib.file_name().unwrap().to_string_lossy(),
                        )),
                        actual: actual.clone(),
                        platforms: None,
                        visibility: self.public_visibility(),
                    });
                    res.push(rule);
                }

                let rule = buck::PrebuiltCxxLibrary {
                    common: Common {
                        name: actual,
                        visibility: Visibility::Private,
                        licenses: Default::default(),
                        compatible_with: compatible_with.clone(),
                        target_compatible_with: target_compatible_with.clone(),
                    },
                    static_lib: self.subtarget_or_path(&static_lib)?,
                };
                res.push(Rule::PrebuiltCxxLibrary(rule));
            }
        }

        if !buildscript_platforms.is_empty() {
            buildscript_build.common.base.env.extend(
                self.fixup_config
                    .base
                    .buildscript
                    .build
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), StringOrPath::String(v.clone()))),
            );
            buildscript_build.common.base.link_style =
                self.fixup_config.base.buildscript.build.link_style.clone();

            let (local_manifest_dir, manifest_dir) = match manifest_dir {
                None => (None, None),
                Some(SubtargetOrPath::Path(path)) => (Some(path), None),
                Some(SubtargetOrPath::Subtarget(subtarget)) => (None, Some(subtarget)),
            };

            let mut buildscript_run = BuildscriptGenrule {
                name: self.buildscript_genrule_name(),
                buildscript_rule: buildscript_rule_name.clone(),
                package_name: self.package.name.clone(),
                version: self.package.version.clone(),
                local_manifest_dir,
                manifest_dir,
                base: PlatformBuildscriptGenrule {
                    features: buildscript_build.common.base.features.clone(),
                    env: BTreeMap::new(),
                },
                platform: BTreeMap::new(),
            };

            for &platform_name in &buildscript_platforms {
                if let Some(buildscript_build_common) =
                    buildscript_build.common.platform.get(platform_name)
                {
                    buildscript_run.platform.insert(
                        platform_name.clone(),
                        PlatformBuildscriptGenrule {
                            features: buildscript_build_common.features.clone(),
                            env: BTreeMap::new(),
                        },
                    );
                }
            }

            evaluate_for_platforms(
                &mut buildscript_build.common.base,
                &mut buildscript_build.common.platform,
                &buildscript_platforms,
                |platform_name| {
                    let mut build_cargo_env = BTreeMap::new();
                    for fixup in self.configs(platform_name) {
                        for cargo_env in fixup.cargo_env.iter() {
                            match cargo_env.purpose() {
                                CargoEnvPurpose::BuildOnly | CargoEnvPurpose::BuildAndRun => {}
                                CargoEnvPurpose::RunOnly => continue,
                            }
                            let value = if cargo_env == CargoEnv::CARGO_CRATE_NAME {
                                Some(StringOrPath::String("build_script_build".to_owned()))
                            } else {
                                let required = !matches!(fixup.cargo_env, CargoEnvs::All);
                                self.cargo_env_value(cargo_env, required, target)?
                            };
                            if let Some(value) = value {
                                build_cargo_env.insert(cargo_env, value);
                            }
                        }
                    }
                    Ok(build_cargo_env)
                },
                |rule, (cargo_env, value)| rule.env.insert(cargo_env.to_string(), value),
            )?;

            evaluate_for_platforms(
                &mut buildscript_run.base,
                &mut buildscript_run.platform,
                &buildscript_platforms,
                |platform_name| {
                    let mut run_cargo_env = BTreeMap::new();
                    for fixup in self.configs(platform_name) {
                        for cargo_env in fixup.cargo_env.iter() {
                            match cargo_env.purpose() {
                                CargoEnvPurpose::RunOnly | CargoEnvPurpose::BuildAndRun => {}
                                CargoEnvPurpose::BuildOnly => continue,
                            }
                            let required = !matches!(fixup.cargo_env, CargoEnvs::All);
                            let value = self.cargo_env_value(cargo_env, required, target)?;
                            if let Some(value) = value {
                                run_cargo_env.insert(cargo_env, value);
                            }
                        }
                    }
                    Ok(run_cargo_env)
                },
                |rule, (cargo_env, value)| rule.env.insert(cargo_env.to_string(), value),
            )?;

            evaluate_for_platforms(
                &mut buildscript_run.base,
                &mut buildscript_run.platform,
                &buildscript_platforms,
                |platform_name| {
                    let mut buildscript_run_env = BTreeMap::new();
                    for fixup in self.configs(platform_name) {
                        if let Some(BuildscriptRun { env }) = &fixup.buildscript.run {
                            buildscript_run_env.extend(env);
                        }
                    }
                    Ok(buildscript_run_env)
                },
                |rule, (key, value)| {
                    rule.env
                        .insert(key.clone(), StringOrPath::String(value.clone()));
                },
            )?;

            // Emit the build script itself
            res.push(Rule::BuildscriptBinary(buildscript_build));

            // Emit rule to get its stdout and filter it into args
            res.push(Rule::BuildscriptGenrule(buildscript_run));
        }

        Ok(res)
    }

    /// Return the set of features to enable, which is the union of the cargo-resolved ones
    /// and additional ones defined in the fixup.
    pub fn compute_features(
        &self,
        platform_name: &PlatformName,
        index: &'meta Index<'meta>,
    ) -> anyhow::Result<BTreeSet<&str>> {
        // Get features according to Cargo.
        let mut features = index.resolved_features(self.package, platform_name);

        // Apply extra feature fixups.
        for fixup in self.configs(platform_name) {
            for feature in &fixup.features {
                features.insert(feature);
            }
        }

        Ok(features)
    }

    pub fn omit_feature(
        &self,
        platform_name: &PlatformName,
        feature: &str,
    ) -> anyhow::Result<bool> {
        for fixup in self.configs(platform_name) {
            if fixup.omit_features.contains(feature) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub fn compute_rustc_cfg(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<BTreeSet<String>> {
        let mut cfgs = BTreeSet::new();

        for config in self.configs(platform_name) {
            for cfg in &config.cfgs {
                cfgs.insert(cfg.clone());
            }
        }

        Ok(cfgs)
    }

    pub fn compute_rustc_flags(&self, platform_name: &PlatformName) -> anyhow::Result<Vec<String>> {
        let mut rustc_flags = Vec::new();

        for config in self.configs(platform_name) {
            rustc_flags.extend_from_slice(&config.rustc_flags);
        }

        Ok(rustc_flags)
    }

    pub fn compute_rustc_flags_select(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<Vec<BTreeMap<String, Vec<String>>>> {
        let mut rustc_flags_select = Vec::new();

        for config in self.configs(platform_name) {
            rustc_flags_select.extend_from_slice(&config.rustc_flags_select);
        }

        Ok(rustc_flags_select)
    }

    /// Generate the set of deps for the target. This could just return the unmodified
    /// depenedencies, or it could add/remove them. This returns the Buck rule reference
    /// and the corresponding package if there is one (so the caller can limit its enumeration
    /// to only targets which were actually used).
    pub fn compute_deps(
        &self,
        platform_name: &PlatformName,
        index: &'meta Index<'meta>,
        target: &'meta ManifestTarget,
    ) -> anyhow::Result<
        Vec<(
            Option<&'meta Manifest>,
            RuleRef,
            Option<&'meta str>,
            &'meta NodeDepKind,
        )>,
    > {
        let mut ret = vec![];

        // Get dependencies according to Cargo.
        let deps = index.resolved_deps_for_target(self.package, target, platform_name);

        // Collect fixups.
        let mut omit_deps = HashSet::new();
        for fixup in self.configs(platform_name) {
            let fixup_omit_deps;
            let fixup_extra_deps;
            if target.crate_bin() && target.kind_custom_build() {
                fixup_omit_deps = &fixup.buildscript.build.omit_deps;
                fixup_extra_deps = &fixup.buildscript.build.extra_deps;
            } else {
                fixup_omit_deps = &fixup.omit_deps;
                fixup_extra_deps = &fixup.extra_deps;
            }

            omit_deps.extend(fixup_omit_deps.iter().map(String::as_str));

            for dep in fixup_extra_deps {
                ret.push((
                    None,
                    RuleRef::new(dep.to_string()),
                    None,
                    &NodeDepKind::ORDINARY,
                ));
            }

            for CxxLibraryFixup {
                name,
                targets,
                add_dep,
                ..
            } in &fixup.cxx_library
            {
                if !add_dep || !self.target_match(target, targets) {
                    continue;
                }
                ret.push((
                    None,
                    RuleRef::new(format!(
                        ":{}-{}",
                        index.private_rule_name(self.package),
                        name
                    )),
                    None,
                    &NodeDepKind::ORDINARY,
                ));
            }

            for PrebuiltCxxLibraryFixup {
                name,
                targets,
                add_dep,
                static_libs,
                ..
            } in &fixup.prebuilt_cxx_library
            {
                if !add_dep || !self.target_match(target, targets) {
                    continue;
                }
                let static_lib_globs = Globs::new(static_libs, NO_EXCLUDE);
                for static_lib in static_lib_globs.walk(self.manifest_dir) {
                    ret.push((
                        None,
                        RuleRef::new(format!(
                            ":{}-{}-{}",
                            index.private_rule_name(self.package),
                            name,
                            static_lib.file_name().unwrap().to_string_lossy(),
                        )),
                        None,
                        &NodeDepKind::ORDINARY,
                    ));
                }
            }
        }

        for ResolvedDep {
            package,
            rename,
            dep_kind,
        } in deps
        {
            log::debug!(
                "Resolved deps for {}/{} ({:?}) - {} as {} ({:?})",
                self.package,
                target.name,
                target.kind(),
                package,
                rename,
                target.kind()
            );

            ret.push((
                Some(package),
                RuleRef::from(index.private_rule_name(package)),
                // Only use the rename if it isn't the same as the target anyway.
                match package.dependency_target() {
                    Some(tgt) if tgt.name.replace('-', "_") == rename => None,
                    Some(_) | None => Some(rename),
                },
                dep_kind,
            ));
        }

        Ok(ret)
    }

    pub fn omit_dep(&self, platform_name: &PlatformName, dep: &str) -> anyhow::Result<bool> {
        for fixup in self.configs(platform_name) {
            if fixup.omit_deps.contains(dep) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Additional environment
    pub fn compute_env(
        &self,
        target: &ManifestTarget,
        platform_name: &PlatformName,
    ) -> anyhow::Result<BTreeMap<String, StringOrPath>> {
        let mut ret = BTreeMap::new();

        for config in self.configs(platform_name) {
            ret.extend(
                config
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), StringOrPath::String(v.clone()))),
            );

            for cargo_env in config.cargo_env.iter() {
                match cargo_env {
                    // Not set for builds, only build script execution
                    CargoEnv::CARGO_MANIFEST_LINKS => continue,
                    CargoEnv::CARGO_CRATE_NAME
                    | CargoEnv::CARGO_MANIFEST_DIR
                    | CargoEnv::CARGO_PKG_AUTHORS
                    | CargoEnv::CARGO_PKG_DESCRIPTION
                    | CargoEnv::CARGO_PKG_NAME
                    | CargoEnv::CARGO_PKG_REPOSITORY
                    | CargoEnv::CARGO_PKG_VERSION
                    | CargoEnv::CARGO_PKG_VERSION_MAJOR
                    | CargoEnv::CARGO_PKG_VERSION_MINOR
                    | CargoEnv::CARGO_PKG_VERSION_PATCH
                    | CargoEnv::CARGO_PKG_VERSION_PRE => {}
                }
                let required = !matches!(config.cargo_env, CargoEnvs::All);
                if let Some(value) = self.cargo_env_value(cargo_env, required, target)? {
                    ret.insert(cargo_env.to_string(), value);
                }
            }
        }

        Ok(ret)
    }

    fn cargo_env_value(
        &self,
        cargo_env: CargoEnv,
        required: bool,
        target: &ManifestTarget,
    ) -> anyhow::Result<Option<StringOrPath>> {
        let value = match cargo_env {
            CargoEnv::CARGO_CRATE_NAME => StringOrPath::String(target.name.replace('-', "_")),
            CargoEnv::CARGO_MANIFEST_DIR => {
                if matches!(self.config.vendor, VendorConfig::Source(_))
                    || matches!(self.package.source, Source::Local)
                {
                    StringOrPath::Path(BuckPath(relative_path(
                        self.third_party_dir,
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
            CargoEnv::CARGO_MANIFEST_LINKS => {
                if let Some(links) = &self.package.links {
                    StringOrPath::String(links.clone())
                } else if required {
                    bail!(
                        "cannot use CARGO_MANIFEST_LINKS in `cargo_env` if there is no `links` in the crate's manifest",
                    );
                } else {
                    // When using `cargo_env = true`, we just omit CARGO_MANIFEST_LINKS
                    // on a crate that has no `links`.
                    return Ok(None);
                }
            }
            CargoEnv::CARGO_PKG_AUTHORS => StringOrPath::String(self.package.authors.join(":")),
            CargoEnv::CARGO_PKG_DESCRIPTION => {
                // Cargo provides "" if there is no `description` in Cargo.toml
                StringOrPath::String(self.package.description.clone().unwrap_or_default())
            }
            CargoEnv::CARGO_PKG_REPOSITORY => {
                // Cargo provides "" if there is no `repository` in Cargo.toml
                StringOrPath::String(self.package.repository.clone().unwrap_or_default())
            }
            CargoEnv::CARGO_PKG_VERSION => StringOrPath::String(self.package.version.to_string()),
            CargoEnv::CARGO_PKG_VERSION_MAJOR => {
                StringOrPath::String(self.package.version.major.to_string())
            }
            CargoEnv::CARGO_PKG_VERSION_MINOR => {
                StringOrPath::String(self.package.version.minor.to_string())
            }
            CargoEnv::CARGO_PKG_VERSION_PATCH => {
                StringOrPath::String(self.package.version.patch.to_string())
            }
            CargoEnv::CARGO_PKG_VERSION_PRE => {
                StringOrPath::String(self.package.version.pre.to_string())
            }
            CargoEnv::CARGO_PKG_NAME => StringOrPath::String(self.package.name.clone()),
        };
        Ok(Some(value))
    }

    /// Given a glob for the srcs, walk the filesystem to get the full set.
    /// `srcs` is the normal source glob rooted at the package's manifest dir.
    pub fn compute_srcs(
        &self,
        platform_name: &PlatformName,
        naive_srcs: &[PathBuf],
    ) -> anyhow::Result<BTreeSet<PathBuf>> {
        // This function is only used in vendoring mode, so it's guaranteed that
        // manifest_dir is a subdirectory of third_party_dir.
        assert!(
            matches!(self.config.vendor, VendorConfig::Source(_))
                || matches!(self.package.source, Source::Local)
        );
        let manifest_rel = relative_path(self.third_party_dir, self.manifest_dir);

        let mut ret = BTreeSet::new();
        for path in naive_srcs {
            let mut src = manifest_rel.clone();
            normalized_extend_path(&mut src, path);
            ret.insert(src);
        }

        for fixup in self.configs(platform_name) {
            ret.extend(self.compute_extra_srcs(&fixup.extra_srcs)?);
        }

        for fixup in self.configs(platform_name) {
            let mapped_files = fixup.overlay_and_mapped_files(&self.fixup_dir)?;
            ret.retain(|path| {
                let path_in_crate = relative_path(&manifest_rel, path);
                !mapped_files.contains(&path_in_crate) && !fixup.omit_srcs.is_match(&path_in_crate)
            });
        }

        log::debug!(
            "pkg {}, naive_srcs {:?}, manifest_rel {} => {:#?}",
            self.package,
            naive_srcs,
            manifest_rel.display(),
            ret
        );

        Ok(ret)
    }

    fn compute_extra_srcs(&self, globs: &TrackedGlobSet) -> anyhow::Result<HashSet<PathBuf>> {
        let mut extra_srcs = HashSet::new();

        for glob in globs {
            // The extra_srcs are allowed to be located outside this crate's
            // manifest dir, i.e. starting with "../". For example libstd refers
            // to files from portable-simd and stdarch, which are located in
            // sibling directories. Thus doing a WalkDir over manifest_dir is
            // not sufficient; here we pick the right directory to walk for this
            // glob.
            let mut dir_containing_extra_srcs = self.manifest_dir.to_owned();
            let mut rest_of_glob = glob.components();
            while let Some(component) = rest_of_glob.as_path().components().next() {
                if component.as_os_str().to_string_lossy().contains('*') {
                    // Ready to do globby stuff.
                    break;
                } else {
                    rest_of_glob.next().unwrap();
                    dir_containing_extra_srcs.push(component);
                }
            }

            let mut insert = |absolute_path: &Path| {
                let tp_rel_path = relative_path(self.third_party_dir, absolute_path);
                extra_srcs.insert(normalize_path(&tp_rel_path));
                glob.mark_used();
            };

            let rest_of_glob = rest_of_glob.as_path();
            if rest_of_glob.as_os_str().is_empty() {
                // None of the components contained glob so this extra_src
                // refers to a specific file.
                if dir_containing_extra_srcs.is_file() {
                    insert(&dir_containing_extra_srcs);
                }
            } else {
                let globs = Globs::new(GlobSetKind::from_iter([rest_of_glob])?, NO_EXCLUDE);
                for path in globs.walk(&dir_containing_extra_srcs) {
                    insert(&dir_containing_extra_srcs.join(path));
                }
            }
        }

        Ok(extra_srcs)
    }

    pub fn compute_mapped_srcs(
        &self,
        mapped_manifest_dir: &Path,
        target: &ManifestTarget,
        platform_name: &PlatformName,
    ) -> anyhow::Result<BTreeMap<SubtargetOrPath, BuckPath>> {
        let mut ret = BTreeMap::new();

        for config in self.configs(platform_name) {
            for (k, v) in &config.extra_mapped_srcs {
                ret.insert(
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
                let relative_overlay_dir = relative_path(self.third_party_dir, &overlay_dir);
                let overlay_files = config.overlay_files(&self.fixup_dir)?;

                log::debug!(
                    "pkg {} target {} overlay_dir {} overlay_files {:?}",
                    self.package,
                    target.name,
                    overlay_dir.display(),
                    overlay_files
                );

                for file in overlay_files {
                    ret.insert(
                        SubtargetOrPath::Path(BuckPath(relative_overlay_dir.join(&file))),
                        BuckPath(mapped_manifest_dir.join(&file)),
                    );
                }
            }
        }

        Ok(ret)
    }

    pub fn has_buildscript_for_platform(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<bool> {
        if self.buildscript_target().is_some() {
            for config in self.configs(platform_name) {
                if config.buildscript.run.is_some() {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Compute link_style (how dependencies should be linked)
    pub fn compute_link_style(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<Option<String>> {
        let mut link_style = None;

        for config in self.configs(platform_name) {
            if config.link_style.is_some() {
                link_style = config.link_style.as_ref();
            }
        }

        Ok(link_style.cloned())
    }

    /// Compute preferred_linkage (how dependents should link you)
    pub fn compute_preferred_linkage(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<Option<String>> {
        let mut preferred_linkage = None;

        for config in self.configs(platform_name) {
            if config.preferred_linkage.is_some() {
                preferred_linkage = config.preferred_linkage.as_ref();
            }
        }

        Ok(preferred_linkage.cloned())
    }

    /// Compute linker_flags (extra flags for the linker)
    pub fn compute_linker_flags(
        &self,
        platform_name: &PlatformName,
    ) -> anyhow::Result<Vec<String>> {
        let mut linker_flags = Vec::new();

        for config in self.configs(platform_name) {
            linker_flags.extend_from_slice(&config.linker_flags);
        }

        Ok(linker_flags)
    }
}
