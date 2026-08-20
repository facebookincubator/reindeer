/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::btree_map;
use std::collections::hash_map;
use std::iter;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::task::Poll;
use std::task::ready;

use anyhow::Context;
use anyhow::bail;
use cargo::core::Dependency;
use cargo::core::Package as CargoPackage;
use cargo::core::PackageId;
use cargo::core::SourceId;
use cargo::core::Summary;
use cargo::sources::IndexSummary;
use cargo::sources::SourceConfigMap;
use cargo::sources::source::MaybePackage;
use cargo::sources::source::QueryKind;
use cargo::sources::source::Source as CargoSource;
use cargo::util::CanonicalUrl;
use cargo::util::OptVersionReq;
use cargo::util::cache_lock::CacheLockMode;
use foldhash::HashMap;
use semver::Version;
use semver::VersionReq;

use crate::Paths;
use crate::fixups::ResolverDependencyFixup;
use crate::fixups::resolver_fixups_for_package;
use crate::semver_ext::CompatibilityLane;
use crate::semver_ext::version_bounds_subset;
use crate::semver_ext::version_compatibility_lane;
use crate::semver_ext::version_req_bounds;
use crate::semver_ext::version_req_is_broad;
use crate::semver_ext::version_req_to_compatibility_lane;

#[derive(Clone)]
struct DeterministicSourceContext<'gctx> {
    source_config: SourceConfigMap<'gctx>,
    previous_edges: BTreeMap<DependencyEdgeKey, PackageId>,
    fixups_dir: PathBuf,
    known_sources: Rc<BTreeSet<SourceId>>,
    discovered_sources: Rc<RefCell<BTreeSet<SourceId>>>,
    root_patch_summaries: BTreeMap<CanonicalUrl, Vec<Summary>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DependencyEdgeKey {
    parent: PackageKey,
    dependency_name_in_toml: String,
    dependency_source: SourceKey,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PackageKey {
    name: String,
    compatibility_lane: CompatibilityLane,
    source: SourceKey,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SourceKey {
    CratesIo,
    Source(SourceId),
}

struct DeterministicSource<'gctx, S> {
    delegate: S,
    context: DeterministicSourceContext<'gctx>,
    candidate_sources: HashMap<SourceId, Box<dyn CargoSource + 'gctx>>,
    fixup_cache: BTreeMap<PackageId, BTreeMap<String, ResolverDependencyFixup>>,
}

impl<'gctx, S> DeterministicSource<'gctx, S> {
    fn new(delegate: S, context: DeterministicSourceContext<'gctx>) -> Self {
        Self {
            delegate,
            context,
            candidate_sources: HashMap::default(),
            fixup_cache: BTreeMap::new(),
        }
    }
}

impl<'gctx, S: CargoSource> DeterministicSource<'gctx, S> {
    fn rewrite_index_summary(
        &mut self,
        summary: IndexSummary,
    ) -> Poll<anyhow::Result<IndexSummary>> {
        let rewritten_summary = ready!(self.rewrite_summary(summary.as_summary().clone()))?;
        Poll::Ready(Ok(summary.map_summary(|_| rewritten_summary.clone())))
    }

    fn rewrite_summary(
        &mut self,
        summary: cargo::core::Summary,
    ) -> Poll<anyhow::Result<cargo::core::Summary>> {
        let parent = summary.package_id();
        let mut dependencies = Vec::with_capacity(summary.dependencies().len());
        for dependency in summary.dependencies().iter().cloned() {
            let dependency = ready!(self.rewrite_dependency(parent, dependency))?;
            dependencies.push(dependency);
        }
        let mut dependencies = dependencies.into_iter();
        Poll::Ready(Ok(summary.try_map_dependencies(|_| {
            Ok(dependencies
                .next()
                .expect("rewritten dependency should exist"))
        })?))
    }

    fn rewrite_dependency(
        &mut self,
        parent: PackageId,
        mut dependency: Dependency,
    ) -> Poll<anyhow::Result<Dependency>> {
        self.record_dependency_source(dependency.source_id());
        if !dependency.source_id().is_registry() {
            return Poll::Ready(Ok(dependency));
        }

        let original_req = match parse_dependency_req(&dependency) {
            Ok(req) => req,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let fixup = match self.resolver_fixup(parent, &dependency) {
            Ok(fixup) => fixup,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let effective_req = if let Some(fixup) = &fixup {
            if let Err(err) = validate_version_req_subset(&fixup.narrow_to, &original_req) {
                return Poll::Ready(Err(err).with_context(|| {
                    format!(
                        "resolver fixup for {} dependency {} narrows {} to {}",
                        parent,
                        dependency.name_in_toml(),
                        original_req,
                        fixup.narrow_to,
                    )
                }));
            }
            fixup.narrow_to.clone()
        } else {
            original_req.clone()
        };

        if fixup.is_none() {
            match version_req_is_broad(&effective_req) {
                Ok(true) => {}
                Ok(false) => return Poll::Ready(Ok(dependency)),
                Err(err) => return Poll::Ready(Err(err)),
            }
        }

        let previous = match self.previous_locked_edge(parent, &dependency, &effective_req) {
            Ok(previous) => previous,
            Err(err) => return Poll::Ready(Err(err)),
        };
        let narrowed_req = if let Some(previous) = previous {
            match version_req_to_compatibility_lane(&effective_req, previous.version()) {
                Ok(req) => req,
                Err(err) => return Poll::Ready(Err(err)),
            }
        } else if fixup.is_some()
            && match version_req_is_broad(&effective_req) {
                Ok(is_broad) => !is_broad,
                Err(err) => return Poll::Ready(Err(err)),
            }
        {
            effective_req.clone()
        } else {
            let candidates = match self.candidate_versions(&dependency) {
                Poll::Ready(Ok(candidates)) => candidates,
                Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                Poll::Pending => return Poll::Pending,
            };
            let Some(narrowed_req) =
                self.fresh_candidate_requirement(&dependency, &effective_req, candidates)
            else {
                return Poll::Ready(Err(anyhow::anyhow!(
                    "deterministic vendor could not find a stable or explicitly requested prerelease non-yanked candidate for broad indirect requirement {} {} from {}",
                    dependency.package_name(),
                    effective_req,
                    parent,
                )));
            };
            narrowed_req
        };

        if let Err(err) = validate_version_req_subset(&narrowed_req, &effective_req) {
            return Poll::Ready(Err(err));
        }
        dependency.set_version_req(OptVersionReq::Req(narrowed_req));
        Poll::Ready(Ok(dependency))
    }

    fn resolver_fixup(
        &mut self,
        parent: PackageId,
        dependency: &Dependency,
    ) -> anyhow::Result<Option<ResolverDependencyFixup>> {
        let fixups = match self.fixup_cache.entry(parent) {
            btree_map::Entry::Occupied(entry) => entry.into_mut(),
            btree_map::Entry::Vacant(entry) => {
                let fixups = resolver_fixups_for_package(
                    &self.context.fixups_dir,
                    parent.name().as_str(),
                    parent.version(),
                )?;
                entry.insert(fixups)
            }
        };

        Ok(fixups.get(dependency.name_in_toml().as_str()).cloned())
    }

    fn previous_locked_edge(
        &self,
        parent: PackageId,
        dependency: &Dependency,
        effective_req: &VersionReq,
    ) -> anyhow::Result<Option<PackageId>> {
        let key = dependency_edge_key(parent, dependency)?;
        let previous = self.context.previous_edges.get(&key);
        let Some(previous) = previous else {
            return Ok(None);
        };
        if previous.name() == dependency.package_name() && effective_req.matches(previous.version())
        {
            Ok(Some(*previous))
        } else {
            Ok(None)
        }
    }

    fn fresh_candidate_requirement(
        &self,
        dependency: &Dependency,
        effective_req: &VersionReq,
        mut candidates: Vec<Version>,
    ) -> Option<VersionReq> {
        candidates.retain(|version| effective_req.matches(version));
        candidates.sort();
        while let Some(candidate) = candidates.pop() {
            let Ok(narrowed_req) = version_req_to_compatibility_lane(effective_req, &candidate)
            else {
                continue;
            };
            if validate_version_req_subset(&narrowed_req, effective_req).is_ok()
                && narrowed_req.matches(&candidate)
                && dependency.version_req().matches(&candidate)
            {
                return Some(narrowed_req);
            }
        }
        None
    }

    fn candidate_versions(
        &mut self,
        dependency: &Dependency,
    ) -> Poll<anyhow::Result<Vec<Version>>> {
        let mut versions = self.root_patch_candidate_versions(dependency);
        if !versions.is_empty() {
            return Poll::Ready(Ok(versions));
        }

        let source_id = dependency.source_id();
        if source_id == self.delegate.source_id() {
            versions.extend(ready!(query_candidate_versions(
                &mut self.delegate,
                dependency
            ))?);
            return Poll::Ready(Ok(versions));
        }
        let source = match self.candidate_sources.entry(source_id) {
            hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hash_map::Entry::Vacant(entry) => {
                let source = match self
                    .context
                    .source_config
                    .load(source_id, &std::collections::HashSet::new())
                {
                    Ok(source) => source,
                    Err(err) => return Poll::Ready(Err(err)),
                };
                entry.insert(source)
            }
        };
        versions.extend(ready!(query_candidate_versions(
            source.as_mut(),
            dependency
        ))?);
        Poll::Ready(Ok(versions))
    }

    fn record_dependency_source(&self, source_id: SourceId) {
        if !source_id.is_path() && !self.context.known_sources.contains(&source_id) {
            self.context
                .discovered_sources
                .borrow_mut()
                .insert(source_id);
        }
    }

    fn root_patch_candidate_versions(&self, dependency: &Dependency) -> Vec<Version> {
        self.context
            .root_patch_summaries
            .get(dependency.source_id().canonical_url())
            .into_iter()
            .flatten()
            .filter(|summary| dependency.matches_ignoring_source(summary.package_id()))
            .map(|summary| summary.package_id().version().clone())
            .collect()
    }
}

fn query_candidate_versions(
    source: &mut dyn CargoSource,
    dependency: &Dependency,
) -> Poll<anyhow::Result<Vec<Version>>> {
    let mut candidate_dependency = dependency.clone();
    candidate_dependency.set_version_req(OptVersionReq::Any);
    let candidates = match source.query_vec(&candidate_dependency, QueryKind::Exact)? {
        Poll::Ready(candidates) => candidates,
        Poll::Pending => return Poll::Pending,
    };
    Poll::Ready(Ok(candidate_versions_from_summaries(candidates)))
}

fn dependency_edge_key(
    parent: PackageId,
    dependency: &Dependency,
) -> anyhow::Result<DependencyEdgeKey> {
    Ok(DependencyEdgeKey {
        parent: package_key(parent)?,
        dependency_name_in_toml: dependency.name_in_toml().as_str().to_owned(),
        dependency_source: source_key(dependency.source_id()),
    })
}

fn selected_package_edge_key(
    parent: PackageId,
    selected_dependency: PackageId,
) -> anyhow::Result<DependencyEdgeKey> {
    Ok(DependencyEdgeKey {
        parent: package_key(parent)?,
        dependency_name_in_toml: selected_dependency.name().as_str().to_owned(),
        dependency_source: source_key(selected_dependency.source_id()),
    })
}

fn package_key(package: PackageId) -> anyhow::Result<PackageKey> {
    Ok(PackageKey {
        name: package.name().as_str().to_owned(),
        compatibility_lane: version_compatibility_lane(package.version())?,
        source: source_key(package.source_id()),
    })
}

fn source_key(source_id: SourceId) -> SourceKey {
    if source_id.is_crates_io() {
        SourceKey::CratesIo
    } else {
        SourceKey::Source(source_id)
    }
}

fn candidate_versions_from_summaries(candidates: Vec<IndexSummary>) -> Vec<Version> {
    candidates
        .into_iter()
        .filter(|candidate| matches!(candidate, IndexSummary::Candidate(_)))
        .map(|candidate| candidate.package_id().version().clone())
        .collect()
}

impl<'gctx, S: CargoSource> CargoSource for DeterministicSource<'gctx, S> {
    fn source_id(&self) -> SourceId {
        self.delegate.source_id()
    }

    fn replaced_source_id(&self) -> SourceId {
        self.delegate.replaced_source_id()
    }

    fn supports_checksums(&self) -> bool {
        self.delegate.supports_checksums()
    }

    fn requires_precise(&self) -> bool {
        self.delegate.requires_precise()
    }

    fn query(
        &mut self,
        dep: &Dependency,
        kind: QueryKind,
        f: &mut dyn FnMut(IndexSummary),
    ) -> Poll<anyhow::Result<()>> {
        let mut summaries = Vec::new();
        match self.delegate.query(dep, kind, &mut |summary| {
            summaries.push(summary);
        }) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        }

        let mut rewritten = Vec::with_capacity(summaries.len());
        for summary in summaries {
            let summary = ready!(self.rewrite_index_summary(summary))?;
            rewritten.push(summary);
        }
        for summary in rewritten {
            f(summary);
        }
        Poll::Ready(Ok(()))
    }

    fn invalidate_cache(&mut self) {
        self.delegate.invalidate_cache();
        for source in self.candidate_sources.values_mut() {
            source.invalidate_cache();
        }
    }

    fn set_quiet(&mut self, quiet: bool) {
        self.delegate.set_quiet(quiet);
        for source in self.candidate_sources.values_mut() {
            source.set_quiet(quiet);
        }
    }

    fn download(&mut self, pkg_id: PackageId) -> anyhow::Result<MaybePackage> {
        self.delegate.download(pkg_id)
    }

    fn finish_download(
        &mut self,
        pkg_id: PackageId,
        contents: Vec<u8>,
    ) -> anyhow::Result<CargoPackage> {
        self.delegate.finish_download(pkg_id, contents)
    }

    fn fingerprint(&self, pkg: &CargoPackage) -> anyhow::Result<String> {
        self.delegate.fingerprint(pkg)
    }

    fn describe(&self) -> String {
        self.delegate.describe()
    }

    fn add_to_yanked_whitelist(&mut self, pkgs: &[PackageId]) {
        self.delegate.add_to_yanked_whitelist(pkgs);
        for source in self.candidate_sources.values_mut() {
            source.add_to_yanked_whitelist(pkgs);
        }
    }

    fn is_yanked(&mut self, pkg: PackageId) -> Poll<anyhow::Result<bool>> {
        self.delegate.is_yanked(pkg)
    }

    fn block_until_ready(&mut self) -> anyhow::Result<()> {
        self.delegate.block_until_ready()?;
        for source in self.candidate_sources.values_mut() {
            source.block_until_ready()?;
        }
        Ok(())
    }

    fn verify(&self, pkg: PackageId) -> anyhow::Result<()> {
        self.delegate.verify(pkg)
    }
}

pub(crate) fn resolve_ws_deterministically_with_original_sources<'gctx>(
    workspace: &cargo::core::Workspace<'gctx>,
    gctx: &'gctx cargo::GlobalContext,
    paths: &Paths,
    fixups_dir: &Path,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    let _cache_lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
    let source_config = cargo::sources::SourceConfigMap::empty(gctx)?;
    let previous_resolve = cargo::ops::load_pkg_lockfile(workspace)?;
    let previous_edges = previous_resolve
        .as_ref()
        .map(previous_dependency_edges)
        .transpose()?
        .unwrap_or_default();
    let root_patch_summaries = root_patch_summaries(workspace, &source_config)?;
    let mut source_ids = deterministic_source_ids(workspace, previous_resolve.as_ref(), gctx)?;

    log::info!("Running deterministic Cargo resolve");
    loop {
        let discovered_sources = Rc::new(RefCell::new(BTreeSet::new()));
        let context = DeterministicSourceContext {
            source_config: source_config.clone(),
            previous_edges: previous_edges.clone(),
            fixups_dir: fixups_dir.to_path_buf(),
            known_sources: Rc::new(source_ids.clone()),
            discovered_sources: Rc::clone(&discovered_sources),
            root_patch_summaries: root_patch_summaries.clone(),
        };
        let mut registry = cargo::core::registry::PackageRegistry::new_with_source_config(
            gctx,
            source_config.clone(),
        )?;
        for source_id in &source_ids {
            let source = source_config.load(*source_id, &std::collections::HashSet::new())?;
            registry.add_preloaded(Box::new(DeterministicSource::new(source, context.clone())));
        }

        let resolve_result = resolve_with_previous_allowing_locked_yanked(
            &mut registry,
            workspace,
            previous_resolve.as_ref(),
            None,
        );

        let new_sources = {
            let discovered_sources = discovered_sources.borrow();
            discovered_sources
                .difference(&source_ids)
                .copied()
                .collect::<Vec<_>>()
        };
        if !new_sources.is_empty() {
            // Future-proof alternate registries and non-path source dependencies introduced
            // by summaries not named by the workspace or the previous lockfile.
            log::info!(
                "Repeating deterministic Cargo resolve after discovering {} additional source(s)",
                new_sources.len(),
            );
            source_ids.extend(new_sources);
            continue;
        }

        let mut resolve = resolve_result?;

        cargo::ops::write_pkg_lockfile(workspace, &mut resolve)?;
        log::info!(
            "Wrote deterministic Cargo lockfile to {}",
            paths.lockfile_path.display()
        );
        return Ok(resolve);
    }
}

fn resolve_with_previous_allowing_locked_yanked<'gctx>(
    registry: &mut cargo::core::registry::PackageRegistry<'gctx>,
    workspace: &cargo::core::Workspace<'gctx>,
    previous_resolve: Option<&cargo::core::resolver::Resolve>,
    keep_previous: Option<&dyn Fn(&PackageId) -> bool>,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    let mut whitelisted = BTreeSet::new();
    let mut register_patches = true;
    loop {
        match cargo::ops::resolve_with_previous(
            registry,
            workspace,
            &cargo::core::resolver::CliFeatures::new_all(true),
            cargo::core::resolver::HasDevUnits::Yes,
            previous_resolve,
            keep_previous,
            &[],
            register_patches,
        ) {
            Ok(resolve) => return Ok(resolve),
            Err(err) => {
                let Some(previous_resolve) = previous_resolve else {
                    return Err(err);
                };
                let message = format!("{err:#}\n{err:?}");
                let Some((name, version)) = parse_yanked_resolution_error(&message)? else {
                    return Err(err);
                };
                let Some(package) = previous_resolve.iter().find(|package| {
                    package.source_id().is_registry()
                        && package.name().as_str() == name
                        && package.version() == &version
                }) else {
                    return Err(err);
                };
                if !whitelisted.insert(package) {
                    return Err(err);
                }
                register_patches = false;
                log::info!(
                    "Allowing previously locked yanked package {} as resolver candidate",
                    package,
                );
                registry.add_to_yanked_whitelist(iter::once(package));
            }
        }
    }
}

