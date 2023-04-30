/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Implement buckification - generate Buck build rules from Cargo metadata

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Write;
use std::iter;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use rayon::prelude::*;

use crate::buck;
use crate::buck::Alias;
use crate::buck::BuckPath;
use crate::buck::Common;
use crate::buck::HttpArchive;
use crate::buck::Name;
use crate::buck::PlatformRustCommon;
use crate::buck::Rule;
use crate::buck::RuleRef;
use crate::buck::RustBinary;
use crate::buck::RustCommon;
use crate::buck::RustLibrary;
use crate::buck::Visibility;
use crate::cargo::cargo_get_lockfile_and_metadata;
use crate::cargo::ArtifactKind;
use crate::cargo::Edition;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::PkgId;
use crate::cargo::Source;
use crate::cargo::TargetReq;
use crate::config::Config;
use crate::fixups::Fixups;
use crate::index;
use crate::lockfile::Lockfile;
use crate::platform::platform_names_for_expr;
use crate::platform::PlatformExpr;
use crate::platform::PlatformName;
use crate::tp_metadata;
use crate::Args;
use crate::Paths;

// normalize a/../b => a/b
pub fn normalize_dotdot(path: &Path) -> PathBuf {
    let mut ret = PathBuf::new();

    for component in path.components() {
        match component {
            Component::ParentDir if ret.parent().is_some() => {
                ret.pop();
            }
            c => ret.push(c),
        }
    }

    ret
}

// Compute a path for `to` relative to `base`.
pub fn relative_path(mut base: &Path, to: &Path) -> PathBuf {
    let mut res = PathBuf::new();

    while !to.starts_with(base) {
        res.push("..");
        base = base.parent().expect("root dir not prefix of other?");
    }

    res.join(
        to.strip_prefix(base)
            .expect("already worked out it was a prefix"),
    )
}

/// Take a stream of platform-tagged items and apply them to the appropriate rule.
/// This also handles mapping a PlatformExpr into PlatformNames.
fn unzip_platform<T: Clone>(
    config: &Config,
    common: &mut PlatformRustCommon,
    perplat: &mut BTreeMap<PlatformName, PlatformRustCommon>,
    mut extend: impl FnMut(&mut PlatformRustCommon, T),
    things: impl IntoIterator<Item = (Option<PlatformExpr>, T)>,
) -> Result<()> {
    for (platform, thing) in things.into_iter() {
        match platform {
            Some(expr) => {
                let plats = platform_names_for_expr(config, &expr)
                    .with_context(|| format!("Bad platform expression \"{}\"", expr))?;

                for plat in plats {
                    extend(perplat.entry(plat.clone()).or_default(), thing.clone())
                }
            }
            None => extend(common, thing),
        }
    }

    Ok(())
}

/// Constant context for generating rules
struct RuleContext<'meta> {
    config: &'meta Config,
    paths: &'meta Paths,
    index: index::Index<'meta>,
    lockfile: Lockfile,
    done: Mutex<HashSet<(&'meta PkgId, TargetReq<'meta>)>>,
}

/// Generate rules for a set of dependencies
/// This is the top-level because the overall structure is that we're
/// generating rules for the top-level pseudo-package.
fn generate_dep_rules<'scope>(
    context: &'scope RuleContext<'scope>,
    scope: &rayon::Scope<'scope>,
    rule_tx: mpsc::Sender<Result<Rule>>,
    pkg_deps: impl IntoIterator<Item = (&'scope Manifest, TargetReq<'scope>)>,
) {
    let mut done = context.done.lock().unwrap();
    for (pkg, target_req) in pkg_deps {
        if done.insert((&pkg.id, target_req)) {
            let rule_tx = rule_tx.clone();
            scope.spawn(move |scope| {
                generate_rules(context, scope, rule_tx, pkg, target_req);
            })
        }
    }
}

