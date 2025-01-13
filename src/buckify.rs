/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Implement buckification - generate Buck build rules from Cargo metadata

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc;

use anyhow::Context;
use anyhow::bail;
use cached::proc_macro::cached;
use fnv::FnvHasher;
use itertools::Itertools;
use url::Url;

use crate::Args;
use crate::Paths;
use crate::buck;
use crate::buck::Alias;
use crate::buck::BuckPath;
use crate::buck::Common;
use crate::buck::ExtractArchive;
use crate::buck::Filegroup;
use crate::buck::GitFetch;
use crate::buck::HttpArchive;
use crate::buck::Name;
use crate::buck::PlatformRustCommon;
use crate::buck::Rule;
use crate::buck::RuleRef;
use crate::buck::RustBinary;
use crate::buck::RustCommon;
use crate::buck::RustLibrary;
use crate::buck::StringOrPath;
use crate::buck::SubtargetOrPath;
use crate::buck::Visibility;
use crate::cargo::ArtifactKind;
use crate::cargo::Edition;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::PkgId;
use crate::cargo::Source;
use crate::cargo::TargetReq;
use crate::cargo::cargo_get_lockfile_and_metadata;
use crate::collection::SetOrMap;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::fixups::ExportSources;
use crate::fixups::Fixups;
use crate::glob::Globs;
use crate::glob::NO_EXCLUDE;
use crate::index;
use crate::lockfile::Lockfile;
use crate::lockfile::LockfilePackage;
use crate::platform::PlatformExpr;
use crate::platform::PlatformName;
use crate::platform::platform_names_for_expr;
use crate::srcfiles::crate_srcfiles;
use crate::universe::UniverseName;

// normalize a/b/../c => a/c and a/./b => a/b
pub fn normalize_path(path: &Path) -> PathBuf {
    #![allow(clippy::enum_glob_use)]
    use std::path::Component::*;

    let max_len = path.as_os_str().len();
    let mut ret = PathBuf::with_capacity(max_len);

    for c in path.components() {
        match c {
            Normal(_) | RootDir | Prefix(_) => ret.push(c),
            ParentDir => match ret.components().next_back() {
                Some(Normal(_)) => {
                    ret.pop();
                }
                Some(RootDir | Prefix(_)) | None => {}
                Some(ParentDir | CurDir) => {
                    unreachable!();
                }
            },
            CurDir => {}
        };
    }

    if ret.as_os_str().is_empty() {
        ret.push(CurDir.as_os_str());
    }

    ret
}

// Compute a path for `to` relative to `base`.
pub fn relative_path(mut base: &Path, to: &Path) -> PathBuf {
    let mut res = PathBuf::new();

    while !to.starts_with(base) {
        log::debug!(
            "relative_path: to={}, base={}, res={}",
            to.display(),
            base.display(),
            res.display()
        );
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
) -> anyhow::Result<()> {
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
    rule_tx: mpsc::Sender<anyhow::Result<Rule>>,
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
    rule_tx: mpsc::Sender<anyhow::Result<Rule>>,
    pkg: &'scope Manifest,
    target_req: TargetReq<'scope>,
) {
    if let TargetReq::Sources = target_req {
        if let Some(nonvendored_sources) =
            generate_nonvendored_sources_archive(context, pkg).transpose()
        {
            let _ = rule_tx.send(nonvendored_sources);
        }
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
                    if !matches!(context.config.vendor, VendorConfig::Source(_)) {
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

fn generate_nonvendored_sources_archive<'scope>(
    context: &'scope RuleContext<'scope>,
    pkg: &'scope Manifest,
) -> anyhow::Result<Option<Rule>> {
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

    match &lockfile_package.source {
        Source::Local => Ok(None),
        Source::CratesIo => match context.config.vendor {
            VendorConfig::Off => generate_http_archive(context, pkg, lockfile_package).map(Some),
            VendorConfig::LocalRegistry => generate_extract_archive(pkg).map(Some),
            VendorConfig::Source(_) => unreachable!(),
        },
        Source::Git {
            repo, commit_hash, ..
        } => match context.config.vendor {
            VendorConfig::Off => generate_git_fetch(repo, commit_hash).map(Some),
            VendorConfig::LocalRegistry => generate_extract_archive(pkg).map(Some),
            VendorConfig::Source(_) => unreachable!(),
        },
        Source::Unrecognized(_) => {
            bail!(
                "`vendor = false` mode is supported only with exclusively crates.io and https git dependencies. \"{}\" {} is coming from some other source",
                pkg.name,
                pkg.version,
            );
        }
    }
}

fn generate_extract_archive(pkg: &Manifest) -> anyhow::Result<Rule> {
    Ok(Rule::ExtractArchive(ExtractArchive {
        name: Name(format!("{}-{}.crate", pkg.name, pkg.version)),
        src: BuckPath(PathBuf::from(format!(
            "vendor/{}-{}.crate",
            pkg.name, pkg.version,
        ))),
        strip_prefix: format!("{}-{}", pkg.name, pkg.version),
        sub_targets: BTreeSet::new(), // populated later after all fixups are constructed
        visibility: Visibility::Private,
        sort_key: Name(format!("{}-{}", pkg.name, pkg.version)),
    }))
}

fn generate_http_archive<'scope>(
    context: &'scope RuleContext<'scope>,
    pkg: &'scope Manifest,
    lockfile_package: &LockfilePackage,
) -> anyhow::Result<Rule> {
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
        sub_targets: BTreeSet::new(), // populated later after all fixups are constructed
        urls: vec![format!(
            "https://static.crates.io/crates/{}/{}/download",
            pkg.name, pkg.version,
        )],
        visibility: Visibility::Private,
        sort_key: Name(format!("{}-{}", pkg.name, pkg.version)),
    }))
}

