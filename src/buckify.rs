/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Implement buckification - generate Buck build rules from Cargo metadata

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::io::Write;
use std::iter;
use std::mem;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::mpsc;
use std::thread;

use anyhow::Context;
use anyhow::bail;
use cached::macros::cached;
use cargo::core::PackageId;
use fnv::FnvHasher;
use foldhash::HashMap;
use foldhash::HashSet;
use indoc::writedoc;
use itertools::Itertools;
use url::Url;

use crate::Args;
use crate::Paths;
use crate::buck;
use crate::buck::Alias;
use crate::buck::BuckPath;
use crate::buck::BuildscriptGenrule;
use crate::buck::BuildscriptGenruleManifestDir;
use crate::buck::Common;
use crate::buck::CxxLibrary;
use crate::buck::ExtractArchive;
use crate::buck::Filegroup;
use crate::buck::FilegroupSources;
use crate::buck::GitFetch;
use crate::buck::HttpArchive;
use crate::buck::Name;
use crate::buck::PackageVersion;
use crate::buck::PlatformRustCommon;
use crate::buck::PlatformSources;
use crate::buck::PrebuiltCxxLibrary;
use crate::buck::Rule;
use crate::buck::RuleRef;
use crate::buck::RustBinary;
use crate::buck::RustCommon;
use crate::buck::RustLibrary;
use crate::buck::Sources;
use crate::buck::StringOrPath;
use crate::buck::SubtargetOrPath;
use crate::buck::Visibility;
use crate::cargo::ArtifactKind;
use crate::cargo::Edition;
use crate::cargo::Manifest;
use crate::cargo::ManifestTarget;
use crate::cargo::Source;
use crate::cargo::TargetReq;
use crate::cargo::cargo_get_lockfile_and_metadata;
use crate::collection::Select;
use crate::collection::SetOrMap;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::fixups::ExportSources;
use crate::fixups::FixupsCache;
use crate::glob::GlobSetKind;
use crate::glob::Globs;
use crate::glob::NO_EXCLUDE;
use crate::index::Index;
use crate::lockfile::Lockfile;
use crate::lockfile::LockfilePackage;
use crate::path::normalize_path;
use crate::path::normalized_extend_path;
use crate::path::relative_path;
use crate::platform::PlatformName;
use crate::srcfiles::crate_srcfiles;
use crate::subtarget::CollectSubtargets;
use crate::subtarget::Subtarget;
use crate::tp_metadata::TpMetadata;
use crate::unused::UnusedFixups;
use crate::version_naming::CollisionInfo;

const BUCKIFY_DIAGNOSTICS_ENV: &str = "REINDEER_BUCKIFY_DIAGNOSTICS";
const BUCKIFY_DIAGNOSTIC_STATE_ENV: &str = "REINDEER_BUCKIFY_DIAGNOSTIC_STATE";
static BUCKIFY_DIAGNOSTIC_STATE_LOCK: Mutex<()> = Mutex::new(());

struct BuckifyDiagnosticConfig {
    diagnostics_enabled: bool,
    state_path: Option<PathBuf>,
}

fn buckify_diagnostic_config() -> &'static BuckifyDiagnosticConfig {
    static CONFIG: OnceLock<BuckifyDiagnosticConfig> = OnceLock::new();
    CONFIG.get_or_init(|| BuckifyDiagnosticConfig {
        diagnostics_enabled: env::var_os(BUCKIFY_DIAGNOSTICS_ENV).is_some_and(|value| value != "0"),
        state_path: env::var_os(BUCKIFY_DIAGNOSTIC_STATE_ENV).map(PathBuf::from),
    })
}

fn buckify_diagnostic(message: impl fmt::Display) {
    let config = buckify_diagnostic_config();
    if !config.diagnostics_enabled && config.state_path.is_none() {
        return;
    }

    let message = message.to_string();
    if let Some(state_path) = &config.state_path {
        let _state_guard = BUCKIFY_DIAGNOSTIC_STATE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = fs::write(state_path, format!("{message}\n"));
    }
    if config.diagnostics_enabled {
        eprintln!("[reindeer buckify diag] {message}");
    }
}

pub fn evaluate_for_platforms<Rule, Collection, R>(
    common: &mut Rule,
    perplat: &mut BTreeMap<PlatformName, Rule>,
    platforms: &BTreeSet<&PlatformName>,
    mut evaluate: impl FnMut(&PlatformName) -> anyhow::Result<Collection>,
    mut apply: impl FnMut(&mut Rule, Collection::Item) -> R,
) -> anyhow::Result<()>
where
    Rule: Default,
    Collection: IntoIterator<Item: Eq + Hash>,
{
    let mut entries: Vec<(&PlatformName, Vec<Collection::Item>)> = Vec::new();
    for &platform_name in platforms {
        let collection = evaluate(platform_name)?;
        entries.push((platform_name, Vec::from_iter(collection)));
    }

    let mut platform_multiplicity: HashMap<&Collection::Item, usize> = HashMap::default();
    for (_, collection) in &entries {
        for value in collection {
            *platform_multiplicity.entry(value).or_insert(0) += 1;
        }
    }

    let mut identical_in_every_platform = VecDeque::new();
    for (_, collection) in &entries {
        for value in collection {
            identical_in_every_platform.push_back(platform_multiplicity[value] == platforms.len());
        }
    }

    for (i, (platform_name, collection)) in entries.into_iter().enumerate() {
        for value in collection {
            if identical_in_every_platform.pop_front().unwrap() {
                if i == 0 {
                    apply(common, value);
                }
            } else {
                apply(
                    perplat
                        .entry(platform_name.clone())
                        .or_insert_with(Rule::default),
                    value,
                );
            }
        }
    }
    Ok(())
}

/// Constant context for generating rules
struct RuleContext<'meta> {
    config: &'meta Config,
    paths: &'meta Paths,
    index: Index<'meta>,
    lockfile: &'meta Lockfile,
    fixups: FixupsCache<'meta>,
    collision_info: CollisionInfo,
    done: Mutex<HashSet<(PackageId, TargetReq<'meta>)>>,
}

/// Generate rules for a set of dependencies
/// This is the top-level because the overall structure is that we're
/// generating rules for the top-level pseudo-package.
fn generate_dep_rules<'scope, 'env>(
    context: &'env RuleContext<'env>,
    scope: &'scope thread::Scope<'scope, 'env>,
    rule_tx: mpsc::Sender<anyhow::Result<Rule>>,
    pkg_deps: impl IntoIterator<Item = (&'env Manifest, TargetReq<'env>)>,
) {
    let mut done = context.done.lock().unwrap();
    for (pkg, target_req) in pkg_deps {
        if done.insert((pkg.id, target_req)) {
            buckify_diagnostic(format_args!(
                "schedule package={pkg} target_req={target_req:?}"
            ));
            let rule_tx = rule_tx.clone();
            scope.spawn(move || {
                generate_rules(context, scope, rule_tx, pkg, target_req);
            });
        }
    }
}