fn deterministic_source_ids<'gctx>(
    workspace: &cargo::core::Workspace<'gctx>,
    previous_resolve: Option<&cargo::core::resolver::Resolve>,
    gctx: &'gctx cargo::GlobalContext,
) -> anyhow::Result<BTreeSet<SourceId>> {
    let mut sources = deterministic_initial_sources(workspace, previous_resolve, gctx)?;
    for patch_dependencies in workspace.root_patch()?.values() {
        for dependency in patch_dependencies {
            sources.insert(dependency.source_id());
        }
    }
    Ok(sources)
}

fn deterministic_initial_sources<'gctx>(
    workspace: &cargo::core::Workspace<'gctx>,
    previous_resolve: Option<&cargo::core::resolver::Resolve>,
    gctx: &'gctx cargo::GlobalContext,
) -> anyhow::Result<BTreeSet<SourceId>> {
    let mut sources = BTreeSet::new();
    sources.insert(SourceId::crates_io(gctx)?);
    for member in workspace.members() {
        for dependency in member.summary().dependencies() {
            insert_non_path_source(&mut sources, dependency.source_id());
        }
    }
    if let Some(previous_resolve) = previous_resolve {
        for package in previous_resolve.iter() {
            insert_registry_source(&mut sources, package.source_id());
            for (_, dependencies) in previous_resolve.deps(package) {
                for dependency in dependencies {
                    insert_registry_source(&mut sources, dependency.source_id());
                }
            }
        }
    }
    Ok(sources)
}