fn generate_git_fetch(repo: &str, commit_hash: &str) -> anyhow::Result<Rule> {
    let short_name = short_name_for_git_repo(repo)?;

    Ok(Rule::GitFetch(GitFetch {
        name: Name(format!("{}.git", short_name)),
        repo: repo.to_owned(),
        rev: commit_hash.to_owned(),
        visibility: Visibility::Private,
    }))
}

/// Create a uniquely hashed directory name for the arbitrary source url
pub fn short_name_for_git_repo(repo: &str) -> anyhow::Result<String> {
    let mut canonical = Url::parse(&repo.to_lowercase()).context("invalid git url")?;

    anyhow::ensure!(
        canonical.scheme() == "https",
        "only https git urls are supported",
    );

    canonical.set_query(None);
    canonical.set_fragment(None);

    // It would be nice to just say "you're using a .git extension, please
    // remove it", but some git providers (notably GitLab) require the .git
    // extension in the URL, while other providers (notably GitHub) treat URLs
    // with or without the extension exactly the same. If we don't take the .git
    // extension into account at all we could run into a situation where 2
    // or more crates are sourced from the same git repo but with and without
    // the .git extension, causing them to be hashed and placed differently
    if canonical.path().ends_with(".git") {
        // This is less efficient but simpler than using the path_segments_mut
        // API.
        let stripped = canonical.path().trim_end_matches(".git").to_owned();
        canonical.set_path(&stripped);
    }

    let repo_name = canonical
        .path_segments()
        .and_then(|mut it| it.next_back())
        .unwrap_or("_git");

    let mut hasher = FnvHasher::default();
    canonical.hash(&mut hasher);
    let url_hash = hasher.finish();

    Ok(format!("{repo_name}-{url_hash:016x}"))
}

/// Find the git repository containing the given manifest directory.
fn find_repository_root(manifest_dir: &Path) -> anyhow::Result<&Path> {
    let mut dir = manifest_dir;
    loop {
        if dir.join(".git").exists() {
            return Ok(dir);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!(
                "failed to find what git repo this manifest directory is a member of: {}",
                manifest_dir.display(),
            ),
        }
    }
}

#[cached]
fn srcfiles(manifest_dir: PathBuf, path: PathBuf) -> Vec<PathBuf> {
    let sources = crate_srcfiles(path);
    if sources.errors.is_empty() {
        let srcs = sources
            .files
            .into_iter()
            .map(|src| normalize_path(&relative_path(&manifest_dir, &src)))
            .collect::<Vec<_>>();
        log::debug!("crate_srcfiles returned {:#?}", srcs);
        srcs
    } else {
        log::debug!("crate_srcfiles failed: {:?}", sources.errors);
        vec![]
    }
}