/// Generate rules for all of a package's targets
fn generate_rules<'scope>(
    context: &'scope RuleContext<'scope>,
    scope: &rayon::Scope<'scope>,
    rule_tx: mpsc::Sender<Result<Rule>>,
    pkg: &'scope Manifest,
    target_req: TargetReq<'scope>,
) {
    if let TargetReq::Sources = target_req {
        let _ = rule_tx.send(generate_http_archive(context, pkg));
        return;
    }
    for tgt in &pkg.targets {
        let matching_kind = match target_req {
            TargetReq::Lib => tgt.kind_lib() || tgt.kind_proc_macro() || tgt.kind_cdylib(),
            TargetReq::Bin(required_bin) => tgt.kind_bin() && tgt.name == required_bin,
            TargetReq::EveryBin => tgt.kind_bin(),
            TargetReq::BuildScript => tgt.kind_custom_build(),
            TargetReq::Staticlib => tgt.kind_staticlib(),
            TargetReq::Cdylib => tgt.kind_cdylib(),
            TargetReq::Sources => unreachable!(),
        };
        if !matching_kind {
            continue;
        }
        match generate_target_rules(context, pkg, tgt) {
            Ok((rules, _)) if rules.is_empty() => {
                // Don't generate rules for dependencies if we're not emitting
                // any rules for this target.
            }
            Ok((rules, mut deps)) => {
                let is_private_root_pkg =
                    context.index.is_root_package(pkg) && !context.index.is_public_package(pkg);
                if !is_private_root_pkg {
                    for rule in rules {
                        let _ = rule_tx.send(Ok(rule));
                    }
                    if context.config.vendor.is_none() {
                        deps.push((pkg, TargetReq::Sources));
                    }
                }
                generate_dep_rules(context, scope, rule_tx.clone(), deps);
            }
            Err(err) => {
                log::error!(
                    "pkg {} target {}: rule generation failed: {:?}",
                    pkg,
                    tgt.name,
                    err
                );
                let _ = rule_tx.send(Err(err));
            }
        }
    }
}

fn generate_http_archive<'scope>(
    context: &'scope RuleContext<'scope>,
    pkg: &'scope Manifest,
) -> Result<Rule> {
    let lockfile_package = match context.lockfile.find(pkg) {
        Some(lockfile_package) => lockfile_package,
        None => {
            // This would be unexpected, because `cargo metadata` just got run,
            // which should have updated this lockfile.
            bail!(
                "Package \"{}\" {} not found in lockfile {}",
                pkg.name,
                pkg.version,
                context.paths.lockfile_path.display(),
            );
        }
    };

    match lockfile_package.source {
        Some(Source::CratesIo) => {}
        _ => {
            bail!(
                "`vendor = false` mode is supported only with exclusively crates.io dependencies. \"{}\" {} is coming from some source other than crates.io",
                pkg.name,
                pkg.version,
            );
        }
    }

    let sha256 = match &lockfile_package.checksum {
        Some(checksum) => checksum.clone(),
        None => {
            // Dependencies from Source::CratesIo should almost certainly be
            // associated with a checksum so a failure here is not expected.
            bail!(
                "No sha256 checksum available for \"{}\" {} in lockfile {}",
                pkg.name,
                pkg.version,
                context.paths.lockfile_path.display(),
            );
        }
    };

    Ok(Rule::HttpArchive(HttpArchive {
        name: Name(format!("{}-{}.crate", pkg.name, pkg.version)),
        sha256,
        strip_prefix: format!("{}-{}", pkg.name, pkg.version),
        urls: vec![format!(
            "https://crates.io/api/v1/crates/{}/{}/download",
            pkg.name, pkg.version,
        )],
        visibility: Visibility::Private,
        sort_key: Name(format!("{}-{}", pkg.name, pkg.version)),
    }))
}