fn insert_non_path_source(sources: &mut BTreeSet<SourceId>, source_id: SourceId) {
    if !source_id.is_path() {
        sources.insert(source_id);
    }
}

fn insert_registry_source(sources: &mut BTreeSet<SourceId>, source_id: SourceId) {
    if source_id.is_registry() {
        sources.insert(source_id);
    }
}

fn root_patch_summaries<'gctx>(
    workspace: &cargo::core::Workspace<'gctx>,
    source_config: &SourceConfigMap<'gctx>,
) -> anyhow::Result<BTreeMap<CanonicalUrl, Vec<Summary>>> {
    let mut patch_sources = HashMap::default();
    let mut root_patch_summaries: BTreeMap<CanonicalUrl, Vec<Summary>> = BTreeMap::new();
    for (url, patch_dependencies) in workspace.root_patch()?.iter() {
        let canonical = CanonicalUrl::new(url)?;
        let summaries = root_patch_summaries.entry(canonical.clone()).or_default();
        for dependency in patch_dependencies {
            let summary = root_patch_summary(source_config, &mut patch_sources, dependency)
                .with_context(|| {
                    format!(
                        "failed to resolve root patch candidate {} {}",
                        dependency.package_name(),
                        dependency.version_req(),
                    )
                })?;
            if summary.package_id().source_id().canonical_url() == &canonical {
                bail!(
                    "patch for `{}` in `{}` points to the same source, but patches must point to different sources",
                    dependency.package_name(),
                    url,
                );
            }
            summaries.push(summary);
        }
    }
    Ok(root_patch_summaries)
}

fn root_patch_summary<'gctx>(
    source_config: &SourceConfigMap<'gctx>,
    patch_sources: &mut HashMap<SourceId, Box<dyn CargoSource + 'gctx>>,
    dependency: &Dependency,
) -> anyhow::Result<Summary> {
    let source_id = dependency.source_id();
    let source = match patch_sources.entry(source_id) {
        hash_map::Entry::Occupied(entry) => entry.into_mut(),
        hash_map::Entry::Vacant(entry) => {
            let source = source_config.load(source_id, &std::collections::HashSet::new())?;
            entry.insert(source)
        }
    };

    let mut summaries = loop {
        match source.query_vec(dependency, QueryKind::Exact)? {
            Poll::Ready(summaries) => {
                break summaries
                    .into_iter()
                    .map(|summary| summary.into_summary())
                    .collect::<Vec<_>>();
            }
            Poll::Pending => source.block_until_ready()?,
        }
    };

    match summaries.len() {
        1 => Ok(summaries.pop().expect("one patch summary should exist")),
        0 => bail!(
            "patch location `{}` does not contain package `{}` matching `{}`",
            dependency.source_id(),
            dependency.package_name(),
            dependency.version_req(),
        ),
        _ => {
            let mut versions = summaries
                .iter()
                .map(|summary| summary.package_id().version().to_string())
                .collect::<Vec<_>>();
            versions.sort();
            bail!(
                "patch for `{}` in `{}` resolved to more than one candidate: {}",
                dependency.package_name(),
                dependency.source_id(),
                versions.join(", "),
            )
        }
    }
}

fn parse_yanked_resolution_error(message: &str) -> anyhow::Result<Option<(String, Version)>> {
    let requirement = message.lines().find_map(|line| {
        let (_, rest) = line.split_once("requirement `")?;
        let (requirement, _) = rest.split_once('`')?;
        requirement.split([' ', '=']).next().map(str::to_owned)
    });
    let version = message.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("version ")?;
        let version = rest.strip_suffix(" is yanked")?;
        Some(version)
    });
    match (requirement, version) {
        (Some(name), Some(version)) => Ok(Some((name, Version::parse(version)?))),
        _ => Ok(None),
    }
}

fn parse_dependency_req(dependency: &Dependency) -> anyhow::Result<VersionReq> {
    VersionReq::parse(&dependency.version_req().to_string()).with_context(|| {
        format!(
            "Failed to parse dependency requirement {} for {}",
            dependency.version_req(),
            dependency.package_name(),
        )
    })
}

fn validate_version_req_subset(narrowed: &VersionReq, original: &VersionReq) -> anyhow::Result<()> {
    let narrowed_bounds = version_req_bounds(narrowed)
        .with_context(|| format!("failed to compute bounds for narrowed requirement {narrowed}"))?;
    let original_bounds = version_req_bounds(original)
        .with_context(|| format!("failed to compute bounds for original requirement {original}"))?;
    if version_bounds_subset(&narrowed_bounds, &original_bounds) {
        Ok(())
    } else {
        bail!(
            "narrowed requirement {narrowed} allows versions outside original requirement {original}"
        )
    }
}

fn previous_dependency_edges(
    previous_resolve: &cargo::core::resolver::Resolve,
) -> anyhow::Result<BTreeMap<DependencyEdgeKey, PackageId>> {
    let mut edges = BTreeMap::new();
    for parent in previous_resolve.iter() {
        for (selected_dependency, dependencies) in previous_resolve.deps(parent) {
            let mut inserted_dependency = false;
            for dependency in dependencies {
                inserted_dependency = true;
                edges.insert(
                    dependency_edge_key(parent, dependency)?,
                    selected_dependency,
                );
            }
            if !inserted_dependency {
                edges.insert(
                    selected_package_edge_key(parent, selected_dependency)?,
                    selected_dependency,
                );
            }
        }
    }
    Ok(edges)
}

#[cfg(test)]
mod test {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::task::Poll;

    use cargo::core::Dependency;
    use cargo::core::Package;
    use cargo::core::PackageId;
    use cargo::core::SourceId;
    use cargo::core::Summary;
    use cargo::core::Workspace;
    use cargo::core::registry::PackageRegistry;
    use cargo::core::resolver::CliFeatures;
    use cargo::core::resolver::HasDevUnits;
    use cargo::core::resolver::Resolve;
    use cargo::core::resolver::ResolveVersion;
    use cargo::sources::IndexSummary;
    use cargo::sources::SourceConfigMap;
    use cargo::sources::source::MaybePackage;
    use cargo::sources::source::QueryKind;
    use cargo::sources::source::Source as CargoSource;
    use cargo::util::CanonicalUrl;
    use cargo::util::Graph;
    use cargo::util::OptVersionReq;
    use foldhash::HashMap;
    use semver::Version;
    use semver::VersionReq;

    use super::DeterministicSource;
    use super::DeterministicSourceContext;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ResolverConstraint {
        parent: PackageId,
        package_name: String,
        source_id: SourceId,
        original_req: String,
        narrowed_req: VersionReq,
    }