/// Generate rules for all of a package's targets
fn generate_rules<'scope, 'env>(
    context: &'env RuleContext<'env>,
    scope: &'scope thread::Scope<'scope, 'env>,
    rule_tx: mpsc::Sender<anyhow::Result<Rule>>,
    pkg: &'env Manifest,
    target_req: TargetReq<'env>,
) {
    buckify_diagnostic(format_args!(
        "start package={pkg} target_req={target_req:?}"
    ));
    if let TargetReq::Sources = target_req {
        match generate_nonvendored_sources_archive(context, pkg) {
            Ok(rules) => {
                for rule in rules {
                    let _ = rule_tx.send(Ok(rule));
                }
            }
            Err(err) => {
                let _ = rule_tx.send(Err(err));
            }
        }
        buckify_diagnostic(format_args!(
            "finish package={pkg} target_req={target_req:?} sources-only"
        ));
        return;
    }

    // If this crate is mapped to an external target, generate alias(es)
    // instead of building from vendored sources. A crate gets an external
    // target when:
    //   1. extern_crates config is present, AND
    //   2. the crate is NOT a local/path dependency (Source::Local), AND
    //   3. the crate is NOT in the explicit `vendored` set.
    if let Some(extern_config) = &context.config.extern_crates {
        if !matches!(pkg.source, Source::Local) && !extern_config.vendored.contains(&pkg.name) {
            // Only generate aliases for library target requests.
            // Build scripts, binaries, etc. are not needed for extern crates.
            if matches!(target_req, TargetReq::Lib) {
                let extern_target = extern_config.target.replace("{name}", &pkg.name);
                let extern_disp = context.collision_info.target_display(pkg);

                // Alias from versioned name (e.g. "rand-0.9") to external target.
                let _ = rule_tx.send(Ok(Rule::Alias(Alias {
                    owner: PackageVersion {
                        name: pkg.name.clone(),
                        version: pkg.version.clone(),
                    },
                    name: Name(extern_disp),
                    actual: RuleRef::new(extern_target),
                    platforms: None,
                    target_compatible_with: Select::default(),
                    visibility: Visibility::Private,
                    sort_key: Name(pkg.to_string()),
                })));
            }
            // Don't recurse into dependencies - the external target handles them.
            buckify_diagnostic(format_args!(
                "finish package={pkg} target_req={target_req:?} extern-alias"
            ));
            return;
        }
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

        let will_use_rules = {
            let is_private_root_pkg =
                context.index.is_root_package(pkg) && !context.index.is_public_package(pkg);
            let is_ignored_workspace_package = !context.config.include_workspace_members
                && context.index.is_workspace_package(pkg);
            !is_private_root_pkg && !is_ignored_workspace_package
        };

        match generate_target_rules(context, pkg, tgt, will_use_rules) {
            Ok((rules, _)) if rules.is_empty() => {
                // Don't generate rules for dependencies if we're not emitting
                // any rules for this target.
            }
            Ok((rules, mut deps)) => {
                if will_use_rules {
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
    buckify_diagnostic(format_args!(
        "finish package={pkg} target_req={target_req:?}"
    ));
}

fn generate_nonvendored_sources_archive(
    context: &RuleContext,
    pkg: &Manifest,
) -> anyhow::Result<Vec<Rule>> {
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
        Source::Local => Ok(vec![]),
        Source::CratesIo => match context.config.vendor {
            VendorConfig::Off => {
                Ok(vec![generate_http_archive(context, pkg, lockfile_package)?])
            }
            VendorConfig::LocalRegistry => Ok(vec![generate_extract_archive(pkg)]),
            VendorConfig::Source(_) => unreachable!(),
        },
        Source::Git {
            repo, commit_hash, ..
        } => match context.config.vendor {
            VendorConfig::Off => generate_git_fetch(context, pkg, repo, commit_hash),
            VendorConfig::LocalRegistry => Ok(vec![generate_extract_archive(pkg)]),
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

fn generate_extract_archive(pkg: &Manifest) -> Rule {
    Rule::ExtractArchive(ExtractArchive {
        owner: PackageVersion {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        },
        name: Name(format!("{}-{}.crate", pkg.name, pkg.version)),
        src: BuckPath(PathBuf::from(format!(
            "vendor/{}-{}.crate",
            pkg.name, pkg.version,
        ))),
        strip_prefix: format!("{}-{}", pkg.name, pkg.version),
        sub_targets: BTreeSet::new(), // populated later after all fixups are constructed
        visibility: Visibility::Private,
        sort_key: Name(format!("{}-{}", pkg.name, pkg.version)),
    })
}

fn generate_http_archive(
    context: &RuleContext,
    pkg: &Manifest,
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
        owner: PackageVersion {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        },
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

fn generate_git_fetch(
    context: &RuleContext,
    pkg: &Manifest,
    repo: &str,
    commit_hash: &str,
) -> anyhow::Result<Vec<Rule>> {
    let paths = context.paths;
    let short_name = short_name_for_git_repo(repo)?;
    let git_target = format!("{}.git", short_name);

    // One git repo can source several crates, so the shared git_fetch stays in
    // the top-level BUCK. In split mode each consuming crate gets a package-local
    // alias, so its `:{short}.git` / `:{short}.git[path]` references resolve, and
    // the git_fetch must be visible to those crate packages.
    if context.config.buck.split {
        let prefix = if paths.buck_package.is_empty() {
            String::new()
        } else {
            format!("{}/", paths.buck_package)
        };
        let mut rules = Vec::with_capacity(2);
        rules.push(Rule::GitFetch(GitFetch {
            name: Name(git_target.clone()),
            repo: repo.to_owned(),
            rev: commit_hash.to_owned(),
            sub_targets: BTreeSet::new(), // populated later after all fixups are constructed
            visibility: Visibility::Custom(vec![format!("//{prefix}vendor/...")]),
        }));
        rules.push(Rule::GitFetchAlias(Alias {
            owner: PackageVersion {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
            },
            name: Name(git_target.clone()),
            actual: RuleRef::new(format!("//{}:{git_target}", paths.buck_package)),
            platforms: None,
            target_compatible_with: Select::default(),
            visibility: Visibility::Private,
            sort_key: Name(git_target),
        }));
        Ok(rules)
    } else {
        Ok(vec![Rule::GitFetch(GitFetch {
            name: Name(git_target),
            repo: repo.to_owned(),
            rev: commit_hash.to_owned(),
            sub_targets: BTreeSet::new(), // populated later after all fixups are constructed
            visibility: Visibility::Private,
        })])
    }
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
fn srcfiles(manifest_dir: PathBuf, crate_root: PathBuf) -> Vec<PathBuf> {
    let sources = crate_srcfiles(&manifest_dir, &crate_root);
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

/// Whether the crate's sources are files in the repo (vendored sources or a
/// local path dependency), as opposed to being produced at build time by an
/// http_archive / extract_archive / git_fetch rule.
pub(crate) fn sources_are_local(config: &Config, pkg: &Manifest) -> bool {
    matches!(config.vendor, VendorConfig::Source(_)) || matches!(pkg.source, Source::Local)
}

/// Visibility scoped to a single crate's own vendor directory.
///
/// In split mode the generated `srcs`/filegroup rules live in
/// `vendor/{name}-{version}/`, but their only consumers (the crate's library,
/// binary, and build-script targets) are written to `vendor/{name}/`. Scoping
/// these helper rules to `//{buck_package}/vendor/{name}:` keeps them reachable
/// by that crate's targets while keeping them private from the rest of the
/// vendor tree.
pub(crate) fn vendor_crate_visibility(paths: &Paths, crate_name: &str) -> Visibility {
    let prefix = if paths.buck_package.is_empty() {
        String::new()
    } else {
        format!("{}/", paths.buck_package)
    };
    Visibility::Custom(vec![format!("//{prefix}vendor/{crate_name}:")])
}

pub(crate) fn split_srcs(
    paths: &Paths,
    common: &mut RustCommon,
    owner: &PackageVersion,
    srcs_name: &Name,
) -> (PlatformSources, BTreeMap<PlatformName, PlatformSources>) {
    common.srcs_filegroup = Some(RuleRef::new(format!(
        "//{}{}vendor/{}-{}:{}",
        paths.buck_package,
        if paths.buck_package.is_empty() {
            ""
        } else {
            "/"
        },
        owner.name,
        owner.version,
        srcs_name,
    )));

    fn rewrite_src(src: &BuckPath) -> BuckPath {
        // "vendor/cxx-1.0.100/src/lib.rs" => "src/lib.rs"
        BuckPath(src.0.components().skip(2).collect())
    }

    common.crate_root = rewrite_src(&common.crate_root);

    let rewrite_platform_sources = |common: &mut PlatformRustCommon| -> PlatformSources {
        let mut srcs = BTreeSet::new();
        for src in mem::take(&mut common.srcs) {
            srcs.insert(rewrite_src(&src));
        }

        // "fixups/cxx/overlay/src/lib.rs" => "//third-party/rust/fixups/cxx/overlay:src/lib.rs"
        let mut mapped_srcs = BTreeMap::new();
        for (src, mapped) in mem::take(&mut common.mapped_srcs) {
            let SubtargetOrPath::Path(src) = src else {
                unimplemented!();
            };
            mapped_srcs.insert(
                SubtargetOrPath::Path(split_fixups_mapped_src_key(paths, &src)),
                rewrite_src(&mapped),
            );
        }

        PlatformSources { srcs, mapped_srcs }
    };

    let mut platform_srcs = BTreeMap::new();
    for (platform_name, platform) in &mut common.platform {
        if !platform.srcs.is_empty() || !platform.mapped_srcs.is_empty() {
            platform_srcs.insert(platform_name.clone(), rewrite_platform_sources(platform));
        }
    }

    (rewrite_platform_sources(&mut common.base), platform_srcs)
}

/// Turn a mapped-src key that points into the fixups tree into a
/// package-absolute label, because in split mode `fixups/<crate>/<overlay>/` is
/// its own buck package. For example
/// "fixups/cxx/overlay/src/lib.rs" => "//third-party/rust/fixups/cxx/overlay:src/lib.rs".
fn split_fixups_mapped_src_key(paths: &Paths, src: &BuckPath) -> BuckPath {
    let mut components = src.0.components();
    assert_eq!(
        components.next(),
        Some(Component::Normal(OsStr::new("fixups"))),
    );
    let rewrite = format!(
        "//{}{}fixups/{}/{}:{}",
        paths.buck_package,
        if paths.buck_package.is_empty() {
            ""
        } else {
            "/"
        },
        components.next().unwrap().as_os_str().to_str().unwrap(),
        components.next().unwrap().as_os_str().to_str().unwrap(),
        components.as_path().to_str().unwrap(),
    );
    BuckPath(PathBuf::from(rewrite))
}

/// Split mode, non-vendored crates: srcs stay archive-target references in the
/// crate's own package; only mapped_srcs keys that point into the fixups tree
/// must become package-absolute labels (the overlay BUCK files are written
/// unconditionally in split mode, so those packages always exist).
pub(crate) fn relabel_fixups_mapped_srcs(paths: &Paths, common: &mut RustCommon) {
    for platform in iter::once(&mut common.base).chain(common.platform.values_mut()) {
        relabel_platform_fixups_mapped_srcs(paths, platform);
    }
}

fn relabel_platform_fixups_mapped_srcs(paths: &Paths, platform: &mut PlatformRustCommon) {
    platform.mapped_srcs = mem::take(&mut platform.mapped_srcs)
        .into_iter()
        .map(|(key, value)| {
            let key = match key {
                SubtargetOrPath::Path(ref p)
                    if p.0.components().next()
                        == Some(Component::Normal(OsStr::new("fixups"))) =>
                {
                    SubtargetOrPath::Path(split_fixups_mapped_src_key(paths, p))
                }
                // Subtarget into the archive, or user-written label: already correct.
                other => other,
            };
            (key, value)
        })
        .collect();
}

fn buckify_overlay_export_files(config: &Config, paths: &Paths) -> String {
    let mut overlay_export_files = String::new();
    overlay_export_files.push_str(&config.buck.generated_file_header);
    if !config.buck.generated_file_header.is_empty() {
        overlay_export_files.push('\n');
    }

    // FIXME: consider restricting to specific package versions that use
    // this overlay, such as `["//third-party/rust/vendor/openssl-0.10.0:"]`.
    let visibility = if paths.buck_package.is_empty() {
        "//vendor/...".to_owned()
    } else {
        format!("//{}/vendor/...", paths.buck_package)
    };

    // Make overlay sources accessible to targets in vendor directory.
    writedoc!(
        overlay_export_files,
        r#"
            [
                export_file(
                    name = name,
                    visibility = ["{visibility}"],
                )
                for name in glob(["**"], exclude = ["{buck}"])
            ]
        "#,
        buck = config.buck.file_name,
    )
    .unwrap();

    overlay_export_files
}

fn buckify_cxx_library_fixup_include_paths(config: &Config, paths: &Paths) -> String {
    let mut include_filegroup = String::new();
    include_filegroup.push_str(&config.buck.generated_file_header);
    if !config.buck.generated_file_header.is_empty() {
        include_filegroup.push('\n');
    }

    // FIXME: consider restricting to specific package versions that use
    // this overlay, such as `["//third-party/rust/vendor/openssl-0.10.0:"]`.
    let visibility = if paths.buck_package.is_empty() {
        "//vendor/...".to_owned()
    } else {
        format!("//{}/vendor/...", paths.buck_package)
    };

    // Make cxx_library fixup_include_paths accessible to vendor directory.
    writedoc!(
        include_filegroup,
        r#"
            filegroup(
                name = "include",
                srcs = glob(["**"], exclude = ["{buck}"]),
                visibility = ["{visibility}"],
            )
        "#,
        buck = config.buck.file_name,
    )
    .unwrap();

    include_filegroup
}

/// Generate rules for a target. Returns the rules, and the
/// packages we depend on for further rule generation.
fn generate_target_rules<'a>(
    context: &'a RuleContext<'a>,
    pkg: &'a Manifest,
    tgt: &'a ManifestTarget,
    will_use_rules: bool,
) -> anyhow::Result<(Vec<Rule>, Vec<(&'a Manifest, TargetReq<'a>)>)> {
    let RuleContext {
        config,
        paths,
        index,
        collision_info,
        ..
    } = context;

    // Version string for target names (e.g. "0.9")
    let tgt_ver = collision_info.target_version(pkg);
    // "name-version" string for target names (e.g. "rand-0.9")
    let tgt_disp = collision_info.target_display(pkg);

    log::debug!("Generating rules for package {} target {}", pkg, tgt.name);
    buckify_diagnostic(format_args!(
        "target start package={pkg} target={}",
        tgt.name
    ));

    let fixups = context.fixups.get(pkg)?;
    if fixups.omit_target(tgt) {
        return Ok((vec![], vec![]));
    }

    log::debug!("pkg {} target {} fixups {:#?}", pkg, tgt.name, fixups);

    let manifest_dir = pkg.manifest_dir();
    let mut manifest_dir_subtarget = None;
    let mapped_manifest_dir;
    let mut crate_root;
    if sources_are_local(config, pkg) {
        let relative_manifest_dir = match manifest_dir
            .strip_prefix(&paths.third_party_dir)
			.with_context(|| format!(
                "crate sources would be inaccessible from the generated BUCK file, cannot refer to {} from {}.",
                relative_path(&paths.third_party_dir, manifest_dir).display(),
                paths.third_party_dir.join(&config.buck.file_name).display(),
            ))
        {
            Err(_) if !will_use_rules => Path::new("__unused__"),
            res => res?,
        };
        if !matches!(config.vendor, VendorConfig::Source(_)) || matches!(pkg.source, Source::Local)
        {
            manifest_dir_subtarget = Some(SubtargetOrPath::Path(BuckPath(
                relative_manifest_dir.to_owned(),
            )));
        }
        mapped_manifest_dir = relative_manifest_dir.to_owned();
        crate_root = relative_manifest_dir.to_owned();
        normalized_extend_path(&mut crate_root, relative_path(manifest_dir, &tgt.src_path));
    } else {
        mapped_manifest_dir = if let VendorConfig::LocalRegistry = config.vendor {
            PathBuf::from(format!("{}-{}.crate", pkg.name, pkg.version))
        } else if let Source::Git { repo, .. } = &pkg.source {
            let short_name = short_name_for_git_repo(repo)?;
            let repository_root = find_repository_root(manifest_dir)?;
            let path_within_repo = relative_path(repository_root, manifest_dir);
            manifest_dir_subtarget = Some(SubtargetOrPath::Subtarget(Subtarget {
                target: Name(format!("{}.git", short_name)),
                relative: BuckPath(path_within_repo.clone()),
            }));
            let mut res = PathBuf::from(short_name);
            if path_within_repo.components().next().is_some() {
                // only do this if we have an actual path to avoid the trailing slash
                res.push(path_within_repo)
            }
            res
        } else {
            PathBuf::from(format!("{}-{}.crate", pkg.name, pkg.version))
        };
        crate_root = mapped_manifest_dir.join(relative_path(manifest_dir, &tgt.src_path));
    }

    let edition = tgt.edition.unwrap_or(pkg.edition);

    let mut licenses = BTreeSet::new();
    let license_globs = Globs::new(&config.license_patterns, NO_EXCLUDE);
    if config.buck.split {
        // The `licenses` attribute is not currently implemented for split mode.
    } else if !matches!(config.vendor, VendorConfig::Source(_)) {
        // The `licenses` attribute takes `attrs.source()` which is the file
        // containing the custom license text. For `vendor = false` mode, we
        // don't have such a file on disk, and we don't have a Buck label either
        // that could refer to the right generated location following download
        // because `http_archive` does not expose subtargets for each of the
        // individual contained files.
        //
        // But we validate globs anyway.
        license_globs.walk(manifest_dir)?;
    } else {
        let rel_manifest = relative_path(&paths.third_party_dir, manifest_dir);
        for path in license_globs.walk(manifest_dir)? {
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

    // Platform-specific rule bits which are common to all platforms
    let mut base = PlatformRustCommon::default();
    // Per platform rule bits
    let mut perplat: BTreeMap<PlatformName, PlatformRustCommon> = BTreeMap::new();
    let compatible_platforms = index.compatible_platforms(pkg);

    if config.buck.platform_compatibility_on_all_targets {
        for &platform in &compatible_platforms {
            perplat.insert(platform.clone(), PlatformRustCommon::default());
        }
    }

    if sources_are_local(config, pkg) {
        // Get a list of the most obvious sources for the crate. This is either
        // a list of filename, or a list of globs.
        let mut srcs = Vec::new();

        // If we're configured to get precise sources and we're using 2018+ edition
        // source, then parse the crate to see what files are actually used.
        if fixups.precise_srcs() && edition >= Edition::Rust2018 {
            measure_time::trace_time!("srcfiles for {}", pkg);
            buckify_diagnostic(format_args!(
                "srcfiles start package={pkg} target={} crate_root={}",
                tgt.name,
                tgt.src_path.display()
            ));
            srcs = srcfiles(manifest_dir.to_owned(), tgt.src_path.clone());
            buckify_diagnostic(format_args!(
                "srcfiles finish package={pkg} target={} files={}",
                tgt.name,
                srcs.len()
            ));
        }

        if srcs.is_empty() {
            // If that didn't work out, get srcs the globby way
            let dir_containing_src = tgt.src_path.parent().unwrap();
            let pattern = relative_path(manifest_dir, dir_containing_src).join("**/*.rs");
            let glob = Globs::new(GlobSetKind::from_iter([pattern]).unwrap(), NO_EXCLUDE);
            srcs.extend(glob.walk(manifest_dir)?);
        }

        evaluate_for_platforms(
            &mut base,
            &mut perplat,
            &compatible_platforms,
            |platform| fixups.compute_srcs(tgt, platform, &srcs),
            |rule, src| rule.srcs.insert(BuckPath(src)),
        )?;
    } else {
        for platform in &compatible_platforms {
            // Although `extra_srcs` and `omit_srcs` do not affect the output
            // when we are not producing a srcs list, still evaluate globs in
            // the fixup against the contents of the manifest directory so that
            // unmatched globs are accurately reported.
            fixups.validate_srcs_fixups(tgt, platform)?;
        }
        if let VendorConfig::LocalRegistry = config.vendor {
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
    }

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| fixups.compute_mapped_srcs(&mapped_manifest_dir, tgt, platform),
        |rule, (key, value)| rule.mapped_srcs.insert(key, value),
    )?;

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| Ok(fixups.compute_features(platform, index)),
        |rule, feature| rule.features.insert(feature.to_owned()),
    )?;

    // Compute set of dependencies any rule we generate here will need. They will only
    // be emitted if we actually emit some rules below.
    let mut platform_deps = Vec::new();
    for &platform_name in &compatible_platforms {
        let deps = fixups.compute_deps(platform_name, index, tgt, collision_info)?;
        platform_deps.push((platform_name, deps));
    }

    let mut dep_in_how_many_platforms = HashMap::default();
    for (_platform_name, deps) in &platform_deps {
        for key @ (dep, _rename, dep_kind) in deps.keys() {
            if let TargetReq::Cdylib = dep_kind.target_req() {
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

            *dep_in_how_many_platforms.entry(key).or_insert(0) += 1;
        }
    }

    let mut dep_pkgs = Vec::new();
    for &(platform_name, ref deps) in &platform_deps {
        for (key @ &(ref dep, rename, dep_kind), &manifest) in deps {
            let recipient = if dep_in_how_many_platforms[&key] == compatible_platforms.len() {
                &mut base
            } else {
                perplat
                    .entry(platform_name.clone())
                    .or_insert_with(PlatformRustCommon::default)
            };
            if let Some(ArtifactKind::Bin(_) | ArtifactKind::EveryBin) = dep_kind.artifact {
                let bin_name = dep_kind.bin_name.as_ref().unwrap();
                let env = if config.buck.split {
                    format!("{}-{}", manifest.unwrap(), bin_name)
                } else {
                    let target_name = dep.target.strip_prefix(':').unwrap();
                    format!("{}-{}", target_name, bin_name)
                };
                let location = format!("$(location {}-{}[check])", dep.target, bin_name);
                recipient.env.insert(env, StringOrPath::String(location));
            } else if let Some(rename) = rename {
                recipient.named_deps.insert(rename.to_owned(), dep.clone());
            } else {
                recipient.deps.insert(dep.clone());
            }
            if let Some(manifest) = manifest {
                dep_pkgs.push((manifest, dep_kind.target_req()));
            }
        }
    }

    let mut metadata = BTreeMap::new();
    if config.third_party_metadata {
        metadata.insert(
            "third-party.metadata".to_owned(),
            Box::new(TpMetadata::new(pkg)) as Box<dyn buck::Metadata>,
        );
    }

    // If this is a build script, we only apply fixups pertaining to srcs
    // (extra_srcs), mapped_srcs (overlay), and features. Return early before
    // applying any other fixups (such as rustc_flags, env, or link_style).
    // Every other fixup handled after this point should only apply to the
    // library or binary, not a build script.
    if tgt.crate_bin() && tgt.kind_custom_build() {
        let buildscript = RustBinary {
            owner: PackageVersion {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
            },
            common: RustCommon {
                common: Common {
                    name: Name(if config.buck.split {
                        format!("{}-{}", tgt_ver, tgt.name)
                    } else {
                        format!("{}-{}", tgt_disp, tgt.name)
                    }),
                    visibility: Visibility::Private,
                    licenses: Default::default(),
                    metadata,
                    compatible_with: vec![],
                    target_compatible_with: Select {
                        common: vec![],
                        selects: vec![],
                    },
                },
                krate: tgt.name.replace('-', "_"),
                srcs_filegroup: None,
                crate_root: BuckPath(crate_root),
                edition,
                base,
                platform: perplat,
                serialize_all_platforms: config.buck.platform_compatibility_on_all_targets,
            },
        };
        let rules = fixups.emit_buildscript_rules(
            buildscript,
            config,
            manifest_dir_subtarget,
            index,
            tgt,
            &compatible_platforms,
            collision_info,
        )?;
        buckify_diagnostic(format_args!(
            "target finish package={pkg} target={} rules={} deps={}",
            tgt.name,
            rules.len(),
            dep_pkgs.len()
        ));
        return Ok((rules, dep_pkgs));
    }

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| Ok(fixups.compute_rustc_cfg(platform)),
        |rule, cfg| rule.rustc_flags.common.push(format!("--cfg={cfg}")),
    )?;

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| Ok(Some(fixups.compute_rustc_flags(platform))),
        |rule, flags| rule.rustc_flags.common.extend(flags),
    )?;

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| Ok(fixups.compute_rustc_flags_select(platform)),
        |rule, rustc_flags_select| rule.rustc_flags.selects.push(rustc_flags_select),
    )?;

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| {
            Ok(if fixups.has_buildscript_for_platform(platform) {
                Some(())
            } else {
                None
            })
        },
        |rule, ()| {
            let genrule = fixups.buildscript_genrule_name(collision_info);
            rule.rustc_flags
                .common
                .push(format!("@$(location :{genrule}[rustc_flags])"));
            rule.env.insert(
                "OUT_DIR".to_owned(),
                StringOrPath::String(format!("$(location :{genrule}[out_dir])")),
            );
        },
    )?;

    evaluate_for_platforms(
        &mut base,
        &mut perplat,
        &compatible_platforms,
        |platform| fixups.compute_env(tgt, platform),
        |rule, (key, value)| rule.env.insert(key, value),
    )?;

    // "link_style" only really applies to binaries, so maintain separate binary base & perplat
    let mut bin_base = base.clone();
    let mut bin_perplat = perplat.clone();

    evaluate_for_platforms(
        &mut bin_base,
        &mut bin_perplat,
        &compatible_platforms,
        |platform| Ok(fixups.compute_link_style(platform)),
        |rule, link_style| rule.link_style = Some(link_style),
    )?;

    evaluate_for_platforms(
        &mut bin_base,
        &mut bin_perplat,
        &compatible_platforms,
        |platform| Ok(Some(fixups.compute_linker_flags(platform))),
        |rule, linker_flags| rule.linker_flags.extend(linker_flags),
    )?;

    // "preferred_linkage" only really applies to libraries, so maintain separate library base &
    // perplat
    let mut lib_base = base.clone();
    let mut lib_perplat = perplat.clone();

    evaluate_for_platforms(
        &mut lib_base,
        &mut lib_perplat,
        &compatible_platforms,
        |platform| Ok(fixups.compute_preferred_linkage(platform)),
        |rule, preferred_linkage| rule.preferred_linkage = Some(preferred_linkage),
    )?;

    // Standalone binary - binary for a package always takes the package's library as a dependency
    // if there is one
    if let Some(true) = pkg.dependency_target().map(ManifestTarget::kind_lib) {
        bin_base.deps.insert(RuleRef::new(if config.buck.split {
            format!(":{}", tgt_ver)
        } else {
            format!(":{}", tgt_disp)
        }));
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
            let platforms =
                if !config.buck.alias_with_platforms.is_default && !tgt.crate_proc_macro() {
                    Some(
                        compatible_platforms
                            .iter()
                            .map(|&platform_name| platform_name.clone())
                            .collect(),
                    )
                } else {
                    None
                };
            rules.push(Rule::Alias(Alias {
                owner: PackageVersion {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                },
                name: index.public_rule_name(pkg),
                actual: RuleRef::new(if config.buck.split {
                    format!(
                        "//{}{}vendor/{}:{}",
                        paths.buck_package,
                        if paths.buck_package.is_empty() {
                            ""
                        } else {
                            "/"
                        },
                        pkg.name,
                        tgt_ver,
                    )
                } else {
                    format!(":{}", tgt_disp)
                }),
                platforms,
                target_compatible_with: Select {
                    common: fixups.target_compatible_with().clone(),
                    selects: fixups.compute_target_compatible_with_select(),
                },
                visibility: fixups.visibility(index),
                sort_key: Name(tgt_disp.clone()),
            }));
        }

        let mut rust_library = RustLibrary {
            owner: PackageVersion {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
            },
            common: RustCommon {
                common: Common {
                    name: if index.is_root_package(pkg) {
                        index.public_rule_name(pkg)
                    } else if config.buck.split {
                        Name(tgt_ver.clone())
                    } else {
                        Name(tgt_disp.clone())
                    },
                    visibility: if index.is_root_package(pkg) {
                        fixups.visibility(index)
                    } else if config.buck.split {
                        Visibility::Custom(vec![
                            format!("//{}:", paths.buck_package),
                            format!(
                                "//{}{}vendor/...",
                                paths.buck_package,
                                if paths.buck_package.is_empty() {
                                    ""
                                } else {
                                    "/"
                                },
                            ),
                        ])
                    } else {
                        Visibility::Private
                    },
                    licenses,
                    metadata,
                    compatible_with: fixups.compatible_with().clone(),
                    target_compatible_with: Select {
                        common: fixups.target_compatible_with().clone(),
                        selects: fixups.compute_target_compatible_with_select(),
                    },
                },
                krate: tgt.name.replace('-', "_"),
                srcs_filegroup: None,
                crate_root: BuckPath(crate_root),
                edition,
                base: lib_base,
                platform: lib_perplat,
                serialize_all_platforms: config.buck.platform_compatibility_on_all_targets,
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

        if index.is_root_package(pkg) {
            if config.buck.split {
                let crate_root = rust_library.common.crate_root.0.parent().unwrap();
                rules.push(Rule::Alias(Alias {
                    owner: PackageVersion {
                        name: pkg.name.clone(),
                        version: pkg.version.clone(),
                    },
                    name: Name(pkg.name.clone()),
                    actual: RuleRef::new(format!(
                        "//{}{}{}:{}",
                        paths.buck_package,
                        if paths.buck_package.is_empty() {
                            ""
                        } else {
                            "/"
                        },
                        crate_root.to_str().unwrap(),
                        pkg.name,
                    )),
                    platforms: None,
                    target_compatible_with: Select::default(),
                    visibility: fixups.visibility(index),
                    sort_key: Name(pkg.to_string()),
                }));
                rust_library.common.base.srcs = rust_library
                    .common
                    .base
                    .srcs
                    .into_iter()
                    .map(|path| {
                        BuckPath(
                            path.0
                                .components()
                                .skip(crate_root.components().count())
                                .collect(),
                        )
                    })
                    .collect();
                rust_library.common.crate_root = BuckPath(PathBuf::from(
                    rust_library.common.crate_root.0.file_name().unwrap(),
                ));
            }
            rules.push(Rule::RootPackage(rust_library));
        } else {
            if config.buck.split {
                if sources_are_local(config, pkg) {
                    let srcs_name = Name("srcs".to_owned());
                    let (base, platform) = split_srcs(
                        paths,
                        &mut rust_library.common,
                        &rust_library.owner,
                        &srcs_name,
                    );
                    rules.push(Rule::Sources(Sources {
                        owner: rust_library.owner.clone(),
                        name: srcs_name,
                        base,
                        platform,
                        visibility: vendor_crate_visibility(paths, &rust_library.owner.name),
                    }));
                } else {
                    // No on-disk sources: the srcs stay archive-target
                    // references, resolved in the crate's own package.
                    relabel_fixups_mapped_srcs(paths, &mut rust_library.common);
                }
            }
            rules.push(Rule::Library(rust_library));
        }

        // Library depends on the build script (if there is one).
        dep_pkgs.push((pkg, TargetReq::BuildScript));

        rules
    } else if tgt.kind_bin() && tgt.crate_bin() {
        let mut rules = vec![];

        if index.is_public_target(pkg, TargetReq::Bin(&tgt.name)) {
            let platforms = if !config.buck.alias_with_platforms.is_default {
                Some(
                    compatible_platforms
                        .iter()
                        .map(|&platform_name| platform_name.clone())
                        .collect(),
                )
            } else {
                None
            };
            rules.push(Rule::Alias(Alias {
                owner: PackageVersion {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                },
                name: Name(format!("{}-{}", index.public_rule_name(pkg), tgt.name)),
                actual: RuleRef::new(if config.buck.split {
                    format!(
                        "//{}{}vendor/{}:{}-{}",
                        paths.buck_package,
                        if paths.buck_package.is_empty() {
                            ""
                        } else {
                            "/"
                        },
                        pkg.name,
                        tgt_ver,
                        tgt.name,
                    )
                } else {
                    format!(":{}-{}", tgt_disp, tgt.name)
                }),
                platforms,
                target_compatible_with: Select::default(),
                visibility: fixups.visibility(index),
                sort_key: Name(format!("{}-{}", tgt_disp, tgt.name)),
            }));
        }

        let mut rust_binary = RustBinary {
            owner: PackageVersion {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
            },
            common: RustCommon {
                common: Common {
                    name: Name(if config.buck.split {
                        format!("{}-{}", tgt_ver, tgt.name)
                    } else {
                        format!("{}-{}", tgt_disp, tgt.name)
                    }),
                    visibility: if config.buck.split {
                        Visibility::Custom(vec![
                            format!("//{}:", paths.buck_package),
                            format!(
                                "//{}{}vendor/...",
                                paths.buck_package,
                                if paths.buck_package.is_empty() {
                                    ""
                                } else {
                                    "/"
                                },
                            ),
                        ])
                    } else {
                        Visibility::Private
                    },
                    licenses,
                    metadata,
                    compatible_with: fixups.compatible_with().clone(),
                    target_compatible_with: Select {
                        common: fixups.target_compatible_with().clone(),
                        selects: fixups.compute_target_compatible_with_select(),
                    },
                },
                krate: tgt.name.replace('-', "_"),
                srcs_filegroup: None,
                crate_root: BuckPath(crate_root),
                edition,
                base: bin_base,
                platform: bin_perplat,
                serialize_all_platforms: config.buck.platform_compatibility_on_all_targets,
            },
        };

        if config.buck.split {
            if sources_are_local(config, pkg) {
                let srcs_name = Name(format!("bin-{}", tgt.name));
                let (base, platform) = split_srcs(
                    paths,
                    &mut rust_binary.common,
                    &rust_binary.owner,
                    &srcs_name,
                );
                rules.push(Rule::Sources(Sources {
                    owner: rust_binary.owner.clone(),
                    name: srcs_name,
                    base,
                    platform,
                    visibility: vendor_crate_visibility(paths, &rust_binary.owner.name),
                }));
            } else {
                relabel_fixups_mapped_srcs(paths, &mut rust_binary.common);
            }
        }

        rules.push(Rule::Binary(rust_binary));

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

    for ExportSources {
        name,
        srcs,
        exclude,
        visibility,
    } in fixups.export_sources()
    {
        let export_globs = Globs::new(srcs, exclude);

        // For non-disk sources (i.e. non-vendor mode git_fetch and
        // http_archive), `srcs` and `exclude` are ignored because
        // we can't look at the files to match globs.
        let srcs = if sources_are_local(config, pkg) {
            if config.buck.split {
                // e.g. ["src/lib.rs"]
                FilegroupSources::Set(BTreeSet::from_iter(
                    export_globs.walk(manifest_dir)?.into_iter().map(BuckPath),
                ))
            } else {
                // e.g. {"src/lib.rs": "vendor/foo-1.0.0/src/lib.rs"}
                FilegroupSources::Map(BTreeMap::from_iter(
                    export_globs.walk(manifest_dir)?.into_iter().map(|path| {
                        let source = mapped_manifest_dir.join(&path);
                        (BuckPath(path), SubtargetOrPath::Path(BuckPath(source)))
                    }),
                ))
            }
        } else {
            // Validate the globs anyway
            export_globs.walk(manifest_dir)?;

            if let VendorConfig::LocalRegistry = config.vendor {
                // e.g. {":foo-1.0.0.git": "foo-1.0.0"}
                let extract_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
                FilegroupSources::Map(BTreeMap::from([(
                    BuckPath(mapped_manifest_dir.clone()),
                    SubtargetOrPath::Path(BuckPath(PathBuf::from(extract_archive_target))),
                )]))
            } else if let Source::Git { repo, .. } = &pkg.source {
                // e.g. {":foo-123.git": "foo-123"}
                let short_name = short_name_for_git_repo(repo)?;
                let git_fetch_target = format!(":{}.git", short_name);
                FilegroupSources::Map(BTreeMap::from([(
                    BuckPath(mapped_manifest_dir.clone()),
                    SubtargetOrPath::Path(BuckPath(PathBuf::from(git_fetch_target))),
                )]))
            } else {
                // e.g. {":foo-1.0.0.git": "foo-1.0.0"}
                let http_archive_target = format!(":{}-{}.crate", pkg.name, pkg.version);
                FilegroupSources::Map(BTreeMap::from([(
                    BuckPath(mapped_manifest_dir.clone()),
                    SubtargetOrPath::Path(BuckPath(PathBuf::from(http_archive_target))),
                )]))
            }
        };
        if config.buck.split {
            // Vendored crates put the filegroup in the versioned source dir
            // (`vendor/{name}-{version}`), so `filegroup-{name}` is unique
            // there. Non-vendored crates share `vendor/{name}` across versions,
            // so the filegroup name and the alias target it points to must be
            // version-disambiguated.
            let (filegroup_name, filegroup_package) = if sources_are_local(config, pkg) {
                (format!("filegroup-{}", name), pkg.to_string())
            } else {
                (format!("filegroup-{}-{}", tgt_ver, name), pkg.name.clone())
            };
            rules.push(Rule::Filegroup(Filegroup {
                owner: PackageVersion {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                },
                name: Name(filegroup_name.clone()),
                srcs,
                visibility: Visibility::Custom(vec![format!("//{}:", paths.buck_package)]),
            }));
            rules.push(Rule::Alias(Alias {
                owner: PackageVersion {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                },
                name: Name(format!("{}-{}", tgt_disp, name)),
                actual: RuleRef::new(format!(
                    "//{}{}vendor/{}:{}",
                    paths.buck_package,
                    if paths.buck_package.is_empty() {
                        ""
                    } else {
                        "/"
                    },
                    filegroup_package,
                    filegroup_name,
                )),
                platforms: None,
                target_compatible_with: Select::default(),
                visibility: visibility.clone(),
                sort_key: Name(format!("{}-{}", tgt_disp, name)),
            }));
        } else {
            rules.push(Rule::Filegroup(Filegroup {
                owner: PackageVersion {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                },
                name: Name(format!("{}-{}", tgt_disp, name)),
                srcs,
                visibility: visibility.clone(),
            }));
        }
    }

    buckify_diagnostic(format_args!(
        "target finish package={pkg} target={} rules={} deps={}",
        tgt.name,
        rules.len(),
        dep_pkgs.len()
    ));
    Ok((rules, dep_pkgs))
}

fn do_buckify<'a>(context: &'a RuleContext<'a>) -> anyhow::Result<BTreeSet<Rule>> {
    let (tx, rx) = mpsc::channel();

    {
        log::info!("Generating buck rules...");
        measure_time::info_time!("Generating buck rules");
        thread::scope(move |scope| {
            for &workspace_member in &context.index.workspace_members {
                generate_dep_rules(
                    context,
                    scope,
                    tx.clone(),
                    [
                        (workspace_member, TargetReq::Lib),
                        (workspace_member, TargetReq::EveryBin),
                    ],
                );
            }
        });
    }

    // Collect rules from channel
    let mut rules: BTreeSet<_> = match rx.iter().collect::<anyhow::Result<_>>() {
        Ok(rules) => rules,
        Err(err) => {
            if let Some(custom_err_msg) = context.config.unresolved_fixup_error_message.as_ref() {
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
    if !matches!(context.config.vendor, VendorConfig::Source(_)) {
        let mut subtargets = CollectSubtargets::new();

        for rule in &rules {
            match rule {
                Rule::Binary(rule) | Rule::BuildscriptBinary(rule) => {
                    subtargets.insert_all(rule.common.base.mapped_srcs.keys());
                    for plat in rule.common.platform.values() {
                        subtargets.insert_all(plat.mapped_srcs.keys());
                    }
                }
                Rule::Library(rule) => {
                    subtargets.insert_all(rule.common.base.mapped_srcs.keys());
                    for plat in rule.common.platform.values() {
                        subtargets.insert_all(plat.mapped_srcs.keys());
                    }
                }
                Rule::BuildscriptGenrule(rule) => {
                    if let BuildscriptGenruleManifestDir::Subtarget(manifest_dir) =
                        &rule.manifest_dir
                    {
                        subtargets.insert(manifest_dir);
                    }
                }
                Rule::CxxLibrary(rule) => {
                    subtargets.insert_all(&rule.srcs);
                    subtargets.insert_all(&rule.headers);
                    match &rule.exported_headers {
                        SetOrMap::Set(set) => subtargets.insert_all(set),
                        SetOrMap::Map(map) => subtargets.insert_all(map.values()),
                    }
                }
                Rule::PrebuiltCxxLibrary(rule) => subtargets.insert(&rule.static_lib),
                _ => {}
            }
        }

        rules = rules
            .into_iter()
            .map(|mut rule| {
                match &mut rule {
                    Rule::HttpArchive(rule) => {
                        if let Some(need_subtargets) = subtargets.remove(&rule.name) {
                            rule.sub_targets = need_subtargets;
                        }
                    }
                    Rule::ExtractArchive(rule) => {
                        if let Some(need_subtargets) = subtargets.remove(&rule.name) {
                            rule.sub_targets = need_subtargets;
                        }
                    }
                    Rule::GitFetch(rule) => {
                        if let Some(need_subtargets) = subtargets.remove(&rule.name) {
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

/// Which split-mode BUCK file a rule is written to.
enum SplitDestination<'a> {
    /// The third-party-dir top-level BUCK file.
    TopLevel,
    /// `vendor/<name>/BUCK`, the crate's own package.
    CrateDir(&'a String),
    /// `vendor/<name>-<version>/BUCK`, the versioned source directory (only
    /// exists in vendored-source mode).
    VersionDir(&'a PackageVersion),
    /// The root package's own BUCK file.
    RootPackage,
}

fn split_destination<'a>(vendor: &VendorConfig, rule: &'a Rule) -> SplitDestination<'a> {
    match rule {
        Rule::Alias(_) | Rule::ExtractArchive(_) | Rule::GitFetch(_) => {
            SplitDestination::TopLevel
        }
        // Non-vendored crates build straight from the downloaded archive, so the
        // archive lives in the crate's own package where the ":{name}-{ver}.crate"
        // srcs references resolve. (HttpArchive never exists in vendored mode.)
        // A GitFetchAlias likewise gives the crate's package a local handle on
        // the shared top-level git_fetch. (Neither exists in vendored mode.)
        Rule::HttpArchive(HttpArchive { owner, .. })
        | Rule::GitFetchAlias(Alias { owner, .. }) => SplitDestination::CrateDir(&owner.name),
        Rule::Binary(RustBinary { owner, .. })
        | Rule::Library(RustLibrary { owner, .. })
        | Rule::BuildscriptBinary(RustBinary { owner, .. })
        | Rule::BuildscriptGenrule(BuildscriptGenrule { owner, .. }) => {
            SplitDestination::CrateDir(&owner.name)
        }
        // Not emitted in non-vendored mode; always a versioned source dir.
        Rule::Sources(Sources { owner, .. }) => SplitDestination::VersionDir(owner),
        Rule::Filegroup(Filegroup { owner, .. })
        | Rule::CxxLibrary(CxxLibrary { owner, .. })
        | Rule::PrebuiltCxxLibrary(PrebuiltCxxLibrary { owner, .. }) => {
            if matches!(vendor, VendorConfig::Source(_)) {
                SplitDestination::VersionDir(owner)
            } else {
                SplitDestination::CrateDir(&owner.name)
            }
        }
        Rule::RootPackage(_) => SplitDestination::RootPackage,
    }
}

pub(crate) fn buckify(
    config: &Config,
    args: &Args,
    paths: &Paths,
    stdout: bool,
    fast: bool,
) -> anyhow::Result<()> {
    buckify_diagnostic(format_args!("buckify start fast={fast} stdout={stdout}"));

    // A vendored local-registry lays out sources as `.crate` files that only
    // the monolithic BUCK file can reference, so per-crate split output cannot
    // point at them yet. (`vendor = false` downgrades an unusable local
    // registry to Off before this point, so this only fires for a real one.)
    if config.buck.split && matches!(config.vendor, VendorConfig::LocalRegistry) {
        bail!("`[buck] split = true` does not support `vendor = \"local-registry\"` yet");
    }

    let (lockfile, metadata) = {
        log::info!("Running `cargo metadata`...");
        measure_time::info_time!("Running `cargo metadata`");
        cargo_get_lockfile_and_metadata(config, args, paths, fast)?
    };
    buckify_diagnostic(format_args!(
        "metadata loaded packages={} resolve_nodes={} workspace_members={}",
        metadata.packages.len(),
        metadata.resolve.nodes.len(),
        metadata.workspace_members.len()
    ));

    log::trace!("Metadata {:#?}", metadata);

    let fixups = FixupsCache::new(config, paths);
    let index = Index::new(config, &metadata, &fixups)?;
    let packages = metadata.packages.iter().collect::<Vec<_>>();
    let collision_info = if config.buck.split {
        CollisionInfo::new(&packages)
    } else {
        CollisionInfo::new_with_reserved(
            &packages,
            index.public_packages.values().flatten().copied(),
        )
    };
    let context = RuleContext {
        config,
        paths,
        index,
        lockfile: &lockfile,
        fixups,
        collision_info,
        done: Mutex::new(HashSet::default()),
    };
    let rules = do_buckify(&context)?;
    buckify_diagnostic(format_args!("rules generated count={}", rules.len()));

    // Report unused fixups
    let fixups = &context.fixups.lock();
    let mut public_packages = HashMap::default();
    for pkgid in context.index.public_packages.keys() {
        public_packages
            .entry(pkgid.name().as_str())
            .or_insert_with(HashSet::default)
            .insert(pkgid.version());
    }
    let mut unused = UnusedFixups::new();
    let no_public_versions = HashSet::default();
    for (name, fixup) in &**fixups {
        let public_versions = public_packages.get(name).unwrap_or(&no_public_versions);
        fixup.collect_unused(name, public_versions, &mut unused);
    }
    unused.check()?;

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
    let buckpath = &paths.third_party_dir.join(&config.buck.file_name);
    {
        measure_time::trace_time!("Write build rules to file");

        if config.buck.split {
            let mut toplevel_rules = Vec::new();
            let mut crate_rules = BTreeMap::new();
            let mut version_rules = BTreeMap::new();
            let mut root_package_rule = None;
            for rule in &rules {
                match split_destination(&config.vendor, rule) {
                    SplitDestination::TopLevel => {
                        toplevel_rules.push(rule);
                    }
                    SplitDestination::CrateDir(name) => {
                        crate_rules.entry(name).or_insert_with(Vec::new).push(rule);
                    }
                    SplitDestination::VersionDir(owner) => {
                        version_rules
                            .entry(owner)
                            .or_insert_with(Vec::new)
                            .push(rule);
                    }
                    SplitDestination::RootPackage => {
                        root_package_rule = Some(rule);
                    }
                }
            }

            let mut generated_vendor_buck_paths = BTreeSet::new();

            fn spawn<'scope, 'env>(
                scope: &'scope thread::Scope<'scope, 'env>,
                errors: &'scope Mutex<Vec<anyhow::Error>>,
                f: impl FnOnce() -> anyhow::Result<()> + Send + 'scope,
            ) {
                scope.spawn(move || {
                    if let Err(err) = f() {
                        errors
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(err);
                    }
                });
            }

            let errors_owned = Mutex::new(Vec::new());
            let errors = &errors_owned;
            thread::scope(|scope| {
                spawn(scope, errors, move || {
                    let mut out = Vec::new();
                    buck::write_buckfile(&config.buck, toplevel_rules.into_iter(), &mut out)
                        .context("writing buck file")?;
                    if !fs::read(buckpath).is_ok_and(|x| x == out) {
                        fs::write(buckpath, out)
                            .with_context(|| format!("write {} file", buckpath.display()))?;
                    }
                    Ok(())
                });

                for (owner, rules) in crate_rules {
                    let owner_dir = paths.third_party_dir.join("vendor").join(owner);
                    let buckpath = owner_dir.join(&config.buck.file_name);
                    generated_vendor_buck_paths.insert(buckpath.clone());
                    spawn(scope, errors, move || {
                        let mut out = Vec::new();
                        buck::write_buckfile(&config.buck, rules.into_iter(), &mut out)
                            .context("writing buck file")?;
                        fs::create_dir_all(&owner_dir)
                            .with_context(|| format!("crate {}", owner_dir.display()))?;
                        if !fs::read(&buckpath).is_ok_and(|x| x == out) {
                            fs::write(&buckpath, out)
                                .with_context(|| format!("write {} file", buckpath.display()))?;
                        }
                        Ok(())
                    });
                }

                for (owner, rules) in version_rules {
                    let buckpath = paths
                        .third_party_dir
                        .join("vendor")
                        .join(format!("{}-{}", owner.name, owner.version))
                        .join(&config.buck.file_name);
                    generated_vendor_buck_paths.insert(buckpath.clone());
                    spawn(scope, errors, move || {
                        let mut out = Vec::new();
                        buck::write_buckfile(&config.buck, rules.into_iter(), &mut out)
                            .context("writing buck file")?;
                        if !fs::read(&buckpath).is_ok_and(|x| x == out) {
                            fs::write(&buckpath, out)
                                .with_context(|| format!("write {} file", buckpath.display()))?;
                        }
                        Ok(())
                    });
                }

                if let Some(root_package_rule) = root_package_rule {
                    spawn(scope, errors, move || {
                        let mut out = Vec::new();
                        buck::write_buckfile(&config.buck, iter::once(root_package_rule), &mut out)
                            .context("writing buck file")?;
                        let buckpath = context.index.root_pkg.unwrap().targets[0]
                            .src_path
                            .with_file_name(&*config.buck.file_name);
                        if !fs::read(&buckpath).is_ok_and(|x| x == out) {
                            fs::write(&buckpath, out)
                                .with_context(|| format!("write {} file", buckpath.display()))?;
                        }
                        Ok(())
                    });
                }

                for fixup in fixups.values() {
                    let mut overlay_dirs = BTreeSet::new();
                    let mut include_dirs = BTreeSet::new();
                    for fixup in iter::once(&fixup.base).chain(&fixup.platform_fixup) {
                        overlay_dirs.extend(&fixup.overlay);
                        for cxx_library in &fixup.cxx_library {
                            include_dirs.extend(&cxx_library.fixup_include_paths);
                        }
                    }

                    // FIXME: what if the same directory is both an overlay
                    // directory and fixup_include_paths directory.
                    for dir in overlay_dirs {
                        spawn(scope, errors, move || {
                            let content = buckify_overlay_export_files(config, paths);
                            let buckpath = fixup.fixup_dir.join(dir).join(&config.buck.file_name);
                            if !fs::read(&buckpath).is_ok_and(|x| x == content.as_bytes()) {
                                fs::write(&buckpath, content).with_context(|| {
                                    format!("write {} file", buckpath.display())
                                })?;
                            }
                            Ok(())
                        });
                    }

                    for dir in include_dirs {
                        spawn(scope, errors, move || {
                            let content = buckify_cxx_library_fixup_include_paths(config, paths);
                            let buckpath = fixup.fixup_dir.join(dir).join(&config.buck.file_name);
                            if !fs::read(&buckpath).is_ok_and(|x| x == content.as_bytes()) {
                                fs::write(&buckpath, content).with_context(|| {
                                    format!("write {} file", buckpath.display())
                                })?;
                            }
                            Ok(())
                        });
                    }
                }
            });

            // Report just the first error, if there is one. Starlark
            // serialization is infallible so errors here are just from
            // filesystem I/O, and if there is more than one, it is very likely
            // there are thousands and they are all identical: for example an
            // unhealthy edenfs mount.
            if let Some(err) = errors_owned
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .into_iter()
                .next()
            {
                return Err(err);
            }

            cleanup_stale_split_vendor_buck_files(config, paths, &generated_vendor_buck_paths)?;
        } else {
            let mut out = Vec::new();
            buck::write_buckfile(&config.buck, rules.iter(), &mut out)
                .context("writing buck file")?;
            if !fs::read(buckpath).is_ok_and(|x| x == out) {
                fs::write(buckpath, out)
                    .with_context(|| format!("write {} file", buckpath.display()))?;
            }
        }
    }

    log::trace!(
        "{} file written to {}",
        config.buck.file_name,
        buckpath.display()
    );

    Ok(())
}

fn cleanup_stale_split_vendor_buck_files(
    config: &Config,
    paths: &Paths,
    generated_vendor_buck_paths: &BTreeSet<PathBuf>,
) -> anyhow::Result<()> {
    let vendor_dir = paths.third_party_dir.join("vendor");
    let entries = match fs::read_dir(&vendor_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", vendor_dir.display()));
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let crate_dir = entry.path();
        let buckpath = crate_dir.join(&config.buck.file_name);
        if generated_vendor_buck_paths.contains(&buckpath)
            || !is_generated_buck_file(config, &buckpath)?
        {
            continue;
        }

        fs::remove_file(&buckpath)
            .with_context(|| format!("failed to remove stale {}", buckpath.display()))?;
        match fs::remove_dir(&crate_dir) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to remove empty {}", crate_dir.display()));
            }
        }
    }

    Ok(())
}

fn is_generated_buck_file(config: &Config, buckpath: &Path) -> anyhow::Result<bool> {
    let header = config.buck.generated_file_header.as_bytes();
    if header.is_empty() {
        return Ok(false);
    }

    let file_type = match fs::symlink_metadata(buckpath) {
        Ok(metadata) => metadata.file_type(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to stat {}", buckpath.display()));
        }
    };
    if !file_type.is_file() {
        return Ok(false);
    }

    fs::read(buckpath)
        .map(|contents| contents.starts_with(header))
        .with_context(|| format!("failed to read {}", buckpath.display()))
}

#[cfg(test)]
mod test {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;

    use std::collections::BTreeMap;

    use semver::Version;

    use super::SplitDestination;
    use super::cleanup_stale_split_vendor_buck_files;
    use super::relabel_platform_fixups_mapped_srcs;
    use super::short_name_for_git_repo;
    use super::split_destination;
    use super::vendor_crate_visibility;
    use crate::Paths;
    use crate::buck::Alias;
    use crate::buck::BuckPath;
    use crate::buck::Filegroup;
    use crate::buck::FilegroupSources;
    use crate::buck::GitFetch;
    use crate::buck::HttpArchive;
    use crate::buck::Name;
    use crate::buck::PackageVersion;
    use crate::buck::PlatformRustCommon;
    use crate::buck::PlatformSources;
    use crate::buck::Rule;
    use crate::buck::RuleRef;
    use crate::buck::Sources;
    use crate::buck::SubtargetOrPath;
    use crate::buck::Visibility;
    use crate::collection::Select;
    use crate::config::Config;
    use crate::config::VendorConfig;

    fn pkg_version(name: &str) -> PackageVersion {
        PackageVersion {
            name: name.to_owned(),
            version: Version::new(1, 0, 0),
        }
    }

    fn paths_with_buck_package(buck_package: &str) -> Paths {
        Paths {
            buck_package: buck_package.to_owned(),
            third_party_dir: PathBuf::new(),
            manifest_path: PathBuf::new(),
            lockfile_path: PathBuf::new(),
            cargo_home: PathBuf::new(),
        }
    }

    fn paths_with_third_party_dir(third_party_dir: PathBuf) -> Paths {
        Paths {
            buck_package: "third-party/rust".to_owned(),
            third_party_dir,
            manifest_path: PathBuf::new(),
            lockfile_path: PathBuf::new(),
            cargo_home: PathBuf::new(),
        }
    }

    fn split_config() -> Config {
        toml::from_str("[buck]\nsplit = true\n").expect("test config should parse")
    }

    #[test]
    fn cleanup_stale_split_vendor_buck_files_removes_only_unproduced_generated_buck_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_with_third_party_dir(dir.path().join("third-party/rust"));
        let vendor_dir = paths.third_party_dir.join("vendor");
        let config = split_config();
        let generated_file_header = &*config.buck.generated_file_header;

        let active_buck = vendor_dir.join("active-crate").join("BUCK");
        fs::create_dir_all(active_buck.parent().expect("active parent")).unwrap();
        fs::write(
            &active_buck,
            format!("{generated_file_header}active rules\n"),
        )
        .unwrap();

        let stale_split_buck = vendor_dir.join("stale-crate").join("BUCK");
        fs::create_dir_all(stale_split_buck.parent().expect("stale parent")).unwrap();
        fs::write(
            &stale_split_buck,
            format!("{generated_file_header}stale rules\n"),
        )
        .unwrap();

        let stale_versioned_dir = vendor_dir.join("stale-crate-1.0.0");
        let stale_versioned_buck = stale_versioned_dir.join("BUCK");
        fs::create_dir_all(&stale_versioned_dir).unwrap();
        fs::write(stale_versioned_dir.join("lib.rs"), "pub fn stale() {}\n").unwrap();
        fs::write(
            &stale_versioned_buck,
            format!("{generated_file_header}stale versioned rules\n"),
        )
        .unwrap();

        let handwritten_buck = vendor_dir.join("handwritten-crate").join("BUCK");
        fs::create_dir_all(handwritten_buck.parent().expect("handwritten parent")).unwrap();
        fs::write(&handwritten_buck, "handwritten rules\n").unwrap();

        cleanup_stale_split_vendor_buck_files(
            &config,
            &paths,
            &BTreeSet::from([active_buck.clone()]),
        )
        .unwrap();

        assert!(
            active_buck.exists(),
            "BUCK files generated by the current buckify run should be preserved"
        );
        assert!(
            !stale_split_buck.exists(),
            "stale generated split BUCK files should be removed"
        );
        assert!(
            !stale_split_buck.parent().expect("stale parent").exists(),
            "empty stale split BUCK package dirs should be removed"
        );
        assert!(
            !stale_versioned_buck.exists(),
            "stale generated BUCK files in versioned vendor dirs should be removed"
        );
        assert!(
            stale_versioned_dir.join("lib.rs").exists(),
            "source files in versioned vendor dirs should be preserved"
        );
        assert!(
            handwritten_buck.exists(),
            "BUCK files without the reindeer generated header should not be deleted"
        );
    }

    #[test]
    fn vendor_crate_visibility_scopes_to_crate_dir() {
        let paths = paths_with_buck_package("third-party/rust");
        assert_eq!(
            vendor_crate_visibility(&paths, "unicorn-factory"),
            Visibility::Custom(vec![
                "//third-party/rust/vendor/unicorn-factory:".to_owned()
            ]),
            "non-empty buck_package should prefix the crate's vendor dir",
        );
    }

    #[test]
    fn vendor_crate_visibility_handles_empty_buck_package() {
        let paths = paths_with_buck_package("");
        assert_eq!(
            vendor_crate_visibility(&paths, "flux-capacitor"),
            Visibility::Custom(vec!["//vendor/flux-capacitor:".to_owned()]),
            "empty buck_package should emit a root-relative vendor path",
        );
    }

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

    #[test]
    fn split_destination_routes_rules_by_kind_and_vendor_mode() {
        let owner = pkg_version("serde");

        let http_archive = Rule::HttpArchive(HttpArchive {
            owner: owner.clone(),
            name: Name("serde-1.0.0.crate".to_owned()),
            sha256: "0".to_owned(),
            strip_prefix: "serde-1.0.0".to_owned(),
            sub_targets: BTreeSet::new(),
            urls: vec![],
            visibility: Visibility::Private,
            sort_key: Name("serde-1.0.0".to_owned()),
        });

        let git_fetch = Rule::GitFetch(GitFetch {
            name: Name("repo-0.git".to_owned()),
            repo: "https://example.com/repo".to_owned(),
            rev: "abc".to_owned(),
            sub_targets: BTreeSet::new(),
            visibility: Visibility::Private,
        });

        let git_fetch_alias = Rule::GitFetchAlias(Alias {
            owner: owner.clone(),
            name: Name("repo-0.git".to_owned()),
            actual: RuleRef::new("//third-party/rust:repo-0.git".to_owned()),
            platforms: None,
            target_compatible_with: Select::default(),
            visibility: Visibility::Private,
            sort_key: Name("repo-0.git".to_owned()),
        });

        let sources = Rule::Sources(Sources {
            owner: owner.clone(),
            name: Name("srcs".to_owned()),
            base: PlatformSources::default(),
            platform: BTreeMap::new(),
            visibility: Visibility::Private,
        });

        let filegroup = Rule::Filegroup(Filegroup {
            owner: owner.clone(),
            name: Name("filegroup-manifest".to_owned()),
            srcs: FilegroupSources::Set(BTreeSet::new()),
            visibility: Visibility::Private,
        });

        // HttpArchive and GitFetchAlias always route to the crate dir.
        assert!(matches!(
            split_destination(&VendorConfig::Off, &http_archive),
            SplitDestination::CrateDir(name) if name == "serde",
        ));
        assert!(matches!(
            split_destination(&VendorConfig::Off, &git_fetch_alias),
            SplitDestination::CrateDir(name) if name == "serde",
        ));

        // GitFetch is shared across crates and stays at top level.
        assert!(matches!(
            split_destination(&VendorConfig::Off, &git_fetch),
            SplitDestination::TopLevel,
        ));

        // Sources only exist in vendored mode and always go to the versioned dir.
        assert!(matches!(
            split_destination(&VendorConfig::Source(Default::default()), &sources),
            SplitDestination::VersionDir(o) if o == &owner,
        ));

        // Filegroup (and, sharing its match arm, CxxLibrary / PrebuiltCxxLibrary)
        // goes to the versioned dir under vendored sources but the crate dir
        // otherwise.
        assert!(matches!(
            split_destination(&VendorConfig::Source(Default::default()), &filegroup),
            SplitDestination::VersionDir(o) if o == &owner,
        ));
        assert!(matches!(
            split_destination(&VendorConfig::Off, &filegroup),
            SplitDestination::CrateDir(name) if name == "serde",
        ));
    }

    #[test]
    fn relabel_fixups_mapped_srcs_rewrites_only_fixup_tree_keys() {
        let paths = paths_with_buck_package("third-party/rust");
        let mut platform = PlatformRustCommon::default();

        let fixup_key =
            SubtargetOrPath::Path(BuckPath(PathBuf::from("fixups/foo/overlay/lib.rs")));
        let fixup_value = BuckPath(PathBuf::from("src/lib.rs"));
        platform
            .mapped_srcs
            .insert(fixup_key, fixup_value.clone());

        let archive_key = SubtargetOrPath::Subtarget(crate::subtarget::Subtarget {
            target: Name("foo-1.0.0.crate".to_owned()),
            relative: BuckPath(PathBuf::from("src/generated.rs")),
        });
        let archive_value = BuckPath(PathBuf::from("src/generated.rs"));
        platform
            .mapped_srcs
            .insert(archive_key.clone(), archive_value.clone());

        platform.srcs.insert(BuckPath(PathBuf::from("keep.rs")));

        relabel_platform_fixups_mapped_srcs(&paths, &mut platform);

        // The fixups-tree key becomes a package-absolute label; its value is
        // untouched.
        let rewritten = SubtargetOrPath::Path(BuckPath(PathBuf::from(
            "//third-party/rust/fixups/foo/overlay:lib.rs",
        )));
        assert_eq!(platform.mapped_srcs.get(&rewritten), Some(&fixup_value));

        // A subtarget into the archive is left exactly as-is.
        assert_eq!(platform.mapped_srcs.get(&archive_key), Some(&archive_value));

        // srcs are untouched.
        assert!(platform.srcs.contains(&BuckPath(PathBuf::from("keep.rs"))));

        // No stray key was created.
        assert_eq!(platform.mapped_srcs.len(), 2);
    }

    #[test]
    fn split_fixups_mapped_src_key_handles_empty_buck_package() {
        let paths = paths_with_buck_package("");
        let key = super::split_fixups_mapped_src_key(
            &paths,
            &BuckPath(PathBuf::from("fixups/foo/overlay/src/lib.rs")),
        );
        assert_eq!(
            key,
            BuckPath(PathBuf::from("//fixups/foo/overlay:src/lib.rs")),
        );
    }
}