/// Generate rules for a target. Returns the rules, and the
/// packages we depend on for further rule generation.
fn generate_target_rules<'scope>(
    context: &'scope RuleContext<'scope>,
    pkg: &'scope Manifest,
    tgt: &'scope ManifestTarget,
) -> Result<(Vec<Rule>, Vec<(&'scope Manifest, TargetReq<'scope>)>)> {
    let RuleContext {
        config,
        paths,
        index,
        ..
    } = context;

    log::info!("Generating rules for package {} target {}", pkg, tgt.name);

    let fixups = Fixups::new(config, paths, index, pkg, tgt)?;

    if fixups.omit_target() {
        return Ok((vec![], vec![]));
    }

    log::debug!("pkg {} target {} fixups {:#?}", pkg, tgt.name, fixups);

    let rootmod = if context.config.vendor.is_some() {
        relative_path(&paths.third_party_dir, &tgt.src_path)
    } else {
        let http_archive = format!("{}-{}.crate", pkg.name, pkg.version);
        let path_within_crate = relative_path(pkg.manifest_dir(), &tgt.src_path);
        PathBuf::from(http_archive).join(path_within_crate)
    };
    let edition = tgt.edition.unwrap_or(pkg.edition);

    let licenses = if config.vendor.is_none() {
        // The `licenses` attribute takes `attrs.source()` which is the file
        // containing the custom license text. For `vendor = false` mode, we
        // don't have such a file on disk, and we don't have a Buck label either
        // that could refer to the right generated location following download
        // because `http_archive` does not expose subtargets for each of the
        // individual contained files.
        BTreeSet::new()
    } else {
        fixups
            .manifestwalk(&config.license_patterns, iter::empty::<&str>(), false)?
            .chain(
                pkg.license_file
                    .as_ref()
                    .map(|file| {
                        relative_path(&paths.third_party_dir, pkg.manifest_dir()).join(file)
                    })
                    .into_iter(),
            )
            .map(BuckPath)
            .collect()
    };

    let global_rustc_flags = config.rustc_flags.clone();
    let global_platform_rustc_flags = config.platform_rustc_flags.clone();

    let srcdir = relative_path(pkg.manifest_dir(), tgt.src_path.parent().unwrap());

    // Get a list of the most obvious sources for the crate. This is either a list of
    // filename, or a list of globs.
    // If we're configured to get precise sources and we're using 2018+ edition source, then
    // parse the crate to see what files are actually used.
    let mut srcs =
        if config.vendor.is_some() && fixups.precise_srcs() && edition >= Edition::Rust2018 {
            measure_time::trace_time!("srcfiles for {}", pkg);
            match srcfiles::crate_srcfiles(&tgt.src_path) {
                Ok(srcs) => {
                    let srcs = srcs
                        .into_iter()
                        .map(|src| normalize_dotdot(&src.path))
                        .collect::<Vec<_>>();
                    log::debug!("crate_srcfiles returned {:#?}", srcs);
                    srcs
                }
                Err(err) => {
                    log::info!("crate_srcfiles failed: {}", err);
                    vec![]
                }
            }
        } else {
            vec![]
        };

    if srcs.is_empty() {
        // If that didn't work out, get srcs the globby way
        srcs = vec![tgt.src_path.parent().unwrap().join("**/*.rs")]
    }

    // Platform-specific rule bits which are common to all platforms
    let mut base = PlatformRustCommon {
        rustc_flags: global_rustc_flags.clone(),
        ..Default::default()
    };
    // Per platform rule bits
    let mut perplat: BTreeMap<PlatformName, PlatformRustCommon> = BTreeMap::new();

    // global_platform_rustc_flags is already in terms of PlatformName, do we can just add them in.
    for (plat, flags) in global_platform_rustc_flags {
        perplat.entry(plat).or_default().rustc_flags.extend(flags)
    }

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, flags| {
            log::debug!("pkg {} target {}: adding flags {:?}", pkg, tgt.name, flags);
            rule.rustc_flags.extend(flags)
        },
        fixups.compute_cmdline(),
    )
    .context("rustc_flags")?;

    if config.vendor.is_some() {
        unzip_platform(
            config,
            &mut base,
            &mut perplat,
            |rule, srcs| {
                log::debug!("pkg {} target {}: adding srcs {:?}", pkg, tgt.name, srcs);
                rule.srcs.extend(srcs.into_iter().map(BuckPath))
            },
            fixups.compute_srcs(srcs)?,
        )
        .context("srcs")?;
    } else {
        let http_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
        base.srcs
            .insert(BuckPath(PathBuf::from(http_archive_target)));
    }

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, map| {
            log::debug!(
                "pkg {} target {}: adding mapped_srcs(gen_srcs) {:?}",
                pkg,
                tgt.name,
                map
            );
            let targets = map
                .into_iter()
                .map(|(name, path)| (format!(":{name}"), BuckPath(path)));
            rule.mapped_srcs.extend(targets);
        },
        fixups.compute_gen_srcs(&srcdir),
    )
    .context("mapped_srcs(gen_srcs)")?;

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, map| {
            log::debug!(
                "pkg {} target {}: adding mapped_srcs(paths) {:?}",
                pkg,
                tgt.name,
                map
            );
            let paths = map
                .into_iter()
                .map(|(from, to)| (from.display().to_string(), BuckPath(to)));
            rule.mapped_srcs.extend(paths);
        },
        fixups.compute_mapped_srcs()?,
    )
    .context("mapped_srcs(paths)")?;

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, features| {
            log::debug!(
                "pkg {} target {}: adding features {:?}",
                pkg,
                tgt.name,
                features
            );
            rule.features.extend(features);
        },
        fixups.compute_features(),
    )
    .context("features")?;

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, env| {
            log::debug!("pkg {} target {}: adding env {:?}", pkg, tgt.name, env);
            rule.env.extend(env);
        },
        fixups.compute_env(),
    )
    .context("env")?;

    // Compute set of dependencies any rule we generate here will need. They will only
    // be emitted if we actually emit some rules below.
    let mut dep_pkgs = Vec::new();
    for (deppkg, dep, rename, dep_kind) in fixups.compute_deps()? {
        let target_req = dep_kind.target_req();
        if let TargetReq::Staticlib | TargetReq::Cdylib = target_req {
            let artifact = &dep_kind.artifact;
            bail!("unsupported artifact kind {artifact:?} for dependency {dep:?}");
        }
        if let Some(compile_target) = &dep_kind.compile_target {
            // Compile_target is not implemented yet.
            //
            // For example artifact dependencies allow a crate to depend on a
            // binary built for a different architecture.
            bail!("unsupported compile_target {compile_target:?} for dependency {dep:?}");
        }
        if dep.has_platform() {
            // If this is a platform-specific dependency, find the
            // matching supported platform(s) and insert it into the appropriate
            // dependency.
            // If the name is DEFAULT_PLATFORM then just put it in the normal generic deps
            for (name, platform) in &config.platform {
                let is_default = name.is_default();

                log::debug!(
                    "pkg {} target {} dep {:?} platform ({}, {:?}) filter {:?}",
                    pkg,
                    tgt.name,
                    dep,
                    name,
                    platform,
                    dep.filter(platform)
                );

                if dep.filter(platform)? {
                    let dep = dep.clone();

                    let recipient = if is_default {
                        // Just use normal deps
                        &mut base
                    } else {
                        perplat.entry(name.clone()).or_default()
                    };

                    if dep_kind.artifact == Some(ArtifactKind::Bin) {
                        let target_name = dep.target.strip_prefix(':').unwrap();
                        let bin_name = dep_kind.bin_name.as_ref().unwrap();
                        let env = format!("{}-{}", target_name, bin_name);
                        let location = format!("$(location {}-{}#check)", dep.target, bin_name);
                        recipient.env.insert(env, location);
                    } else if let Some(rename) = rename {
                        recipient.named_deps.insert(rename.to_owned(), dep);
                    } else {
                        recipient.deps.insert(dep);
                    }
                    if let Some(deppkg) = deppkg {
                        dep_pkgs.push((deppkg, target_req));
                    }
                }
            }
        } else {
            // Otherwise this is not platform-specific and can go into the
            // generic dependencies.
            if dep_kind.artifact == Some(ArtifactKind::Bin) {
                let target_name = dep.target.strip_prefix(':').unwrap();
                let bin_name = dep_kind.bin_name.as_ref().unwrap();
                let env = format!("{}-{}", target_name, bin_name);
                let location = format!("$(location {}-{}#check)", dep.target, bin_name);
                base.env.insert(env, location);
            } else if let Some(rename) = rename {
                base.named_deps.insert(rename.to_owned(), dep);
            } else {
                base.deps.insert(dep);
            }
            if let Some(deppkg) = deppkg {
                dep_pkgs.push((deppkg, target_req));
            }
        }
    }

    // "link_style" only really applies to binaries, so maintain separate binary base & perplat
    let mut bin_base = base.clone();
    let mut bin_perplat = perplat.clone();

    unzip_platform(
        config,
        &mut bin_base,
        &mut bin_perplat,
        |rule, link_style| {
            log::debug!("pkg {} target {}: link_style {}", pkg, tgt.name, link_style);
            rule.link_style = Some(link_style);
        },
        fixups.compute_link_style(),
    )
    .context("link_style")?;

    // "preferred_linkage" only really applies to libraries, so maintain separate library base &
    // perplat
    let mut lib_base = base.clone();
    let mut lib_perplat = perplat.clone();

    unzip_platform(
        config,
        &mut lib_base,
        &mut lib_perplat,
        |rule, preferred_linkage| {
            log::debug!(
                "pkg {} target {}: preferred_linkage {}",
                pkg,
                tgt.name,
                preferred_linkage
            );
            rule.preferred_linkage = Some(preferred_linkage);
        },
        fixups.compute_preferred_linkage(),
    )
    .context("preferred_linkage")?;

    // Standalone binary - binary for a package always takes the package's library as a dependency
    // if there is one
    if let Some(true) = pkg.dependency_target().map(ManifestTarget::kind_lib) {
        bin_base
            .deps
            .insert(RuleRef::from(index.private_rule_name(pkg)));
    }

    // Generate rules appropriate to each kind of crate we want to support
    let rules: Vec<Rule> = if (tgt.kind_lib() && tgt.crate_lib())
        || (tgt.kind_proc_macro() && tgt.crate_proc_macro())
        || (tgt.kind_cdylib() && tgt.crate_cdylib())
    {
        // Library or procmacro
        let mut rules = vec![];

        // The root package is public but we don't expose it via
        // an alias. The root package library is exposed directly.
        if index.is_public_target(pkg, TargetReq::Lib) && !index.is_root_package(pkg) {
            rules.push(Rule::Alias(Alias {
                name: index.public_rule_name(pkg),
                actual: index.private_rule_name(pkg),
                visibility: fixups.public_visibility(),
            }));
        }

        rules.push(Rule::Library(RustLibrary {
            common: RustCommon {
                common: Common {
                    name: if index.is_root_package(pkg) {
                        index.public_rule_name(pkg)
                    } else {
                        index.private_rule_name(pkg)
                    },
                    visibility: if index.is_root_package(pkg) {
                        Visibility::Public
                    } else {
                        Visibility::Private
                    },
                    licenses,
                    compatible_with: vec![],
                },
                krate: tgt.name.replace('-', "_"),
                rootmod: BuckPath(rootmod),
                edition,
                base: lib_base,
                platform: lib_perplat,
            },
            proc_macro: tgt.crate_proc_macro(),
            dlopen_enable: tgt.kind_cdylib() && fixups.python_ext().is_none(),
            python_ext: fixups.python_ext().map(str::to_string),
            linkable_alias: if index.is_public_target(pkg, TargetReq::Lib)
                && (tgt.kind_cdylib() || fixups.python_ext().is_some())
            {
                Some(index.public_rule_name(pkg).0)
            } else {
                None
            },
        }));

        // Library depends on the build script (if there is one).
        dep_pkgs.push((pkg, TargetReq::BuildScript));

        rules
    } else if tgt.crate_bin() && tgt.kind_custom_build() {
        // Build script
        let buildscript = RustBinary {
            common: RustCommon {
                common: Common {
                    name: Name(format!("{}-{}", pkg, tgt.name)),
                    visibility: Visibility::Private,
                    licenses: Default::default(),
                    compatible_with: vec![],
                },
                krate: tgt.name.replace('-', "_"),
                rootmod: BuckPath(rootmod),
                edition,
                base: PlatformRustCommon {
                    // don't use fixed ones because it will be a cyclic dependency
                    rustc_flags: global_rustc_flags,
                    link_style: bin_base.link_style.clone(),
                    ..base
                },
                platform: bin_perplat,
            },
        };
        fixups.emit_buildscript_rules(buildscript, config)?
    } else if tgt.kind_bin() && tgt.crate_bin() {
        let mut rules = vec![];
        let actual = Name(format!("{}-{}", index.private_rule_name(pkg), tgt.name));

        if index.is_public_target(pkg, TargetReq::Bin(&tgt.name)) {
            rules.push(Rule::Alias(Alias {
                name: Name(format!("{}-{}", index.public_rule_name(pkg), tgt.name)),
                actual: actual.clone(),
                visibility: fixups.public_visibility(),
            }));
        }

        rules.push(Rule::Binary(RustBinary {
            common: RustCommon {
                common: Common {
                    name: actual,
                    visibility: Visibility::Private,
                    licenses,
                    compatible_with: vec![],
                },
                krate: tgt.name.replace('-', "_"),
                rootmod: BuckPath(rootmod),
                edition,
                base: bin_base,
                platform: bin_perplat,
            },
        }));

        // Binary depends on the library (if there is one) and build script (if
        // there is one).
        dep_pkgs.push((pkg, TargetReq::Lib));
        dep_pkgs.push((pkg, TargetReq::BuildScript));

        rules
    } else {
        // Ignore everything else for now.
        log::info!("pkg {} target {} Skipping {:?}", pkg, tgt.name, tgt.kind());

        vec![]
    };

    Ok((rules, dep_pkgs))
}