    impl ResolverConstraint {
        fn new(
            parent: PackageId,
            package_name: &str,
            source_id: SourceId,
            original_req: &str,
            narrowed_req: VersionReq,
        ) -> Self {
            Self {
                parent,
                package_name: package_name.to_owned(),
                source_id,
                original_req: original_req.to_owned(),
                narrowed_req,
            }
        }

        fn key(&self) -> PackageId {
            self.parent
        }
    }

    struct ConstrainedSource<S> {
        delegate: S,
        constraints: HashMap<PackageId, Vec<ResolverConstraint>>,
    }

    impl<S> ConstrainedSource<S> {
        fn new(delegate: S, constraints: Vec<ResolverConstraint>) -> Self {
            Self {
                delegate,
                constraints: constraints.into_iter().fold(
                    HashMap::<PackageId, Vec<ResolverConstraint>>::default(),
                    |mut constraints, constraint| {
                        constraints
                            .entry(constraint.key())
                            .or_default()
                            .push(constraint);
                        constraints
                    },
                ),
            }
        }
    }

    impl<S: CargoSource> CargoSource for ConstrainedSource<S> {
        fn source_id(&self) -> SourceId {
            self.delegate.source_id()
        }

        fn replaced_source_id(&self) -> SourceId {
            self.delegate.replaced_source_id()
        }

        fn supports_checksums(&self) -> bool {
            self.delegate.supports_checksums()
        }

        fn requires_precise(&self) -> bool {
            self.delegate.requires_precise()
        }

        fn query(
            &mut self,
            dep: &Dependency,
            kind: QueryKind,
            f: &mut dyn FnMut(IndexSummary),
        ) -> Poll<anyhow::Result<()>> {
            let constraints = &self.constraints;
            self.delegate.query(dep, kind, &mut |summary| {
                f(summary.map_summary(|summary| {
                    let Some(package_constraints) = constraints.get(&summary.package_id()) else {
                        return summary;
                    };
                    summary.map_dependencies(|mut dependency| {
                        if let Some(constraint) = package_constraints.iter().find(|constraint| {
                            dependency.package_name().as_str() == constraint.package_name
                                && dependency.source_id() == constraint.source_id
                                && dependency.version_req().to_string() == constraint.original_req
                        }) {
                            dependency.set_version_req(OptVersionReq::Req(
                                constraint.narrowed_req.clone(),
                            ));
                        }
                        dependency
                    })
                }));
            })
        }

        fn invalidate_cache(&mut self) {
            self.delegate.invalidate_cache();
        }

        fn set_quiet(&mut self, quiet: bool) {
            self.delegate.set_quiet(quiet);
        }

        fn download(&mut self, pkg_id: PackageId) -> anyhow::Result<MaybePackage> {
            self.delegate.download(pkg_id)
        }

        fn finish_download(
            &mut self,
            pkg_id: PackageId,
            contents: Vec<u8>,
        ) -> anyhow::Result<Package> {
            self.delegate.finish_download(pkg_id, contents)
        }

        fn fingerprint(&self, pkg: &Package) -> anyhow::Result<String> {
            self.delegate.fingerprint(pkg)
        }

        fn describe(&self) -> String {
            self.delegate.describe()
        }

        fn add_to_yanked_whitelist(&mut self, pkgs: &[PackageId]) {
            self.delegate.add_to_yanked_whitelist(pkgs);
        }

        fn is_yanked(&mut self, pkg: PackageId) -> Poll<anyhow::Result<bool>> {
            self.delegate.is_yanked(pkg)
        }

        fn block_until_ready(&mut self) -> anyhow::Result<()> {
            self.delegate.block_until_ready()
        }

        fn verify(&self, pkg: PackageId) -> anyhow::Result<()> {
            self.delegate.verify(pkg)
        }
    }

    #[derive(Default)]
    struct RecordingSource {
        source_id: Option<SourceId>,
        summaries: Vec<Summary>,
        yanked: BTreeSet<PackageId>,
        yanked_whitelist: BTreeSet<PackageId>,
        queried_reqs: Vec<(String, String, Option<Version>)>,
    }

    impl RecordingSource {
        fn new(source_id: SourceId, summaries: Vec<Summary>) -> Self {
            Self {
                source_id: Some(source_id),
                summaries,
                yanked: BTreeSet::new(),
                yanked_whitelist: BTreeSet::new(),
                queried_reqs: Vec::new(),
            }
        }
    }

    impl CargoSource for RecordingSource {
        fn source_id(&self) -> SourceId {
            self.source_id.expect("source id must be set")
        }

        fn supports_checksums(&self) -> bool {
            true
        }

        fn requires_precise(&self) -> bool {
            false
        }

        fn query(
            &mut self,
            dep: &Dependency,
            kind: QueryKind,
            f: &mut dyn FnMut(IndexSummary),
        ) -> Poll<anyhow::Result<()>> {
            self.queried_reqs.push((
                dep.package_name().as_str().to_owned(),
                dep.version_req().to_string(),
                dep.version_req().locked_version().cloned(),
            ));
            for summary in &self.summaries {
                if dep
                    .version_req()
                    .locked_version()
                    .is_some_and(|version| summary.package_id().version() != version)
                {
                    continue;
                }
                if dep.matches(summary) {
                    let package_id = summary.package_id();
                    if self.yanked.contains(&package_id) {
                        if kind == QueryKind::RejectedVersions
                            || self.yanked_whitelist.contains(&package_id)
                        {
                            f(IndexSummary::Yanked(summary.clone()));
                        }
                    } else {
                        f(IndexSummary::Candidate(summary.clone()));
                    }
                }
            }
            Poll::Ready(Ok(()))
        }

        fn invalidate_cache(&mut self) {}

        fn set_quiet(&mut self, _quiet: bool) {}

        fn download(&mut self, pkg_id: PackageId) -> anyhow::Result<MaybePackage> {
            anyhow::bail!("unexpected download of {pkg_id}")
        }

        fn finish_download(
            &mut self,
            pkg_id: PackageId,
            _contents: Vec<u8>,
        ) -> anyhow::Result<Package> {
            anyhow::bail!("unexpected finish_download of {pkg_id}")
        }

        fn fingerprint(&self, pkg: &Package) -> anyhow::Result<String> {
            Ok(pkg.package_id().to_string())
        }

        fn describe(&self) -> String {
            "recording source".to_owned()
        }

        fn add_to_yanked_whitelist(&mut self, pkgs: &[PackageId]) {
            self.yanked_whitelist.extend(pkgs);
        }

        fn is_yanked(&mut self, pkg: PackageId) -> Poll<anyhow::Result<bool>> {
            Poll::Ready(Ok(self.yanked.contains(&pkg)))
        }