/// Generate rules for a target. Returns the rules, and the
/// packages we depend on for further rule generation.
fn generate_target_rules<'scope>(
    context: &'scope RuleContext<'scope>,
    pkg: &'scope Manifest,
    tgt: &'scope ManifestTarget,
) -> anyhow::Result<(Vec<Rule>, Vec<(&'scope Manifest, TargetReq<'scope>)>)> {
    let RuleContext {
        config,
        paths,
        index,
        ..
    } = context;

    log::debug!("Generating rules for package {} target {}", pkg, tgt.name);

    let fixups = Fixups::new(config, paths, index, pkg, tgt)?;

    if fixups.omit_target() {
        return Ok((vec![], vec![]));
    }

    log::debug!("pkg {} target {} fixups {:#?}", pkg, tgt.name, fixups);

    let manifest_dir = pkg.manifest_dir();
    let mapped_manifest_dir = if matches!(config.vendor, VendorConfig::Source(_))
        || matches!(pkg.source, Source::Local)
    {
        match manifest_dir
            .strip_prefix(&paths.third_party_dir)
			.with_context(|| format!(
					"crate sources would be inaccessible from the generated BUCK file, cannot refer to {} from {}.",
					relative_path(&paths.third_party_dir, manifest_dir).display(),
					paths.third_party_dir.join(&config.buck.file_name).display(),
				))
			.map(ToOwned::to_owned){
				Err(_) if !will_use_rules => {
					PathBuf::from("__unused__")
				}
				res => res?
			}
    } else if let VendorConfig::LocalRegistry = config.vendor {
        PathBuf::from(format!("{}-{}.crate", pkg.name, pkg.version))
    } else if let Source::Git { repo, .. } = &pkg.source {
        let git_fetch = short_name_for_git_repo(repo)?;
        let repository_root = find_repository_root(manifest_dir)?;
        let path_within_repo = relative_path(repository_root, manifest_dir);
        PathBuf::from(git_fetch).join(path_within_repo)
    } else {
        PathBuf::from(format!("{}-{}.crate", pkg.name, pkg.version))
    };
    let crate_root = mapped_manifest_dir.join(relative_path(manifest_dir, &tgt.src_path));
    let edition = tgt.edition.unwrap_or(pkg.edition);

    let mut licenses = BTreeSet::new();
    if !matches!(config.vendor, VendorConfig::Source(_)) {
        // The `licenses` attribute takes `attrs.source()` which is the file
        // containing the custom license text. For `vendor = false` mode, we
        // don't have such a file on disk, and we don't have a Buck label either
        // that could refer to the right generated location following download
        // because `http_archive` does not expose subtargets for each of the
        // individual contained files.
    } else {
        let rel_manifest = relative_path(&paths.third_party_dir, manifest_dir);
        let mut license_globs = Globs::new(&config.license_patterns, NO_EXCLUDE)?;
        for path in license_globs.walk(manifest_dir) {
            licenses.insert(BuckPath(rel_manifest.join(path)));
        }
        if let Some(license_file) = &pkg.license_file {
            // Buck rejects `..` in a source path: "Error when treated as a
            // path: expected a normalized path but got an un-normalized path
            // instead: `vendor/libcst_derive-0.1.0/../../LICENSE`"
            if !license_file.components().contains(&Component::ParentDir) {
                // But still normalize to get rid of `.`
                // (e.g. `vendor/polars-arrow-0.34.2/./LICENSE`).
                let license_path = normalize_path(&rel_manifest.join(license_file));
                licenses.insert(BuckPath(license_path));
            }
        }
    };

    // Get a list of the most obvious sources for the crate. This is either a list of
    // filename, or a list of globs.
    // If we're configured to get precise sources and we're using 2018+ edition source, then
    // parse the crate to see what files are actually used.
    let mut srcs = if (matches!(config.vendor, VendorConfig::Source(_))
        || matches!(pkg.source, Source::Local))
        && fixups.precise_srcs()
        && edition >= Edition::Rust2018
    {
        measure_time::trace_time!("srcfiles for {}", pkg);
        srcfiles(manifest_dir.to_owned(), tgt.src_path.clone())
    } else {
        vec![]
    };

    if srcs.is_empty() {
        // If that didn't work out, get srcs the globby way
        let dir_containing_src = tgt.src_path.parent().unwrap();
        srcs.push(relative_path(manifest_dir, dir_containing_src).join("**/*.rs"));
    }

    // Platform-specific rule bits which are common to all platforms
    let mut base = PlatformRustCommon::default();
    // Per platform rule bits
    let mut perplat: BTreeMap<PlatformName, PlatformRustCommon> = BTreeMap::new();

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, (flags, flags_select)| {
            log::debug!(
                "pkg {} target {}: adding flags {:?} and select {:?}",
                pkg,
                tgt.name,
                flags,
                flags_select
            );
            rule.rustc_flags.common.extend(flags);
            flags_select.into_iter().for_each(|(k, v)| {
                rule.rustc_flags
                    .selects
                    .entry(k)
                    .or_insert(BTreeSet::new())
                    .extend(v);
            });
        },
        fixups.compute_cmdline(),
    )
    .context("rustc_flags")?;

    if matches!(config.vendor, VendorConfig::Source(_)) || matches!(pkg.source, Source::Local) {
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
    } else if let VendorConfig::LocalRegistry = config.vendor {
        let extract_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
        base.srcs
            .insert(BuckPath(PathBuf::from(extract_archive_target)));
    } else if let Source::Git { repo, .. } = &pkg.source {
        let short_name = short_name_for_git_repo(repo)?;
        let git_fetch_target = format!(":{}.git", short_name);
        base.srcs.insert(BuckPath(PathBuf::from(git_fetch_target)));
    } else {
        let http_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
        base.srcs
            .insert(BuckPath(PathBuf::from(http_archive_target)));
    }

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, ()| {
            log::debug!(
                "pkg {} target {}: adding OUT_DIR for gen_srcs",
                pkg,
                tgt.name,
            );
            rule.env.unwrap_mut().insert(
                "OUT_DIR".to_owned(),
                StringOrPath::String(format!(
                    "$(location :{}[out_dir])",
                    fixups.buildscript_genrule_name(),
                )),
            );
        },
        fixups.compute_gen_srcs(),
    )
    .context("OUT_DIR for gen_srcs")?;

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
            rule.mapped_srcs.extend(map);
        },
        fixups.compute_mapped_srcs(&mapped_manifest_dir)?,
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
            rule.features.unwrap_mut().extend(features);
        },
        fixups.compute_features()?,
    )
    .context("features")?;

    unzip_platform(
        config,
        &mut base,
        &mut perplat,
        |rule, env| {
            log::debug!("pkg {} target {}: adding env {:?}", pkg, tgt.name, env);
            rule.env.unwrap_mut().extend(env);
        },
        fixups.compute_env()?,
    )
    .context("env")?;

    // Compute set of dependencies any rule we generate here will need. They will only
    // be emitted if we actually emit some rules below.
    let mut dep_pkgs = Vec::new();
    for (deppkg, dep, rename, dep_kind) in fixups.compute_deps()? {
        let target_req = dep_kind.target_req();
        if let TargetReq::Cdylib = target_req {
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
                        recipient
                            .env
                            .unwrap_mut()
                            .insert(env, StringOrPath::String(location));
                    } else if let Some(rename) = rename {
                        recipient
                            .named_deps
                            .unwrap_mut()
                            .insert(rename.to_owned(), dep);
                    } else {
                        recipient.deps.unwrap_mut().insert(dep);
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
                base.env
                    .unwrap_mut()
                    .insert(env, StringOrPath::String(location));
            } else if let Some(rename) = rename {
                base.named_deps.unwrap_mut().insert(rename.to_owned(), dep);
            } else {
                base.deps.unwrap_mut().insert(dep);
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

    unzip_platform(
        config,
        &mut bin_base,
        &mut bin_perplat,
        |rule, linker_flags| {
            log::debug!(
                "pkg {} target {}: linker_flags {:?}",
                pkg,
                tgt.name,
                linker_flags
            );
            rule.linker_flags = linker_flags;
        },
        fixups.compute_linker_flags(),
    )
    .context("linker_flags")?;

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
            .unwrap_mut()
            .insert(RuleRef::from(index.private_rule_name(pkg)));
    }

    // Generate rules appropriate to each kind of crate we want to support
    let mut rules: Vec<Rule> = if (tgt.kind_lib() && tgt.crate_lib())
        || (tgt.kind_proc_macro() && tgt.crate_proc_macro())
        || (tgt.kind_cdylib() && tgt.crate_cdylib())
        || (tgt.kind_staticlib() && tgt.crate_staticlib())
    {
        // Library, procmacro, cdylib, or staticlib
        let mut rules = vec![];

        // The root package is public but we don't expose it via an alias. The
        // root package library is exposed directly. In the case the root
        // package is public and the target is a staticlib we do expose it via
        // an alias.
        if index.is_public_target(pkg, TargetReq::Lib) && !index.is_root_package(pkg)
            || index.is_public_target(pkg, TargetReq::Staticlib)
        {
            rules.push(Rule::Alias(Alias {
                name: index.public_rule_name(pkg),
                actual: index.private_rule_name(pkg),
                visibility: fixups.public_visibility(),
            }));
        }

        let rust_library = RustLibrary {
            common: RustCommon {
                common: Common {
                    name: if index.is_root_package(pkg) {
                        index.public_rule_name(pkg)
                    } else {
                        index.private_rule_name(pkg)
                    },
                    visibility: if index.is_root_package(pkg) {
                        fixups.public_visibility()
                    } else {
                        Visibility::Private
                    },
                    licenses,
                    compatible_with: vec![],
                },
                krate: tgt.name.replace('-', "_"),
                crate_root: BuckPath(crate_root),
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
        };

        rules.push(if index.is_root_package(pkg) {
            Rule::RootPackage(rust_library)
        } else {
            Rule::Library(rust_library)
        });

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
                crate_root: BuckPath(crate_root),
                edition,
                base: PlatformRustCommon {
                    // don't use fixed ones because it will be a cyclic dependency
                    rustc_flags: Default::default(),
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
                crate_root: BuckPath(crate_root),
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
        log::warn!("pkg {} target {} Skipping {:?}", pkg, tgt.name, tgt.kind());

        vec![]
    };

    if let Some(ExportSources {
        name,
        srcs,
        exclude,
        visibility,
    }) = fixups.export_sources()
    {
        // For non-disk sources (i.e. non-vendor mode git_fetch and
        // http_archive), `srcs` and `exclude` are ignored because
        // we can't look at the files to match globs.
        let srcs = if matches!(config.vendor, VendorConfig::Source(_))
            || matches!(pkg.source, Source::Local)
        {
            // e.g. {"src/lib.rs": "vendor/foo-1.0.0/src/lib.rs"}
            let mut globs = Globs::new(srcs, exclude).context("export sources")?;
            let srcs = globs
                .walk(manifest_dir)
                .map(|path| {
                    let source = mapped_manifest_dir.join(&path);
                    (BuckPath(path), SubtargetOrPath::Path(BuckPath(source)))
                })
                .collect();
            if config.strict_globs {
                globs.check_all_globs_used()?;
            }
            srcs
        } else if let VendorConfig::LocalRegistry = config.vendor {
            // e.g. {":foo-1.0.0.git": "foo-1.0.0"}
            let extract_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
            [(
                BuckPath(mapped_manifest_dir.clone()),
                SubtargetOrPath::Path(BuckPath(PathBuf::from(extract_archive_target))),
            )]
            .into()
        } else if let Source::Git { repo, .. } = &pkg.source {
            // e.g. {":foo-123.git": "foo-123"}
            let short_name = short_name_for_git_repo(repo)?;
            let git_fetch_target = format!(":{}.git", short_name);
            [(
                BuckPath(mapped_manifest_dir.clone()),
                SubtargetOrPath::Path(BuckPath(PathBuf::from(git_fetch_target))),
            )]
            .into()
        } else {
            // e.g. {":foo-1.0.0.git": "foo-1.0.0"}
            let http_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
            [(
                BuckPath(mapped_manifest_dir.clone()),
                SubtargetOrPath::Path(BuckPath(PathBuf::from(http_archive_target))),
            )]
            .into()
        };
        let rule = Filegroup {
            name: Name(format!("{}-{}", index.private_rule_name(pkg), name)),
            srcs,
            visibility: Visibility::Custom(visibility.to_vec()),
        };
        rules.push(Rule::Filegroup(rule));
    }

    Ok((rules, dep_pkgs))
}

fn buckify_for_universe(
    config: &Config,
    args: &Args,
    paths: &Paths,
    universe: &UniverseName,
) -> anyhow::Result<BTreeSet<Rule>> {
    let universe_config = &config.universe[universe];
    let features = universe_config.features.iter().join(",");
    let (lockfile, metadata) = {
        let _guard = if universe_config.include_crates.is_empty() {
            None
        } else {
            Some(crate::universe::mutate_manifest(
                universe_config,
                &paths.manifest_path,
            ))
        };
        log::info!("Running `cargo metadata` for universe {universe}...");
        measure_time::info_time!("Running `cargo metadata`");
        let is_default = *universe == Default::default();
        cargo_get_lockfile_and_metadata(config, args, paths, features, is_default)?
    };

    log::trace!("Metadata {:#?}", metadata);

    let index = index::Index::new(config.include_top_level, &metadata)?;
    crate::universe::validate_universe_config(universe, universe_config, &index)?;

    let context = &RuleContext {
        config,
        paths,
        index,
        lockfile,
        done: Mutex::new(HashSet::new()),
    };

    let (tx, rx) = mpsc::channel();

    {
        log::info!("Generating buck rules for universe {universe}...");
        measure_time::info_time!("Generating buck rules");
        rayon::scope(move |scope| {
            for &workspace_member in &context.index.workspace_members {
                generate_dep_rules(context, scope, tx.clone(), [
                    (workspace_member, TargetReq::Lib),
                    (workspace_member, TargetReq::EveryBin),
                ]);
            }
        });
    }

    // Collect rules from channel
    let mut rules: BTreeSet<_> = match rx.iter().collect::<anyhow::Result<_>>() {
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

    // Fill in all http_archive rules with all the sub_targets which got
    // mentioned by fixups.
    if !matches!(config.vendor, VendorConfig::Source(_)) {
        let mut need_subtargets = HashMap::<Name, BTreeSet<BuckPath>>::new();
        let mut insert = |subtarget_or_path: &SubtargetOrPath| {
            if let SubtargetOrPath::Subtarget(subtarget) = subtarget_or_path {
                need_subtargets
                    .entry(subtarget.target.clone())
                    .or_insert_with(BTreeSet::new)
                    .insert(subtarget.relative.clone());
            }
        };

        for rule in &rules {
            match rule {
                Rule::Binary(rule) | Rule::BuildscriptBinary(rule) => {
                    rule.common.base.mapped_srcs.keys().for_each(&mut insert);
                    for plat in rule.common.platform.values() {
                        plat.mapped_srcs.keys().for_each(&mut insert);
                    }
                }
                Rule::Library(rule) => {
                    rule.common.base.mapped_srcs.keys().for_each(&mut insert);
                    for plat in rule.common.platform.values() {
                        plat.mapped_srcs.keys().for_each(&mut insert);
                    }
                }
                Rule::CxxLibrary(rule) => {
                    rule.srcs.iter().for_each(&mut insert);
                    rule.headers.iter().for_each(&mut insert);
                    match &rule.exported_headers {
                        SetOrMap::Set(set) => set.iter().for_each(&mut insert),
                        SetOrMap::Map(map) => map.values().for_each(&mut insert),
                    }
                }
                Rule::PrebuiltCxxLibrary(rule) => insert(&rule.static_lib),
                _ => {}
            }
        }

        rules = rules
            .into_iter()
            .map(|mut rule| {
                match &mut rule {
                    Rule::HttpArchive(rule) => {
                        if let Some(need_subtargets) = need_subtargets.remove(&rule.name) {
                            rule.sub_targets = need_subtargets;
                        }
                    }
                    Rule::ExtractArchive(rule) => {
                        if let Some(need_subtargets) = need_subtargets.remove(&rule.name) {
                            rule.sub_targets = need_subtargets;
                        }
                    }
                    _ => {}
                }
                rule
            })
            .collect();
    }
    Ok(rules)
}

pub(crate) fn buckify(
    config: &Config,
    args: &Args,
    paths: &Paths,
    stdout: bool,
) -> anyhow::Result<()> {
    let mut rules = BTreeMap::new();
    for universe in config.universe.keys().cloned() {
        let universe_rules = buckify_for_universe(config, args, paths, &universe)?;
        rules.insert(universe, universe_rules);
    }
    let rules = crate::universe::merge_universes(&config.universe, rules)?;

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
        if !fs::read(&buckpath).is_ok_and(|x| x == out) {
            fs::write(&buckpath, out)
                .with_context(|| format!("write {} file", buckpath.display()))?;
        }
    }

    log::trace!(
        "{} file written to {}",
        config.buck.file_name,
        buckpath.display()
    );

    Ok(())
}

#[cfg(test)]
mod test {
    use super::short_name_for_git_repo;

    #[test]
    fn hashes_with_same_repo_variations() {
        for url in [
            "https://github.com/facebookincubator/reindeer.git",
            "https://github.com/facebookincubator/reindeer",
            "https://github.com/FacebookIncubator/reindeer.git",
        ] {
            assert_eq!(
                short_name_for_git_repo(url).unwrap(),
                "reindeer-09128a716876493e",
                "{url}",
            );
        }
    }

    #[test]
    fn hashes_non_github() {
        assert_eq!(
            short_name_for_git_repo("https://gitlab.com/gilrs-project/gilrs.git").unwrap(),
            "gilrs-bbe0e8b5f013041b",
        );
    }
}