pub(crate) fn buckify(config: &Config, args: &Args, paths: &Paths, stdout: bool) -> Result<()> {
    let (lockfile, metadata) = {
        measure_time::trace_time!("Get cargo metadata");
        cargo_get_lockfile_and_metadata(config, args, paths)?
    };

    if args.debug {
        log::trace!("Metadata {:#?}", metadata);
    }

    let index = index::Index::new(config.include_top_level, &metadata);

    let context = &RuleContext {
        config,
        paths,
        index,
        lockfile,
        done: Mutex::new(HashSet::new()),
    };

    let (tx, rx) = mpsc::channel();

    {
        measure_time::trace_time!("generate_dep_rules");
        rayon::scope(move |scope| {
            generate_dep_rules(
                context,
                scope,
                tx,
                [
                    (context.index.root_pkg, TargetReq::Lib),
                    (context.index.root_pkg, TargetReq::EveryBin),
                ],
            );
        });
    }

    // Collect rules from channel
    let rules: BTreeSet<_> = match rx.iter().collect::<Result<_>>() {
        Ok(rules) => rules,
        Err(err) => {
            if let Some(custom_err_msg) = config.unresolved_fixup_error_message.as_ref() {
                log::warn!(
                    "Additional info on how to fix unresolved fixup errors: {}",
                    custom_err_msg
                );
            }
            return Err(err);
        }
    };

    // Emit build rules to stdout
    if stdout {
        let mut out = Vec::new();
        buck::write_buckfile(&config.buck, rules.iter(), &mut out).context("writing buck file")?;
        // Ignore error, for example pipe closed resulting from
        // `reindeer buckify --stdout | head`.
        let _ = io::stdout().write_all(&out);
        return Ok(());
    }

    // Write build rules to file
    let buckpath = paths.third_party_dir.join(&config.buck.file_name);
    {
        measure_time::trace_time!("Write build rules to file");

        let mut out = Vec::new();
        buck::write_buckfile(&config.buck, rules.iter(), &mut out).context("writing buck file")?;
        if !matches!(fs::read(&buckpath), Ok(x) if x == out) {
            fs::write(&buckpath, out)
                .with_context(|| format!("write {} file", buckpath.display()))?;
        }
    }

    // Emit METADATA.bzl for each vendored dependency.
    if config.emit_metadata {
        measure_time::trace_time!("Emit METADATA.bzl for each vendored dependency");

        let extra_meta = context.index.get_extra_meta()?;
        context
            .index
            .all_packages()
            .par_bridge()
            .try_for_each(|pkg| {
                let mut out = Vec::new();
                tp_metadata::write(&config.buck, pkg, &extra_meta, &mut out).with_context(
                    || format!("writing METADATA.bzl file for {} {}", pkg.name, pkg.version),
                )?;
                // Avoid watcher churn since more often than not, nothing has
                // changed in existing files.
                let metadata_path = pkg.manifest_dir().join("METADATA.bzl");
                if !matches!(fs::read(&metadata_path), Ok(x) if x == out) {
                    fs::write(&metadata_path, out)
                        .with_context(|| format!("write {} file", metadata_path.display()))?;
                }
                anyhow::Ok(())
            })?;
    }

    log::trace!(
        "{} file written to {}",
        config.buck.file_name,
        buckpath.display()
    );

    Ok(())
}