        fn block_until_ready(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn verify(&self, _pkg: PackageId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn registry_source_id() -> SourceId {
        SourceId::from_url("registry+https://github.com/rust-lang/crates.io-index").unwrap()
    }

    fn git_source_id() -> SourceId {
        SourceId::from_url(
            "git+https://example.com/git-parent.git?rev=0123456789abcdef0123456789abcdef01234567",
        )
        .unwrap()
    }

    fn summary(name: &str, version: &str, source_id: SourceId) -> Summary {
        let package_id = PackageId::try_new(name, version, source_id).unwrap();
        Summary::new(
            package_id,
            Vec::new(),
            &BTreeMap::new(),
            None::<String>,
            None,
        )
        .unwrap()
    }

    fn dependency(name: &str, version_req: &str, source_id: SourceId) -> Dependency {
        Dependency::parse(name, Some(version_req), source_id).unwrap()
    }

    fn dependency_with_features(
        name: &str,
        version_req: &str,
        source_id: SourceId,
        features: &[&str],
    ) -> Dependency {
        let mut dependency = dependency(name, version_req, source_id);
        dependency.set_features(features.iter().copied());
        dependency
    }

    fn renamed_dependency(
        name_in_toml: &str,
        package_name: &str,
        version_req: &str,
        source_id: SourceId,
    ) -> Dependency {
        let mut dependency = dependency(package_name, version_req, source_id);
        dependency.set_explicit_name_in_toml(name_in_toml);
        dependency
    }

    fn optional_dependency(name: &str, version_req: &str, source_id: SourceId) -> Dependency {
        let mut dependency = dependency(name, version_req, source_id);
        dependency.set_optional(true);
        dependency
    }

    fn summary_with_deps(
        name: &str,
        version: &str,
        source_id: SourceId,
        dependencies: Vec<Dependency>,
    ) -> Summary {
        let package_id = PackageId::try_new(name, version, source_id).unwrap();
        Summary::new(
            package_id,
            dependencies,
            &BTreeMap::new(),
            None::<String>,
            None,
        )
        .unwrap()
    }

    fn summary_with_deps_and_features(
        name: &str,
        version: &str,
        source_id: SourceId,
        dependencies: Vec<Dependency>,
        features: &[(&str, &[&str])],
    ) -> Summary {
        let package_id = PackageId::try_new(name, version, source_id).unwrap();
        let features = features
            .iter()
            .map(|(feature, values)| {
                (
                    (*feature).into(),
                    values.iter().map(|value| (*value).into()).collect(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        Summary::new(package_id, dependencies, &features, None::<String>, None).unwrap()
    }

    fn resolver_constraint(
        parent_name: &str,
        parent_version: &str,
        package_name: &str,
        original_req: &str,
        narrowed_req: &str,
        source_id: SourceId,
    ) -> ResolverConstraint {
        ResolverConstraint::new(
            PackageId::try_new(parent_name, parent_version, source_id).unwrap(),
            package_name,
            source_id,
            original_req,
            VersionReq::parse(narrowed_req).unwrap(),
        )
    }

    fn deterministic_source_context<'gctx>(
        source_config: SourceConfigMap<'gctx>,
        third_party_dir: PathBuf,
        known_sources: impl IntoIterator<Item = SourceId>,
        discovered_sources: Rc<RefCell<BTreeSet<SourceId>>>,
    ) -> DeterministicSourceContext<'gctx> {
        DeterministicSourceContext {
            source_config,
            previous_edges: BTreeMap::new(),
            fixups_dir: third_party_dir.join("fixups"),
            known_sources: Rc::new(known_sources.into_iter().collect()),
            discovered_sources,
            root_patch_summaries: BTreeMap::new(),
        }
    }

    fn deterministic_source_context_with_root_patches<'gctx>(
        source_config: SourceConfigMap<'gctx>,
        third_party_dir: PathBuf,
        known_sources: impl IntoIterator<Item = SourceId>,
        root_patch_summaries: BTreeMap<CanonicalUrl, Vec<Summary>>,
    ) -> DeterministicSourceContext<'gctx> {
        DeterministicSourceContext {
            source_config,
            previous_edges: BTreeMap::new(),
            fixups_dir: third_party_dir.join("fixups"),
            known_sources: Rc::new(known_sources.into_iter().collect()),
            discovered_sources: Rc::new(RefCell::new(BTreeSet::new())),
            root_patch_summaries,
        }
    }

    #[test]
    fn test_deterministic_source_ids_include_git_workspace_dependencies() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
git_parent = { git = "https://example.com/git-parent.git", rev = "0123456789abcdef0123456789abcdef01234567" }
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();

        let sources = super::deterministic_source_ids(&workspace, None, &gctx).unwrap();

        assert!(sources.iter().any(|source_id| source_id.is_git()));
    }

    #[test]
    fn test_deterministic_source_ids_do_not_preload_previous_git_sources() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let crates_io_source_id = SourceId::crates_io(&gctx).unwrap();
        let registry_source_id =
            SourceId::from_url("registry+https://example.com/alt-index").unwrap();
        let git_source_id = git_source_id();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();

        let registry_parent =
            PackageId::try_new("registry-parent", "1.0.0", registry_source_id).unwrap();
        let registry_child =
            PackageId::try_new("registry-child", "1.0.0", registry_source_id).unwrap();
        let git_parent = PackageId::try_new("git-parent", "1.0.0", git_source_id).unwrap();
        let git_child = PackageId::try_new("git-child", "1.0.0", git_source_id).unwrap();
        let mut graph: Graph<PackageId, HashSet<Dependency>> = Graph::new();
        graph.add(registry_parent);
        graph.add(registry_child);
        graph.add(git_parent);
        graph.add(git_child);
        graph.link(registry_parent, git_child).insert(dependency(
            "git-child",
            "=1.0.0",
            git_source_id,
        ));
        graph.link(git_parent, registry_child).insert(dependency(
            "registry-child",
            "=1.0.0",
            registry_source_id,
        ));
        let previous_resolve = Resolve::new(
            graph,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Vec::new(),
            ResolveVersion::V4,
            Default::default(),
        );

        let sources =
            super::deterministic_source_ids(&workspace, Some(&previous_resolve), &gctx).unwrap();

        assert!(sources.contains(&crates_io_source_id));
        assert!(sources.contains(&registry_source_id));
        assert!(!sources.contains(&git_source_id));
    }

    #[test]
    fn test_constrained_source_narrows_parent_dependencies() {
        let source_id = registry_source_id();
        let delegate = RecordingSource::new(
            source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    source_id,
                    vec![
                        dependency("alpha", ">=0.1, <0.3", source_id),
                        dependency("beta", ">=1, <3", source_id),
                        dependency("gamma", ">=3, <4", source_id),
                    ],
                ),
                summary("other_dep", "1.0.0", source_id),
            ],
        );
        let constraints = vec![
            resolver_constraint(
                "root_dep",
                "1.0.0",
                "alpha",
                ">=0.1, <0.3",
                "^0.1",
                source_id,
            ),
            resolver_constraint("root_dep", "1.0.0", "beta", ">=1, <3", "^2", source_id),
        ];
        let mut source = ConstrainedSource::new(delegate, constraints);

        let mut dependency_reqs = Vec::new();
        let result = source.query(
            &dependency("root_dep", "=1.0.0", source_id),
            QueryKind::Exact,
            &mut |summary| {
                dependency_reqs = summary
                    .as_summary()
                    .dependencies()
                    .iter()
                    .map(|dependency| {
                        (
                            dependency.package_name().to_string(),
                            dependency.version_req().to_string(),
                        )
                    })
                    .collect();
            },
        );
        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(
            dependency_reqs,
            vec![
                ("alpha".to_owned(), "^0.1".to_owned()),
                ("beta".to_owned(), "^2".to_owned()),
                ("gamma".to_owned(), ">=3, <4".to_owned()),
            ]
        );
    }

    #[test]
    fn test_constrained_source_scopes_constraints_to_parent() {
        let source_id = registry_source_id();
        let delegate = RecordingSource::new(
            source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    source_id,
                    vec![dependency("alpha", ">=0.1, <0.3", source_id)],
                ),
                summary_with_deps(
                    "other_dep",
                    "1.0.0",
                    source_id,
                    vec![dependency("alpha", ">=0.1, <0.3", source_id)],
                ),
            ],
        );
        let constraints = vec![resolver_constraint(
            "root_dep",
            "1.0.0",
            "alpha",
            ">=0.1, <0.3",
            "^0.1",
            source_id,
        )];
        let mut source = ConstrainedSource::new(delegate, constraints);

        let mut root_dep_req = None;
        let root_result = source.query(
            &dependency("root_dep", ">=1, <2", source_id),
            QueryKind::Exact,
            &mut |summary| {
                root_dep_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );
        let mut other_dep_req = None;
        let other_result = source.query(
            &dependency("other_dep", ">=1, <2", source_id),
            QueryKind::Exact,
            &mut |summary| {
                other_dep_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );

        assert!(matches!(root_result, Poll::Ready(Ok(()))));
        assert!(matches!(other_result, Poll::Ready(Ok(()))));
        assert_eq!(root_dep_req.as_deref(), Some("^0.1"));
        assert_eq!(other_dep_req.as_deref(), Some(">=0.1, <0.3"));
    }

    #[test]
    fn test_constrained_source_is_used_by_resolver() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
root_dep = "=1.0.0"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let delegate = RecordingSource::new(
            source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    source_id,
                    vec![
                        dependency("alpha", ">=0.1, <0.3", source_id),
                        dependency("beta", ">=1, <3", source_id),
                    ],
                ),
                summary("alpha", "0.1.0", source_id),
                summary("alpha", "0.2.0", source_id),
                summary("beta", "1.0.0", source_id),
                summary("beta", "2.0.0", source_id),
            ],
        );
        let constraints = vec![
            resolver_constraint(
                "root_dep",
                "1.0.0",
                "alpha",
                ">=0.1, <0.3",
                "^0.1",
                source_id,
            ),
            resolver_constraint("root_dep", "1.0.0", "beta", ">=1, <3", "^2", source_id),
        ];
        registry.add_preloaded(Box::new(ConstrainedSource::new(delegate, constraints)));

        let mut resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let mut resolved = resolve.iter().collect::<Vec<_>>();
        resolved.sort();

        let package_versions = resolved
            .iter()
            .map(|pkg| format!("{} {}", pkg.name(), pkg.version()))
            .collect::<Vec<_>>();
        assert_eq!(
            package_versions,
            vec![
                "alpha 0.1.0",
                "beta 2.0.0",
                "resolver-fixture 0.0.0",
                "root_dep 1.0.0",
            ]
        );

        cargo::ops::write_pkg_lockfile(&workspace, &mut resolve).unwrap();
        let lockfile = fs::read_to_string(tempdir.path().join("Cargo.lock")).unwrap();
        assert!(lockfile.contains("name = \"alpha\""));
        assert!(lockfile.contains("version = \"0.1.0\""));
        assert!(lockfile.contains("name = \"beta\""));
        assert!(lockfile.contains("version = \"2.0.0\""));
    }

    #[test]
    fn test_deterministic_source_rewrites_candidate_dependencies_before_resolve_selection() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
