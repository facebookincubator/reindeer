/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Context as _;
use anyhow::bail;

use crate::Args;
use crate::Paths;
use crate::cargo::GctxProperties;
use crate::cargo::make_gctx;
use crate::cargo::resolve_ws_deterministically_with_original_sources;
use crate::config::Config;

pub(crate) fn generate_lockfile(
    config: &Config,
    args: &Args,
    paths: &Paths,
    locked: bool,
) -> anyhow::Result<(cargo::GlobalContext, cargo::core::resolver::Resolve)> {
    let gctx = make_gctx(
        config,
        args,
        paths,
        GctxProperties {
            frozen: false,
            locked,
            offline: false,
            quiet: true,
            git_fetch_with_cli: true,
        },
    )?;

    let manifest_path = paths.manifest_path.clone();
    let ws = cargo::core::Workspace::new(&manifest_path, &gctx)?;

    eprintln!("Resolving workspace...");
    let fixups_dir = config.resolved_fixups_dir(&paths.third_party_dir);
    let resolve = if locked {
        resolve_locked_ws_with_original_sources(&ws, &gctx, &paths.lockfile_path)
    } else {
        resolve_ws_deterministically_with_original_sources(&ws, &gctx, paths, &fixups_dir)
    }
    .context("failed to resolve workspace")?;

    Ok((gctx, resolve))
}

fn resolve_locked_ws_with_original_sources<'gctx>(
    ws: &cargo::core::Workspace<'gctx>,
    gctx: &'gctx cargo::GlobalContext,
    lockfile_path: &Path,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    let Some(previous_resolve) = cargo::ops::load_pkg_lockfile(ws)? else {
        bail!(
            "locked vendoring requires {}; run `reindeer vendor` without `--locked` to create it",
            lockfile_path.display(),
        );
    };
    let source_config = cargo::sources::SourceConfigMap::empty(gctx)?;
    let mut registry =
        cargo::core::registry::PackageRegistry::new_with_source_config(gctx, source_config)?;
    let resolve = cargo::ops::resolve_with_previous(
        &mut registry,
        ws,
        &cargo::core::resolver::CliFeatures::new_all(true),
        cargo::core::resolver::HasDevUnits::Yes,
        Some(&previous_resolve),
        None,
        &[],
        true,
    )
    .with_context(|| {
        format!(
            "failed to resolve workspace from {}; run `reindeer vendor` without `--locked` to update Cargo.lock",
            lockfile_path.display(),
        )
    })?;
    locked_vendor_materialization_resolve(resolve, previous_resolve, lockfile_path)
}

pub(crate) fn locked_vendor_materialization_resolve(
    resolve: cargo::core::resolver::Resolve,
    previous_resolve: cargo::core::resolver::Resolve,
    lockfile_path: &Path,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    validate_deterministic_vendor_resolve_matches_lockfile(
        &resolve,
        &previous_resolve,
        lockfile_path,
    )?;
    Ok(previous_resolve)
}

fn validate_deterministic_vendor_resolve_matches_lockfile(
    resolve: &cargo::core::resolver::Resolve,
    previous_resolve: &cargo::core::resolver::Resolve,
    lockfile_path: &Path,
) -> anyhow::Result<()> {
    let resolved_packages = resolve.iter().collect::<BTreeSet<_>>();
    let locked_packages = previous_resolve.iter().collect::<BTreeSet<_>>();
    if resolved_packages == locked_packages {
        return Ok(());
    }

    let added = resolved_packages
        .difference(&locked_packages)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let removed = locked_packages
        .difference(&resolved_packages)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    bail!(
        "locked vendoring requires {} to match Cargo.toml; run `reindeer vendor` without `--locked` to update Cargo.lock. Added package(s): [{}]. Removed package(s): [{}].",
        lockfile_path.display(),
        format_package_diff(&added),
        format_package_diff(&removed),
    )
}

fn format_package_diff(packages: &[String]) -> String {
    const LIMIT: usize = 10;

    let mut summary = packages
        .iter()
        .take(LIMIT)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if packages.len() > LIMIT {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("+{} more", packages.len() - LIMIT));
    }
    summary
}