a = "=1.0.0"
"#,
        )
        .unwrap();
        fs::create_dir_all(tempdir.path().join("fixups/a")).unwrap();
        fs::write(
            tempdir.path().join("fixups/a/fixups.toml"),
            r#"
[resolver.dependencies.b]
narrow_to = "0.10"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry =
            PackageRegistry::new_with_source_config(&gctx, source_config.clone()).unwrap();
        let discovered_sources = Rc::new(RefCell::new(BTreeSet::new()));
        let context = deterministic_source_context(
            source_config,
            tempdir.path().to_owned(),
            [source_id],
            discovered_sources,
        );
        registry.add_preloaded(Box::new(DeterministicSource::new(
            RecordingSource::new(
                source_id,
                vec![
                    summary_with_deps(
                        "a",
                        "1.0.0",
                        source_id,
                        vec![dependency("b", ">=0.8, <0.12", source_id)],
                    ),
                    summary_with_deps(
                        "b",
                        "0.10.0",
                        source_id,
                        vec![dependency("c", ">=1, <3", source_id)],
                    ),
                    summary("b", "0.11.0", source_id),
                    summary("c", "1.0.0", source_id),
                    summary("c", "2.0.0", source_id),
                ],
            ),
            context,
        )));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let resolved = resolve
            .iter()
            .map(|package| {
                (
                    package.name().as_str().to_owned(),
                    package.version().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(resolved["b"], "0.10.0");
        assert_eq!(resolved["c"], "2.0.0");
    }

    #[test]
    fn test_deterministic_source_discovers_and_rewrites_alternate_registry_sources() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        fs::create_dir_all(tempdir.path().join("fixups/a")).unwrap();
        fs::write(
            tempdir.path().join("fixups/a/fixups.toml"),
            r#"
[resolver.dependencies.b]
narrow_to = "0.10"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let primary_source_id = SourceId::crates_io(&gctx).unwrap();
        let alternate_source_id =
            SourceId::from_url("registry+https://example.com/alt-index").unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();

        let first_pass_discovered_sources = Rc::new(RefCell::new(BTreeSet::new()));
        let mut first_pass_source = DeterministicSource::new(
            RecordingSource::new(
                primary_source_id,
                vec![summary_with_deps(
                    "a",
                    "1.0.0",
                    primary_source_id,
                    vec![dependency("b", ">=0.8, <0.12", alternate_source_id)],
                )],
            ),
            deterministic_source_context(
                source_config.clone(),
                tempdir.path().to_owned(),
                [primary_source_id],
                Rc::clone(&first_pass_discovered_sources),
            ),
        );
        let first_pass_result = first_pass_source.query(
            &dependency("a", "=1.0.0", primary_source_id),
            QueryKind::Exact,
            &mut |_| {},
        );
        assert!(matches!(first_pass_result, Poll::Ready(Ok(()))));
        assert_eq!(
            first_pass_discovered_sources.borrow().clone(),
            BTreeSet::from([alternate_source_id])
        );

        let second_pass_discovered_sources = Rc::new(RefCell::new(BTreeSet::new()));
        let mut second_pass_source = DeterministicSource::new(
            RecordingSource::new(
                alternate_source_id,
                vec![
                    summary_with_deps(
                        "b",
                        "0.10.0",
                        alternate_source_id,
                        vec![dependency("c", ">=1, <3", alternate_source_id)],
                    ),
                    summary("c", "1.0.0", alternate_source_id),
                    summary("c", "2.0.0", alternate_source_id),
                ],
            ),
            deterministic_source_context(
                source_config,
                tempdir.path().to_owned(),
                [primary_source_id, alternate_source_id],
                Rc::clone(&second_pass_discovered_sources),
            ),
        );
        let mut rewritten_req = None;
        let second_pass_result = second_pass_source.query(
            &dependency("b", "=0.10.0", alternate_source_id),
            QueryKind::Exact,
            &mut |summary| {
                rewritten_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );
        assert!(matches!(second_pass_result, Poll::Ready(Ok(()))));
        assert_eq!(
            VersionReq::parse(&rewritten_req.unwrap()).unwrap(),
            VersionReq::parse(">=2.0.0, <3.0.0").unwrap()
        );
        assert!(second_pass_discovered_sources.borrow().is_empty());
    }

    #[test]
    fn test_deterministic_source_discovers_git_dependency_sources_without_rewriting_edge() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let registry_source_id = SourceId::crates_io(&gctx).unwrap();
        let git_source_id = git_source_id();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let discovered_sources = Rc::new(RefCell::new(BTreeSet::new()));
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                registry_source_id,
                vec![summary_with_deps(
                    "parent",
                    "1.0.0",
                    registry_source_id,
                    vec![dependency("git_child", "=0.1.0", git_source_id)],
                )],
            ),
            deterministic_source_context(
                source_config,
                tempdir.path().to_owned(),
                [registry_source_id],
                Rc::clone(&discovered_sources),
            ),
        );

        let mut rewritten_dependency = None;
        let result = source.query(
            &dependency("parent", "=1.0.0", registry_source_id),
            QueryKind::Exact,
            &mut |summary| {
                rewritten_dependency = Some(summary.as_summary().dependencies()[0].clone());
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        let rewritten_dependency = rewritten_dependency.unwrap();
        assert_eq!(rewritten_dependency.source_id(), git_source_id);
        assert_eq!(rewritten_dependency.version_req().to_string(), "=0.1.0");
        assert_eq!(
            discovered_sources.borrow().clone(),
            BTreeSet::from([git_source_id])
        );
    }

    #[test]
    fn test_deterministic_source_prefers_root_patch_candidates_for_fresh_lanes() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::create_dir_all(tempdir.path().join("alpha_patch/src")).unwrap();
        fs::write(
            tempdir.path().join("alpha_patch/Cargo.toml"),
            r#"
[package]
name = "alpha"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::write(tempdir.path().join("alpha_patch/src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
root_dep = "=1.0.0"

[patch.crates-io]
alpha = { path = "alpha_patch" }
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let root_patch_summaries = super::root_patch_summaries(&workspace, &source_config).unwrap();
        let mut registry =
            PackageRegistry::new_with_source_config(&gctx, source_config.clone()).unwrap();
        registry.add_preloaded(Box::new(DeterministicSource::new(
            RecordingSource::new(
                source_id,
                vec![
                    summary_with_deps(
                        "root_dep",
                        "1.0.0",
                        source_id,
                        vec![dependency("alpha", ">=0.1, <0.3", source_id)],
                    ),
                    summary("alpha", "0.2.0", source_id),
                ],
            ),
            deterministic_source_context_with_root_patches(
                source_config,
                tempdir.path().to_owned(),
                [source_id],
                root_patch_summaries,
            ),
        )));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let selected_alpha = resolve
            .iter()
            .find(|pkg| pkg.name().as_str() == "alpha")
            .unwrap();
        assert_eq!(selected_alpha.version(), &Version::parse("0.1.0").unwrap());
        assert!(selected_alpha.source_id().is_path());
    }

    #[test]
    fn test_resolver_fixup_uses_dependency_key_for_renamed_dependency_edges() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        fs::create_dir_all(tempdir.path().join("fixups/a")).unwrap();
        fs::write(
            tempdir.path().join("fixups/a/fixups.toml"),
            r#"
[resolver.dependencies.http-02x]
narrow_to = "0.2"

[resolver.dependencies.http-1x]
narrow_to = "1"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                source_id,
                vec![summary_with_deps(
                    "a",
                    "1.0.0",
                    source_id,
                    vec![
                        renamed_dependency("http-02x", "http", ">=0.2, <0.4", source_id),
                        renamed_dependency("http-1x", "http", ">=1, <3", source_id),
                    ],
                )],
            ),
            deterministic_source_context(
                source_config,
                tempdir.path().to_owned(),
                [source_id],
                Rc::new(RefCell::new(BTreeSet::new())),
            ),
        );

        let mut dependency_reqs = BTreeMap::new();
        let result = source.query(
            &dependency("a", "=1.0.0", source_id),
            QueryKind::Exact,
            &mut |summary| {
                dependency_reqs = summary
                    .as_summary()
                    .dependencies()
                    .iter()
                    .map(|dependency| {
                        (
                            dependency.name_in_toml().to_string(),
                            dependency.version_req().to_string(),
                        )
                    })
                    .collect();
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(dependency_reqs["http-02x"], "^0.2");
        assert_eq!(dependency_reqs["http-1x"], "^1");
    }

    #[test]
    fn test_previous_locked_edges_use_dependency_key_for_renamed_dependencies() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let parent = PackageId::try_new("a", "1.0.0", source_id).unwrap();
        let alpha_01x = renamed_dependency("alpha-01x", "alpha", ">=0.1, <0.3", source_id);
        let alpha_02x = renamed_dependency("alpha-02x", "alpha", ">=0.2, <0.4", source_id);
        let mut context = deterministic_source_context(
            source_config,
            tempdir.path().to_owned(),
            [source_id],
            Rc::new(RefCell::new(BTreeSet::new())),
        );
        context.previous_edges = BTreeMap::from([
            (
                super::dependency_edge_key(parent, &alpha_01x).unwrap(),
                PackageId::try_new("alpha", "0.1.0", source_id).unwrap(),
            ),
            (
                super::dependency_edge_key(parent, &alpha_02x).unwrap(),
                PackageId::try_new("alpha", "0.2.0", source_id).unwrap(),
            ),
        ]);
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                source_id,
                vec![
                    summary_with_deps("a", "1.0.0", source_id, vec![alpha_01x, alpha_02x]),
                    summary("alpha", "0.1.0", source_id),
                    summary("alpha", "0.2.0", source_id),
                    summary("alpha", "0.3.0", source_id),
                ],
            ),
            context,
        );

        let mut dependency_reqs = BTreeMap::new();
        let result = source.query(
            &dependency("a", "=1.0.0", source_id),
            QueryKind::Exact,
            &mut |summary| {
                dependency_reqs = summary
                    .as_summary()
                    .dependencies()
                    .iter()
                    .map(|dependency| {
                        (
                            dependency.name_in_toml().to_string(),
                            dependency.version_req().to_string(),
                        )
                    })
                    .collect();
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(dependency_reqs["alpha-01x"], ">=0.1.0, <0.2.0");
        assert_eq!(dependency_reqs["alpha-02x"], ">=0.2.0, <0.3.0");
    }

    #[test]
    fn test_previous_locked_edges_apply_across_parent_semver_lane() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let previous_parent = PackageId::try_new("a", "1.0.0", source_id).unwrap();
        let previous_dependency = dependency("b", ">=1, <3", source_id);
        let mut context = deterministic_source_context(
            source_config,
            tempdir.path().to_owned(),
            [source_id],
            Rc::new(RefCell::new(BTreeSet::new())),
        );
        context.previous_edges = BTreeMap::from([(
            super::dependency_edge_key(previous_parent, &previous_dependency).unwrap(),
            PackageId::try_new("b", "1.2.3", source_id).unwrap(),
        )]);
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                source_id,
                vec![
                    summary_with_deps(
                        "a",
                        "1.0.1",
                        source_id,
                        vec![dependency("b", ">=1, <3", source_id)],
                    ),
                    summary("b", "1.2.3", source_id),
                    summary("b", "2.0.0", source_id),
                ],
            ),
            context,
        );

        let mut rewritten_req = None;
        let result = source.query(
            &dependency("a", "=1.0.1", source_id),
            QueryKind::Exact,
            &mut |summary| {
                rewritten_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(rewritten_req.as_deref(), Some(">=1.0.0, <2.0.0"));
    }

    #[test]
    fn test_previous_locked_edges_treat_crates_io_registry_and_sparse_as_same_source() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let registry_source_id = registry_source_id();
        let sparse_source_id = SourceId::from_url("sparse+https://index.crates.io/").unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let previous_parent =
            PackageId::try_new("gcloud-sdk", "0.28.5", registry_source_id).unwrap();
        let previous_dependency = dependency("reqwest", ">=0.12.7", registry_source_id);
        let mut context = deterministic_source_context(
            source_config,
            tempdir.path().to_owned(),
            [sparse_source_id],
            Rc::new(RefCell::new(BTreeSet::new())),
        );
        context.previous_edges = BTreeMap::from([(
            super::dependency_edge_key(previous_parent, &previous_dependency).unwrap(),
            PackageId::try_new("reqwest", "0.12.28", registry_source_id).unwrap(),
        )]);
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                sparse_source_id,
                vec![
                    summary_with_deps(
                        "gcloud-sdk",
                        "0.28.5",
                        sparse_source_id,
                        vec![dependency("reqwest", ">=0.12.7", sparse_source_id)],
                    ),
                    summary("reqwest", "0.12.28", sparse_source_id),
                    summary("reqwest", "0.13.4", sparse_source_id),
                ],
            ),
            context,
        );

        let mut rewritten_req = None;
        let result = source.query(
            &dependency("gcloud-sdk", "=0.28.5", sparse_source_id),
            QueryKind::Exact,
            &mut |summary| {
                rewritten_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(rewritten_req.as_deref(), Some(">=0.12.7, <0.13.0"));
    }

    #[test]
    fn test_previous_locked_edges_fall_back_to_lockfile_selected_package_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let registry_source_id = registry_source_id();
        let sparse_source_id = SourceId::from_url("sparse+https://index.crates.io/").unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let previous_parent =
            PackageId::try_new("gcloud-sdk", "0.28.5", registry_source_id).unwrap();
        let previous_reqwest =
            PackageId::try_new("reqwest", "0.12.28", registry_source_id).unwrap();
        let mut graph: Graph<PackageId, HashSet<Dependency>> = Graph::new();
        graph.add(previous_parent);
        graph.add(previous_reqwest);
        graph.link(previous_parent, previous_reqwest);
        let previous_resolve = Resolve::new(
            graph,
            Default::default(),
            Default::default(),
            Default::default(),
            Default::default(),
            Vec::new(),
            ResolveVersion::V4,
            Default::default(),
        );
        let mut context = deterministic_source_context(
            source_config,
            tempdir.path().to_owned(),
            [sparse_source_id],
            Rc::new(RefCell::new(BTreeSet::new())),
        );
        context.previous_edges = super::previous_dependency_edges(&previous_resolve).unwrap();
        let mut source = DeterministicSource::new(
            RecordingSource::new(
                sparse_source_id,
                vec![
                    summary_with_deps(
                        "gcloud-sdk",
                        "0.28.5",
                        sparse_source_id,
                        vec![dependency("reqwest", ">=0.12.7", sparse_source_id)],
                    ),
                    summary("reqwest", "0.12.28", sparse_source_id),
                    summary("reqwest", "0.13.4", sparse_source_id),
                ],
            ),
            context,
        );

        let mut rewritten_req = None;
        let result = source.query(
            &dependency("gcloud-sdk", "=0.28.5", sparse_source_id),
            QueryKind::Exact,
            &mut |summary| {
                rewritten_req = Some(
                    summary.as_summary().dependencies()[0]
                        .version_req()
                        .to_string(),
                );
            },
        );

        assert!(matches!(result, Poll::Ready(Ok(()))));
        assert_eq!(rewritten_req.as_deref(), Some(">=0.12.7, <0.13.0"));
    }

    #[test]
    fn test_constrained_source_uses_source_identity_for_duplicate_package_names() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::create_dir_all(tempdir.path().join("alpha_local/src")).unwrap();
        fs::write(
            tempdir.path().join("alpha_local/Cargo.toml"),
            r#"
[package]
name = "alpha"
version = "9.0.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::write(tempdir.path().join("alpha_local/src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
local_alpha = { package = "alpha", path = "alpha_local" }
root_dep = "=1.0.0"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let registry_source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let delegate = RecordingSource::new(
            registry_source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    registry_source_id,
                    vec![dependency("alpha", ">=0.1, <0.3", registry_source_id)],
                ),
                summary("alpha", "0.1.0", registry_source_id),
                summary("alpha", "0.2.0", registry_source_id),
            ],
        );
        let constraints = vec![resolver_constraint(
            "root_dep",
            "1.0.0",
            "alpha",
            ">=0.1, <0.3",
            "^0.1",
            registry_source_id,
        )];
        registry.add_preloaded(Box::new(ConstrainedSource::new(delegate, constraints)));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let mut resolved = resolve
            .iter()
            .map(|pkg| {
                format!(
                    "{} {} {}",
                    pkg.name(),
                    pkg.version(),
                    if pkg.source_id() == registry_source_id {
                        "registry"
                    } else {
                        "local"
                    }
                )
            })
            .collect::<Vec<_>>();
        resolved.sort();

        assert_eq!(
            resolved,
            vec![
                "alpha 0.1.0 registry",
                "alpha 9.0.0 local",
                "resolver-fixture 0.0.0 local",
                "root_dep 1.0.0 registry",
            ]
        );
    }

    #[test]
    fn test_constrained_source_preserves_patch_resolution() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::create_dir_all(tempdir.path().join("alpha_patch/src")).unwrap();
        fs::write(
            tempdir.path().join("alpha_patch/Cargo.toml"),
            r#"
[package]
name = "alpha"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        fs::write(tempdir.path().join("alpha_patch/src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
root_dep = "=1.0.0"

[patch.crates-io]
alpha = { path = "alpha_patch" }
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let registry_source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let delegate = RecordingSource::new(
            registry_source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    registry_source_id,
                    vec![dependency("alpha", ">=0.1, <0.3", registry_source_id)],
                ),
                summary("alpha", "0.2.0", registry_source_id),
            ],
        );
        let constraints = vec![resolver_constraint(
            "root_dep",
            "1.0.0",
            "alpha",
            ">=0.1, <0.3",
            "^0.1",
            registry_source_id,
        )];
        registry.add_preloaded(Box::new(ConstrainedSource::new(delegate, constraints)));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let mut resolved = resolve
            .iter()
            .map(|pkg| {
                format!(
                    "{} {} {}",
                    pkg.name(),
                    pkg.version(),
                    if pkg.source_id() == registry_source_id {
                        "registry"
                    } else {
                        "local"
                    }
                )
            })
            .collect::<Vec<_>>();
        resolved.sort();

        assert_eq!(
            resolved,
            vec![
                "alpha 0.1.0 local",
                "resolver-fixture 0.0.0 local",
                "root_dep 1.0.0 registry",
            ]
        );
    }

    #[test]
    fn test_constrained_source_uses_package_identity_for_renamed_dependencies() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
root_dep = "=1.0.0"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let delegate = RecordingSource::new(
            source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    source_id,
                    vec![renamed_dependency(
                        "renamed_alpha",
                        "alpha",
                        ">=0.1, <0.3",
                        source_id,
                    )],
                ),
                summary("alpha", "0.1.0", source_id),
                summary("alpha", "0.2.0", source_id),
            ],
        );
        let constraints = vec![resolver_constraint(
            "root_dep",
            "1.0.0",
            "alpha",
            ">=0.1, <0.3",
            "^0.1",
            source_id,
        )];
        registry.add_preloaded(Box::new(ConstrainedSource::new(delegate, constraints)));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let selected_alpha = resolve
            .iter()
            .find(|pkg| pkg.name().as_str() == "alpha")
            .unwrap();
        assert_eq!(selected_alpha.version(), &Version::parse("0.1.0").unwrap());
    }

    #[test]
    fn test_constrained_source_preserves_source_replacement() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::create_dir(tempdir.path().join("local-registry")).unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
root_dep = "=1.0.0"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let crates_io_source_id = SourceId::crates_io(&gctx).unwrap();
        let replacement_source_id =
            SourceId::for_local_registry(&tempdir.path().join("local-registry")).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let replacement_source = RecordingSource::new(
            replacement_source_id,
            vec![
                summary_with_deps(
                    "root_dep",
                    "1.0.0",
                    replacement_source_id,
                    vec![dependency("alpha", ">=0.1, <0.3", replacement_source_id)],
                ),
                summary("alpha", "0.1.0", replacement_source_id),
                summary("alpha", "0.2.0", replacement_source_id),
            ],
        );
        let replaced_source = cargo::sources::ReplacedSource::new(
            crates_io_source_id,
            replacement_source_id,
            Box::new(replacement_source),
        );
        let constraints = vec![resolver_constraint(
            "root_dep",
            "1.0.0",
            "alpha",
            ">=0.1, <0.3",
            "^0.1",
            crates_io_source_id,
        )];
        registry.add_preloaded(Box::new(ConstrainedSource::new(
            replaced_source,
            constraints,
        )));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let resolved = resolve
            .iter()
            .map(|pkg| (pkg.name().as_str().to_owned(), pkg))
            .collect::<BTreeMap<_, _>>();
        let alpha = resolved.get("alpha").unwrap();
        assert_eq!(alpha.version(), &Version::parse("0.1.0").unwrap());
        assert_eq!(alpha.source_id(), crates_io_source_id);
        let root_dep = resolved.get("root_dep").unwrap();
        assert_eq!(root_dep.version(), &Version::parse("1.0.0").unwrap());
        assert_eq!(root_dep.source_id(), crates_io_source_id);
        let root = resolved.get("resolver-fixture").unwrap();
        assert!(!root.source_id().is_registry());
    }

    #[test]
    fn test_constrained_source_preserves_feature_unification() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
parent_left = "=1.0.0"
parent_right = "=1.0.0"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();

        let delegate = RecordingSource::new(
            source_id,
            vec![
                summary_with_deps(
                    "parent_left",
                    "1.0.0",
                    source_id,
                    vec![dependency_with_features(
                        "feature_carrier",
                        ">=1, <2",
                        source_id,
                        &["left"],
                    )],
                ),
                summary_with_deps(
                    "parent_right",
                    "1.0.0",
                    source_id,
                    vec![dependency_with_features(
                        "feature_carrier",
                        ">=1, <2",
                        source_id,
                        &["right"],
                    )],
                ),
                summary_with_deps_and_features(
                    "feature_carrier",
                    "1.0.0",
                    source_id,
                    vec![
                        optional_dependency("left_dep", ">=1, <2", source_id),
                        optional_dependency("right_dep", ">=1, <2", source_id),
                    ],
                    &[("left", &["dep:left_dep"]), ("right", &["dep:right_dep"])],
                ),
                summary("left_dep", "1.0.0", source_id),
                summary("right_dep", "1.0.0", source_id),
            ],
        );
        let constraints = vec![resolver_constraint(
            "parent_left",
            "1.0.0",
            "feature_carrier",
            ">=1, <2",
            "^1",
            source_id,
        )];
        registry.add_preloaded(Box::new(ConstrainedSource::new(delegate, constraints)));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let mut package_versions = resolve
            .iter()
            .map(|pkg| format!("{} {}", pkg.name(), pkg.version()))
            .collect::<Vec<_>>();
        package_versions.sort();

        assert_eq!(
            package_versions,
            vec![
                "feature_carrier 1.0.0",
                "left_dep 1.0.0",
                "parent_left 1.0.0",
                "parent_right 1.0.0",
                "resolver-fixture 0.0.0",
                "right_dep 1.0.0",
            ]
        );
    }

    #[test]
    fn test_deterministic_resolve_preserves_legal_previous_lockfile_version() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
alpha = "1"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();

        let mut initial_registry =
            PackageRegistry::new_with_source_config(&gctx, source_config.clone()).unwrap();
        initial_registry.add_preloaded(Box::new(RecordingSource::new(
            source_id,
            vec![summary("alpha", "1.0.0", source_id)],
        )));
        let mut previous_resolve = cargo::ops::resolve_with_previous(
            &mut initial_registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        cargo::ops::write_pkg_lockfile(&workspace, &mut previous_resolve).unwrap();

        let previous_resolve = cargo::ops::load_pkg_lockfile(&workspace).unwrap().unwrap();
        let mut registry =
            PackageRegistry::new_with_source_config(&gctx, source_config.clone()).unwrap();
        registry.add_preloaded(Box::new(RecordingSource::new(
            source_id,
            vec![
                summary("alpha", "1.0.0", source_id),
                summary("alpha", "1.1.0", source_id),
            ],
        )));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            Some(&previous_resolve),
            None,
            &[],
            true,
        )
        .unwrap();
        let selected_alpha = resolve
            .iter()
            .find(|pkg| pkg.name().as_str() == "alpha")
            .unwrap();

        assert_eq!(selected_alpha.version(), &Version::parse("1.0.0").unwrap());
    }

    #[test]
    fn test_deterministic_resolve_updates_stale_previous_lockfile_version() {
        let tempdir = tempfile::tempdir().unwrap();
        let cargo_home = tempdir.path().join(".cargo");
        let manifest_path = tempdir.path().join("Cargo.toml");
        fs::create_dir(tempdir.path().join("src")).unwrap();
        fs::write(tempdir.path().join("src/lib.rs"), "").unwrap();
        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
alpha = "1"
"#,
        )
        .unwrap();

        let shell = cargo::core::Shell::new();
        let mut gctx =
            cargo::GlobalContext::new(shell, tempdir.path().to_owned(), cargo_home.clone());
        gctx.configure(0, true, None, false, false, false, &None, &[], &[])
            .unwrap();
        let source_id = SourceId::crates_io(&gctx).unwrap();
        let source_config = SourceConfigMap::new(&gctx).unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();

        let mut initial_registry =
            PackageRegistry::new_with_source_config(&gctx, source_config.clone()).unwrap();
        initial_registry.add_preloaded(Box::new(RecordingSource::new(
            source_id,
            vec![summary("alpha", "1.0.0", source_id)],
        )));
        let mut previous_resolve = cargo::ops::resolve_with_previous(
            &mut initial_registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        cargo::ops::write_pkg_lockfile(&workspace, &mut previous_resolve).unwrap();

        fs::write(
            &manifest_path,
            r#"
[package]
name = "resolver-fixture"
version = "0.0.0"
edition = "2021"

[dependencies]
alpha = ">=1.1, <2"
"#,
        )
        .unwrap();
        let workspace = Workspace::new(&manifest_path, &gctx).unwrap();
        let previous_resolve = cargo::ops::load_pkg_lockfile(&workspace).unwrap().unwrap();
        let mut registry = PackageRegistry::new_with_source_config(&gctx, source_config).unwrap();
        registry.add_preloaded(Box::new(RecordingSource::new(
            source_id,
            vec![
                summary("alpha", "1.0.0", source_id),
                summary("alpha", "1.1.0", source_id),
            ],
        )));

        let resolve = cargo::ops::resolve_with_previous(
            &mut registry,
            &workspace,
            &CliFeatures::new_all(true),
            HasDevUnits::Yes,
            Some(&previous_resolve),
            None,
            &[],
            true,
        )
        .unwrap();
        let selected_alpha = resolve
            .iter()
            .find(|pkg| pkg.name().as_str() == "alpha")
            .unwrap();

        assert_eq!(selected_alpha.version(), &Version::parse("1.1.0").unwrap());
    }
}
