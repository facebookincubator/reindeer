/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod fingerprint;
mod limit_reader;
mod materialization;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::io::ErrorKind;
use std::io::Read as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::thread;

use anyhow::Context;
use anyhow::bail;
use cargo::core::GitReference;
use cargo::core::Package;
use cargo::core::PackageId;
use cargo::core::SourceId;
use cargo::sources::CRATES_IO_REGISTRY;
use cargo::util::cache_lock::CacheLockMode;
use globset::Glob;
use globset::GlobSet;
use globset::GlobSetBuilder;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;
use sha2::Digest as _;
use walkdir::WalkDir;

use crate::Args;
use crate::Paths;
use crate::cargo::GctxProperties;
use crate::cargo::make_gctx;
use crate::cargo::run_cargo;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::config::VendorSourceConfig;
use crate::fast_vendor::fingerprint::vendor_dir_matches_expected_source;
use crate::fast_vendor::materialization::Materialization;
use crate::fast_vendor::materialization::materialize_expected_crate;
use crate::remap::RemapConfig;
use crate::remap::RemapSource;

static STAGING_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);
const SYNTHESIZED_BUILD_RS: &[u8] = b"fn main() {}\n";

/// Glob and gitignore rules for files to omit from `.cargo-checksum.json`.
///
/// Both `GlobSet` and `Gitignore` are `Sync`, so this struct can be shared
/// across threads via a shared reference.
struct ChecksumFilter {
    remove_globs: GlobSet,
    gitignore: Gitignore,
}

/// Filtering parameters passed into `fast_vendor`.
///
/// Controls which files are excluded from the vendor directory and checksums.
struct VendorFilters {
    /// Name of the BUCK file to exclude from extraction and checksums (e.g. `"BUCK"`).
    /// `None` means no exclusion (split mode is disabled).
    buck_file_name: Option<String>,
    /// Glob/gitignore rules for files to omit from `.cargo-checksum.json`.
    checksum_filter: Option<ChecksumFilter>,
}

struct ExpectedCrate {
    dst_name: String,
    dst: PathBuf,
    pkgdir: PathBuf,
    pkg_cksum: Option<String>,
    materialization: Materialization,
}

struct PendingCrate {
    pkg_id: PackageId,
    dst_name: String,
    dst: PathBuf,
    pkgdir: PathBuf,
    pkg_cksum: Option<String>,
    materialization: PendingMaterialization,
}

enum PendingMaterialization {
    RegistryArchive { archive: PathBuf },
    LoadFromPackageSet,
}

pub(crate) fn cargo_vendor(
    config: &Config,
    no_delete: bool,
    #[cfg(fbcode_build)] audit_sec: bool,
    #[cfg(fbcode_build)] no_fetch: bool,
    args: &Args,
    paths: &Paths,
) -> anyhow::Result<()> {
    let vendordir = Path::new("vendor"); // relative to third_party_dir
    let full_vendor_dir = paths.third_party_dir.join("vendor");

    match &config.vendor {
        VendorConfig::Off => {
            unreachable!(
                "VendorConfig::Off is only ever set during `reindeer buckify`, not `reindeer vendor`"
            );
        }
        VendorConfig::LocalRegistry => {
            let mut cmdline = vec![
                "local-registry",
                "-s",
                paths.lockfile_path.to_str().unwrap(),
                full_vendor_dir.to_str().unwrap(),
                "--git",
            ];
            if no_delete {
                cmdline.push("--no-delete");
            }
            log::info!("Running cargo {:?}", cmdline);
            let _ = run_cargo(config, Some(&paths.cargo_home), None, args, &cmdline)?;
            let mut remap = RemapConfig::default();
            remap.sources.insert(
                "crates-io".to_owned(),
                RemapSource {
                    registry: Some("sparse+https://index.crates.io/".to_owned()),
                    replace_with: Some("local-registry".to_owned()),
                    ..RemapSource::default()
                },
            );
            remap.sources.insert(
                "local-registry".to_owned(),
                RemapSource {
                    local_registry: Some(vendordir.to_owned()),
                    ..RemapSource::default()
                },
            );
            let config_toml = toml::to_string(&remap).context("failed to serialize config.toml")?;
            fs::write(paths.cargo_home.join("config.toml"), config_toml)?;
            assert!(is_vendored(config, paths)?);
        }
        VendorConfig::Source(source_config) => {
            log::info!("Running fast vendor (library mode)");
            let filters = build_filters(config, paths, source_config)?;
            fast_vendor(config, no_delete, args, paths, filters)?;
            assert!(is_vendored(config, paths)?);
        }
    }

    #[cfg(fbcode_build)]
    {
        if audit_sec {
            crate::audit_sec::audit_sec(paths, no_fetch).context("doing audit_sec")?;
        }
    }

    Ok(())
}

pub(crate) fn is_vendored(config: &Config, paths: &Paths) -> anyhow::Result<bool> {
    // .cargo/config.toml is Cargo's preferred name for the config, but .cargo/config
    // is the older name so it takes priority if present.
    let mut cargo_config_path = paths.cargo_home.join("config");
    let result = match fs::read_to_string(&cargo_config_path) {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            cargo_config_path = paths.cargo_home.join("config.toml");
            match fs::read_to_string(&cargo_config_path) {
                Ok(content) => Ok(content),
                Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    };

    let content = result.with_context(|| {
        format!(
            "Failed to read cargo config {}",
            cargo_config_path.display(),
        )
    })?;

    let remap_config: RemapConfig = toml::from_str(&content)
        .context(format!("Failed to parse {}", cargo_config_path.display()))?;

    let source_name = match config.vendor {
        VendorConfig::LocalRegistry => "local-registry",
        VendorConfig::Source(_) => "vendored-sources",
        _ => return Ok(false),
    };
    match remap_config.sources.get(source_name) {
        Some(source) => match config.vendor {
            VendorConfig::LocalRegistry => Ok(source.local_registry.is_some()),
            VendorConfig::Source(_) | VendorConfig::Off => Ok(source.directory.is_some()),
        },
        None => Ok(false),
    }
}

fn build_filters(
    config: &Config,
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<VendorFilters> {
    let buck_file_name = if config.buck.split {
        Some((*config.buck.file_name).clone())
    } else {
        None
    };

    let checksum_filter = build_checksum_filter(
        config.buck.split,
        &config.buck.file_name,
        &source_config.gitignore_checksum_exclude,
        &paths.third_party_dir,
    )?;

    Ok(VendorFilters {
        buck_file_name,
        checksum_filter,
    })
}

/// Build the checksum filter from primitive inputs.
///
/// Returns `None` when no filtering is needed. Extracted from `build_filters`
/// so the logic can be tested without constructing `Config` or `Paths`.
fn build_checksum_filter(
    buck_split: bool,
    buck_file_name: &str,
    gitignore_checksum_exclude: &[PathBuf],
    third_party_dir: &Path,
) -> anyhow::Result<Option<ChecksumFilter>> {
    // Build a checksum filter when there are explicit excludes configured, or
    // when split mode is enabled (BUCK files must be excluded from checksums to
    // match the on-disk exclusion that split mode also applies).
    let needs_filter = !gitignore_checksum_exclude.is_empty() || buck_split;
    if !needs_filter {
        return Ok(None);
    }

    log::debug!(
        "vendor.gitignore_checksum_exclude = {:?}",
        gitignore_checksum_exclude,
    );

    let mut remove_globs = GlobSetBuilder::new();
    // Exclude the BUCK file from checksums only when split mode is enabled.
    // This keeps checksum exclusion aligned with on-disk exclusion: both are
    // gated on the same split-mode condition.
    if buck_split {
        let buck_glob = Glob::new(buck_file_name)
            .with_context(|| format!("Invalid buck.file_name glob `{}`", buck_file_name))?;
        remove_globs.add(buck_glob);
    }
    let remove_globs = remove_globs.build()?;

    let mut gitignore = GitignoreBuilder::new(third_party_dir);
    for ignore in gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(third_party_dir.join(ignore)) {
            log::warn!(
                "Failed to read ignore file {}: {}; skipping",
                ignore.display(),
                err
            );
        }
    }
    let gitignore = gitignore.build()?;

    log::debug!(
        "remove_globs {:#?}, gitignore {:#?}",
        remove_globs,
        gitignore
    );

    Ok(Some(ChecksumFilter {
        remove_globs,
        gitignore,
    }))
}

/// Vendor crates using cargo-as-a-library, with parallel archive extraction.
///
/// This replaces the `cargo vendor` subprocess call with direct API usage,
/// enabling parallel extraction of `.crate` archives via `thread::scope`.
/// The result is a populated `vendor/` directory and a `.cargo/config.toml`
/// with source replacement entries.
fn fast_vendor(
    config: &Config,
    no_delete: bool,
    args: &Args,
    paths: &Paths,
    filters: VendorFilters,
) -> anyhow::Result<()> {
    let vendor_dir = paths.third_party_dir.join("vendor");
    let cargo_config_path = paths.cargo_home.join("config.toml");
    let gctx = make_gctx(
        config,
        args,
        paths,
        GctxProperties {
            frozen: false,
            locked: false,
            offline: false,
            quiet: true,
            git_fetch_with_cli: true,
        },
    )?;

    let manifest_path = paths.manifest_path.clone();
    let ws = cargo::core::Workspace::new(&manifest_path, &gctx)?;

    eprintln!("Resolving workspace...");
    let resolve = resolve_ws_with_original_sources(&ws, &gctx, false)
        .context("failed to resolve workspace")?;

    let original_package_ids = resolve
        .iter()
        .filter(|pkg_id| !pkg_id.source_id().is_path())
        .collect::<Vec<_>>();

    fs::create_dir_all(&vendor_dir)?;

    // Collect existing top-level entries for cleanup (unless --no-delete).
    let mut to_remove: BTreeSet<PathBuf> = if no_delete {
        BTreeSet::new()
    } else {
        collect_vendor_cleanup_entries(&vendor_dir)?
    };

    let mut pending_crates = Vec::new();
    let mut package_load_ids = Vec::new();
    let mut destinations = BTreeMap::new();
    let mut sources: BTreeSet<SourceId> = BTreeSet::new();
    let mut prepared_crates = 0usize;

    if let n @ 1.. = original_package_ids.len() {
        eprintln!("Preparing expected contents for {n} vendored crates...");
    }

    for pkg_id in resolve.iter() {
        // Skip path dependencies -- they're already in the source tree.
        // Any preexisting vendor/<name>-<version> directory for this crate
        // stays in `to_remove` so it will be deleted as stale.
        if pkg_id.source_id().is_path() {
            continue;
        }

        if pkg_id.source_id().is_git() {
            eprintln!("Preparing git crate {pkg_id}...");
        }
        let dst_name = format!("{}-{}", pkg_id.name(), pkg_id.version());
        if let Some(previous) = destinations.insert(dst_name.clone(), pkg_id) {
            bail!(
                "multiple packages resolve to vendored directory `{}`: {} and {}",
                dst_name,
                previous,
                pkg_id,
            );
        }
        let dst = vendor_dir.join(&dst_name);
        let pkgdir = dst
            .strip_prefix(&paths.third_party_dir)
            .expect("dst is always under third_party_dir")
            .to_path_buf();
        remove_expected_vendor_entries_from_cleanup(
            &mut to_remove,
            &vendor_dir,
            &dst,
            pkg_id.name().as_str(),
            filters.buck_file_name.as_deref(),
        );
        sources.insert(pkg_id.source_id());

        let pkg_cksum = resolve.checksums().get(&pkg_id).and_then(|c| c.clone());

        let materialization = if pkg_id.source_id().is_registry() {
            let pkg_cksum = pkg_cksum.as_deref().with_context(|| {
                format!("missing lockfile checksum for registry package {pkg_id}")
            })?;
            if let Some(archive) =
                find_cached_registry_archive(&paths.cargo_home, pkg_id, pkg_cksum)?
            {
                PendingMaterialization::RegistryArchive { archive }
            } else {
                package_load_ids.push(pkg_id);
                PendingMaterialization::LoadFromPackageSet
            }
        } else {
            if pkg_id.source_id().is_git() && pkg_id.source_id().precise_git_fragment().is_none() {
                bail!("git package {pkg_id} is not locked to a precise revision");
            }
            package_load_ids.push(pkg_id);
            PendingMaterialization::LoadFromPackageSet
        };

        pending_crates.push(PendingCrate {
            pkg_id,
            dst_name,
            dst,
            pkgdir,
            pkg_cksum,
            materialization,
        });
        prepared_crates += 1;
        if prepared_crates == original_package_ids.len() || prepared_crates.is_multiple_of(250) {
            eprintln!(
                "Prepared {prepared_crates}/{} expected crate contents...",
                original_package_ids.len(),
            );
        }
    }

    let original_package_set = if package_load_ids.is_empty() {
        None
    } else {
        let source_ids_to_load = package_load_ids
            .iter()
            .map(|pkg_id| pkg_id.source_id())
            .collect::<BTreeSet<_>>();
        eprintln!(
            "Preparing {} external source(s) for {} git or missing-cache crate(s)...",
            source_ids_to_load.len(),
            package_load_ids.len(),
        );

        let original_source_config = cargo::sources::SourceConfigMap::empty(&gctx)?;
        let mut original_source_map = cargo::sources::source::SourceMap::new();
        let yanked_whitelist = std::collections::HashSet::new();
        {
            let _lock = gctx.acquire_package_cache_lock(CacheLockMode::DownloadExclusive)?;
            for (index, source_id) in source_ids_to_load.iter().copied().enumerate() {
                if source_id.is_git() {
                    eprintln!("Fetching git source {source_id}...");
                }
                let mut source = original_source_config
                    .load(source_id, &yanked_whitelist)
                    .with_context(|| format!("failed to load original source {source_id}"))?;
                cargo::sources::source::Source::block_until_ready(&mut source)?;
                original_source_map.insert(source);
                let completed = index + 1;
                if completed == source_ids_to_load.len() || completed.is_multiple_of(10) {
                    eprintln!(
                        "Prepared {completed}/{} external sources...",
                        source_ids_to_load.len(),
                    );
                }
            }
        }

        let package_set =
            cargo::core::PackageSet::new(&package_load_ids, original_source_map, &gctx)?;
        eprintln!(
            "Downloading/materializing {} git or missing-cache crate(s)...",
            package_load_ids.len(),
        );
        package_set
            .get_many(package_load_ids.iter().copied())
            .context("failed to download original packages")?;
        Some(package_set)
    };

    let mut expected_crates = Vec::new();
    for pending in pending_crates {
        let materialization = match pending.materialization {
            PendingMaterialization::RegistryArchive { archive } => {
                Materialization::RegistryArchive { archive }
            }
            PendingMaterialization::LoadFromPackageSet => {
                let package_set = original_package_set
                    .as_ref()
                    .expect("package set should be loaded when crates need it");
                let original_pkg = package_set
                    .get_one(pending.pkg_id)
                    .context("failed to fetch original package")?;
                if pending.pkg_id.source_id().is_registry() {
                    let pkg_cksum = pending.pkg_cksum.as_deref().with_context(|| {
                        format!(
                            "missing lockfile checksum for registry package {}",
                            pending.pkg_id
                        )
                    })?;
                    let archive = find_cached_registry_archive(
                        &paths.cargo_home,
                        pending.pkg_id,
                        pkg_cksum,
                    )?
                    .with_context(|| {
                        format!(
                            "missing cached registry archive for {} after Cargo package load",
                            pending.pkg_id,
                        )
                    })?;
                    Materialization::RegistryArchive { archive }
                } else {
                    let src_root = original_pkg.root().to_path_buf();
                    let file_paths = cargo::sources::PathSource::new(
                        original_pkg.root(),
                        pending.pkg_id.source_id(),
                        &gctx,
                    )
                    .list_files(original_pkg)?
                    .into_iter()
                    .map(|entry| entry.into_path_buf())
                    .collect::<Vec<_>>();
                    let normalized_cargo_toml = if pending.pkg_id.source_id().is_git() {
                        Some(prepare_git_cargo_toml_for_vendor(
                            original_pkg,
                            &file_paths,
                            &src_root,
                            &gctx,
                        )?)
                    } else {
                        None
                    };

                    Materialization::CopyFiles {
                        src_root,
                        file_paths,
                        normalized_cargo_toml,
                    }
                }
            }
        };
        expected_crates.push(ExpectedCrate {
            dst_name: pending.dst_name,
            dst: pending.dst,
            pkgdir: pending.pkgdir,
            pkg_cksum: pending.pkg_cksum,
            materialization,
        });
    }

    // Generate .cargo/config.toml with source replacements before mutating the
    // vendor tree so serialization failures are caught early.
    let vendor_config = generate_vendor_config(&sources, &vendor_dir)?;

    // Compare expected crates in parallel and replace only mismatches.
    // Filters are shared by reference across threads; ChecksumFilter is Sync.
    let num_threads = std::thread::available_parallelism().map_or(8, |n| n.get());
    let chunk_size = expected_crates.len().div_ceil(num_threads.max(1));
    let filters = &filters;
    let total = expected_crates.len();
    let progress = AtomicUsize::new(0);

    if total > 0 {
        eprintln!("Checking {total} vendored crates...");
    }

    let progress = &progress;
    let process_result = thread::scope(|s| {
        let handles: Vec<_> = expected_crates
            .chunks(chunk_size.max(1))
            .map(|chunk| {
                s.spawn(move || {
                    for expected in chunk {
                        process_expected_crate(expected, filters)
                            .with_context(|| format!("failed to vendor {}", expected.dst_name))?;
                        let completed = progress.fetch_add(1, Ordering::Relaxed) + 1;
                        if completed == total || completed.is_multiple_of(250) {
                            eprintln!("Checked {completed}/{total} vendored crates...");
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("vendor thread panicked")?;
        }
        Ok::<_, anyhow::Error>(())
    });
    process_result.context(
        "fast vendoring failed; if the vendor tree was partially updated, run `sl revert third-party/rust/vendor third-party/rust/.cargo/config.toml`, then `sl purge --cwd \"$FBSOURCE\" --all third-party/rust/vendor`, then retry `fbcode/common/rust/tools/reindeer/vendor --fast`",
    )?;

    // Remove stale vendor entries left behind by prior runs or by drift in the
    // top-level vendor directory.
    for stale in &to_remove {
        remove_existing_path(stale)?;
    }

    fs::create_dir_all(&paths.cargo_home)?;
    write_regular_file_if_changed(&cargo_config_path, vendor_config.as_bytes())
        .with_context(|| format!("failed to write {}", cargo_config_path.display()))?;

    Ok(())
}

fn resolve_ws_with_original_sources<'gctx>(
    ws: &cargo::core::Workspace<'gctx>,
    gctx: &'gctx cargo::GlobalContext,
    dry_run: bool,
) -> anyhow::Result<cargo::core::resolver::Resolve> {
    let source_config = cargo::sources::SourceConfigMap::empty(gctx)?;
    let mut registry =
        cargo::core::registry::PackageRegistry::new_with_source_config(gctx, source_config)?;
    let previous_resolve = cargo::ops::load_pkg_lockfile(ws)?;
    let mut resolve = cargo::ops::resolve_with_previous(
        &mut registry,
        ws,
        &cargo::core::resolver::CliFeatures::new_all(true),
        cargo::core::resolver::HasDevUnits::Yes,
        previous_resolve.as_ref(),
        None,
        &[],
        true,
    )?;

    let print_changes = if !ws.is_ephemeral() && ws.require_optional_deps() {
        if dry_run {
            true
        } else {
            cargo::ops::write_pkg_lockfile(ws, &mut resolve)?
        }
    } else {
        false
    };
    if print_changes {
        cargo::ops::print_lockfile_changes(ws, previous_resolve.as_ref(), &resolve, &mut registry)?;
    }

    Ok(resolve)
}

fn path_file_type_no_follow(path: &Path) -> anyhow::Result<Option<fs::FileType>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata.file_type())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
    }
}

fn file_sha256(path: &Path) -> anyhow::Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let len = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if len == 0 {
            break;
        }
        hasher.update(&buffer[..len]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn bytes_sha256(bytes: &[u8]) -> String {
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn remove_existing_path(path: &Path) -> anyhow::Result<()> {
    let Some(file_type) = path_file_type_no_follow(path)? else {
        return Ok(());
    };
    if file_type.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove stale vendor dir {}", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove stale vendor file {}", path.display()))?;
    }
    Ok(())
}

fn prepare_regular_file_target(path: &Path) -> anyhow::Result<()> {
    let Some(file_type) = path_file_type_no_follow(path)? else {
        return Ok(());
    };
    if file_type.is_file() {
        return Ok(());
    }
    remove_existing_path(path)
}

fn write_regular_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    prepare_regular_file_target(path)?;
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

fn write_regular_file_if_changed(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    if path_file_type_no_follow(path)?.is_some_and(|file_type| file_type.is_file())
        && fs::read(path).with_context(|| format!("failed to read {}", path.display()))? == contents
    {
        return Ok(());
    }
    write_regular_file(path, contents)
}

fn collect_vendor_cleanup_entries(vendor_dir: &Path) -> anyhow::Result<BTreeSet<PathBuf>> {
    if !vendor_dir.exists() {
        return Ok(BTreeSet::new());
    }

    fs::read_dir(vendor_dir)?
        .map(|entry| {
            let entry = entry?;
            Ok(entry.path())
        })
        .collect::<Result<BTreeSet<_>, io::Error>>()
        .map_err(anyhow::Error::from)
        .with_context(|| format!("failed to read {}", vendor_dir.display()))
}

fn remove_expected_vendor_entries_from_cleanup(
    to_remove: &mut BTreeSet<PathBuf>,
    vendor_dir: &Path,
    source_dir: &Path,
    package_name: &str,
    buck_file_name: Option<&str>,
) {
    to_remove.remove(source_dir);
    if buck_file_name.is_some() {
        to_remove.remove(&vendor_dir.join(package_name));
    }
}

fn find_cached_registry_archive(
    cargo_home: &Path,
    pkg_id: PackageId,
    expected_checksum: &str,
) -> anyhow::Result<Option<PathBuf>> {
    find_cached_registry_archive_by_tarball(cargo_home, &pkg_id.tarball_name(), expected_checksum)
}

fn find_cached_registry_archive_by_tarball(
    cargo_home: &Path,
    tarball_name: &str,
    expected_checksum: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let cache_dir = cargo_home.join("registry").join("cache");
    let entries = match fs::read_dir(&cache_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", cache_dir.display()));
        }
    };

    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let archive = entry.path().join(tarball_name);
        let Some(file_type) = path_file_type_no_follow(&archive)? else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }

        match file_sha256(&archive) {
            Ok(actual) if actual == expected_checksum => return Ok(Some(archive)),
            Ok(_) => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(None)
}

fn is_split_buck_file(relative: &Path, buck_file_name: Option<&str>) -> bool {
    buck_file_name.is_some_and(|name| relative == Path::new(name))
}

fn process_expected_crate(expected: &ExpectedCrate, filters: &VendorFilters) -> anyhow::Result<()> {
    if vendor_dir_matches_expected_source(expected, filters)? {
        return Ok(());
    }

    let (staging_root, staging_dst) = make_staging_destination(&expected.dst)?;
    let result = (|| -> anyhow::Result<()> {
        materialize_expected_crate(expected, &staging_dst, filters)?;
        replace_vendor_dir(&staging_dst, &expected.dst)?;
        Ok(())
    })();
    let _ = fs::remove_dir_all(&staging_root);
    result
}

fn checksum_excluded(
    pkgdir: &Path,
    relative: &Path,
    key: &str,
    filter: Option<&ChecksumFilter>,
) -> bool {
    filter.is_some_and(|filter| {
        filter.remove_globs.is_match(key) || gitignore_excluded(pkgdir, relative, Some(filter))
    })
}

fn source_excluded(pkgdir: &Path, relative: &Path, filters: &VendorFilters) -> bool {
    materialization_excluded(relative, filters)
        || gitignore_excluded(pkgdir, relative, filters.checksum_filter.as_ref())
}

fn materialization_excluded(relative: &Path, filters: &VendorFilters) -> bool {
    !vendor_this(relative) || is_split_buck_file(relative, filters.buck_file_name.as_deref())
}

fn gitignore_excluded(pkgdir: &Path, relative: &Path, filter: Option<&ChecksumFilter>) -> bool {
    filter.is_some_and(|filter| {
        filter
            .gitignore
            .matched_path_or_any_parents(pkgdir.join(relative), false)
            .is_ignore()
    })
}

/// Returns `true` if this relative path should be included in the vendor dir.
///
/// Excludes VCS bookkeeping files anywhere in the package. Cargo's own helper
/// only checks the package root, but fbsource does not track these files in
/// vendored third-party trees, so treating them as source would make clean
/// checkouts fail the fast no-op comparison.
fn vendor_this(relative: &Path) -> bool {
    !relative.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".gitattributes" | ".gitignore" | ".git" | ".hg" | ".cargo-ok")
        )
    })
}

fn prepare_git_cargo_toml_for_vendor(
    pkg: &Package,
    file_paths: &[PathBuf],
    src_root: &Path,
    gctx: &cargo::GlobalContext,
) -> anyhow::Result<String> {
    let packaged_files = file_paths
        .iter()
        .map(|path| {
            path.strip_prefix(src_root)
                .with_context(|| format!("{} is not under {}", path.display(), src_root.display()))
                .map(Path::to_path_buf)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let vendored_pkg = prepare_package_for_vendor(pkg, &packaged_files, gctx)?;
    vendored_pkg
        .manifest()
        .to_normalized_contents()
        .context("failed to render prepared Cargo.toml")
}

fn prepare_package_for_vendor(
    pkg: &Package,
    packaged_files: &[PathBuf],
    gctx: &cargo::GlobalContext,
) -> cargo::CargoResult<Package> {
    let contents = pkg.manifest().contents();
    let document = pkg.manifest().document();
    let original_toml = {
        let mut manifest = pkg.manifest().normalized_toml().clone();
        {
            let package = manifest
                .package
                .as_mut()
                .expect("vendored manifests must have packages");
            if let Some(custom_build_scripts) =
                package.normalized_build().expect("previously normalized")
            {
                let mut included_scripts = Vec::new();
                for script in custom_build_scripts {
                    let path = normalize_manifest_path(Path::new(script));
                    if packaged_files.contains(&path) {
                        let path = path
                            .into_os_string()
                            .into_string()
                            .map_err(|_err| anyhow::format_err!("non-UTF8 `package.build`"))?;
                        included_scripts.push(cargo::util::toml::normalize_path_string_sep(path));
                    } else {
                        gctx.shell().warn(format!(
                            "ignoring `package.build` entry `{}` as it is not included in the published package",
                            path.display()
                        ))?;
                    }
                }
                package.build = Some(match included_scripts.len() {
                    0 => toml::Value::Boolean(false).try_into()?,
                    1 => toml::Value::String(included_scripts[0].clone()).try_into()?,
                    _ => toml::Value::Array(
                        included_scripts
                            .into_iter()
                            .map(toml::Value::String)
                            .collect(),
                    )
                    .try_into()?,
                });
            }
        }

        manifest.lib = if let Some(target) = &manifest.lib {
            cargo::util::toml::prepare_target_for_publish(
                target,
                Some(packaged_files),
                "library",
                gctx,
            )?
        } else {
            None
        };
        manifest.bin = cargo::util::toml::prepare_targets_for_publish(
            manifest.bin.as_ref(),
            Some(packaged_files),
            "binary",
            gctx,
        )?;
        manifest.example = cargo::util::toml::prepare_targets_for_publish(
            manifest.example.as_ref(),
            Some(packaged_files),
            "example",
            gctx,
        )?;
        manifest.test = cargo::util::toml::prepare_targets_for_publish(
            manifest.test.as_ref(),
            Some(packaged_files),
            "test",
            gctx,
        )?;
        manifest.bench = cargo::util::toml::prepare_targets_for_publish(
            manifest.bench.as_ref(),
            Some(packaged_files),
            "benchmark",
            gctx,
        )?;

        manifest
    };
    let normalized_toml = original_toml.clone();
    let features = pkg.manifest().unstable_features().clone();
    let workspace_config = pkg.manifest().workspace_config().clone();
    let source_id = pkg.package_id().source_id();
    let mut warnings = Default::default();
    let mut errors = Default::default();
    let manifest = cargo::util::toml::to_real_manifest(
        contents.to_owned(),
        document.clone(),
        original_toml,
        normalized_toml,
        features,
        workspace_config,
        source_id,
        pkg.manifest_path(),
        pkg.manifest().is_embedded(),
        gctx,
        &mut warnings,
        &mut errors,
    )?;
    Ok(Package::new(manifest, pkg.manifest_path()))
}

fn normalize_manifest_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut normalized = if let Some(component @ Component::Prefix(..)) = components.peek().cloned()
    {
        components.next();
        PathBuf::from(component.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => normalized.push(Component::RootDir),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.ends_with(Component::ParentDir) {
                    normalized.push(Component::ParentDir);
                } else {
                    let popped = normalized.pop();
                    if !popped && !normalized.has_root() {
                        normalized.push(Component::ParentDir);
                    }
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }

    normalized
}

fn make_staging_destination(dst: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let parent = dst
        .parent()
        .context("vendor destination must have a parent")?;
    let leaf = dst
        .file_name()
        .context("vendor destination must have a file name")?;
    let suffix = STAGING_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staging_root = parent.join(format!(
        ".reindeer-staging-{}-{}",
        std::process::id(),
        suffix,
    ));
    let _ = fs::remove_dir_all(&staging_root);
    fs::create_dir_all(&staging_root)
        .with_context(|| format!("failed to create {}", staging_root.display()))?;
    Ok((staging_root.clone(), staging_root.join(leaf)))
}

fn replace_vendor_dir(staged_dst: &Path, dst: &Path) -> anyhow::Result<()> {
    remove_existing_path(dst)?;
    fs::rename(staged_dst, dst).with_context(|| {
        format!(
            "failed to move staged vendor dir {} into {}",
            staged_dst.display(),
            dst.display(),
        )
    })?;
    Ok(())
}

/// Walk a directory and compute SHA256 checksums for all regular files.
///
/// Files matched by checksum globs are left on disk but omitted from the
/// returned map. VCS bookkeeping files and configured gitignore matches are
/// treated as absent from the vendored source tree.
fn compute_dir_checksums_filtered(
    root: &Path,
    pkgdir: &Path,
    filter: Option<&ChecksumFilter>,
) -> anyhow::Result<BTreeMap<String, String>> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() => Some(Ok(entry)),
            Ok(_) => None,
            Err(err) => Some(Err(anyhow::Error::from(err))),
        })
        .map(|entry| {
            let e = entry?;
            let path = e.path();
            let relative = path
                .strip_prefix(root)
                .expect("walkdir entry must be under root");
            let key = relative.to_str().expect("non-UTF8 path").replace('\\', "/");
            if key == ".cargo-checksum.json" {
                return Ok(None);
            }
            if !vendor_this(relative) || gitignore_excluded(pkgdir, relative, filter) {
                log::trace!("checksum: skipping source-excluded file {}", key);
                return Ok(None);
            }
            if checksum_excluded(pkgdir, relative, &key, filter) {
                log::trace!("checksum: skipping excluded file {}", key);
                return Ok(None);
            }
            let contents = fs::read(path)?;
            let hash = bytes_sha256(&contents);
            Ok(Some((key, hash)))
        })
        .filter_map(|entry| entry.transpose())
        .collect()
}

fn checksum_json_bytes(
    pkg_cksum: Option<&str>,
    file_cksums: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::json!({
        "package": pkg_cksum,
        "files": file_cksums,
    });
    Ok(json.to_string().into_bytes())
}

/// Generate a `.cargo/config.toml` string with source replacement entries
/// that point all resolved sources to the `vendored-sources` directory.
fn generate_vendor_config(
    sources: &BTreeSet<SourceId>,
    _vendor_dir: &Path,
) -> anyhow::Result<String> {
    let mut remap = RemapConfig::default();
    let merged = "vendored-sources";

    for sid in sources {
        let name = if sid.is_crates_io() {
            CRATES_IO_REGISTRY.to_string()
        } else {
            sid.without_precise().as_url().to_string()
        };

        let source = if sid.is_crates_io() {
            RemapSource {
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_remote_registry() {
            RemapSource {
                registry: Some(sid.url().to_string()),
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else if sid.is_git() {
            let mut branch = None;
            let mut tag = None;
            let mut rev = None;
            if let Some(reference) = sid.git_reference() {
                match reference {
                    GitReference::Branch(b) => branch = Some(b.clone()),
                    GitReference::Tag(t) => tag = Some(t.clone()),
                    GitReference::Rev(r) => rev = Some(r.clone()),
                    GitReference::DefaultBranch => {}
                }
            }
            RemapSource {
                git: Some(sid.url().to_string()),
                branch,
                tag,
                rev,
                replace_with: Some(merged.to_owned()),
                ..RemapSource::default()
            }
        } else {
            anyhow::bail!("unsupported source type: {}", sid);
        };

        remap.sources.insert(name, source);
    }

    // Always write [source.vendored-sources] so that is_vendored() returns true
    // even for workspaces with only path dependencies (where sources is empty).
    remap.sources.insert(
        merged.to_owned(),
        RemapSource {
            directory: Some(PathBuf::from("vendor")),
            ..RemapSource::default()
        },
    );

    toml::to_string(&remap).context("failed to serialize vendor config")
}

/// Extract crate name from a versioned directory
fn extract_crate_name(dir_name: &str) -> &str {
    let dot_pos = dir_name.find('.').unwrap_or(dir_name.len());
    match dir_name[..dot_pos].rfind('-') {
        Some(i) => &dir_name[..i],
        None => dir_name,
    }
}

/// Delete vendored directories for crates not in the `vendored` list (extern_crates).
/// Crates not in the vendored set are expected to be resolved by external Buck
/// targets, so their vendored sources are unnecessary.
pub(crate) fn cleanup_extern_crates(config: &Config, paths: &Paths) -> anyhow::Result<()> {
    let Some(extern_config) = &config.extern_crates else {
        log::info!("No extern_crates configured, nothing to clean up");
        return Ok(());
    };

    let vendor_path = paths.third_party_dir.join("vendor");
    if !vendor_path.try_exists()? {
        log::info!("No vendor directory found at {}", vendor_path.display());
        return Ok(());
    }

    let mut removed = 0;
    for entry in fs::read_dir(&vendor_path)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir_name = entry.file_name();
        let dir_name = dir_name.to_string_lossy();
        let crate_name = extract_crate_name(&dir_name);

        if !extern_config.vendored.contains(crate_name) {
            let path = entry.path();
            log::info!("Removing non-vendored crate dir: {}", path.display());
            fs::remove_dir_all(&path)
                .with_context(|| format!("failed to remove {}", path.display()))?;
            removed += 1;
        }
    }

    log::info!("Removed {} non-vendored crate directories", removed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use globset::GlobBuilder;
    use globset::GlobSetBuilder;
    use ignore::gitignore::GitignoreBuilder;
    use sha2::Digest;

    use crate::fast_vendor::ChecksumFilter;
    use crate::fast_vendor::build_checksum_filter;
    use crate::fast_vendor::collect_vendor_cleanup_entries;
    use crate::fast_vendor::compute_dir_checksums_filtered;
    use crate::fast_vendor::extract_crate_name;
    use crate::fast_vendor::file_sha256;
    use crate::fast_vendor::find_cached_registry_archive_by_tarball;
    use crate::fast_vendor::generate_vendor_config;
    use crate::fast_vendor::is_split_buck_file;
    use crate::fast_vendor::make_staging_destination;
    use crate::fast_vendor::remove_expected_vendor_entries_from_cleanup;
    use crate::fast_vendor::vendor_this;

    // Build a ChecksumFilter that matches a single glob pattern.
    fn glob_filter(pattern: &str) -> ChecksumFilter {
        let mut builder = GlobSetBuilder::new();
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .unwrap(),
        );
        let gitignore = GitignoreBuilder::new("/").build().unwrap();
        ChecksumFilter {
            remove_globs: builder.build().unwrap(),
            gitignore,
        }
    }

    pub(crate) fn gitignore_filter(pattern: &str) -> ChecksumFilter {
        let remove_globs = GlobSetBuilder::new().build().unwrap();
        let mut builder = GitignoreBuilder::new("/");
        builder.add_line(None, pattern).unwrap();
        let gitignore = builder.build().unwrap();
        ChecksumFilter {
            remove_globs,
            gitignore,
        }
    }

    #[test]
    fn test_extract_crate_name() {
        assert_eq!(extract_crate_name("serde-1.0.228"), "serde");
        assert_eq!(extract_crate_name("aes-gcm-siv-0.11.1"), "aes-gcm-siv");
        assert_eq!(extract_crate_name("asn1-rs-derive-0.6.0"), "asn1-rs-derive");
        assert_eq!(
            extract_crate_name("curve25519-dalek-derive-0.1.0"),
            "curve25519-dalek-derive"
        );
        assert_eq!(extract_crate_name("ed25519-dalek-2.2.0"), "ed25519-dalek");
        assert_eq!(extract_crate_name("base64-url-2.0.2"), "base64-url");
        assert_eq!(extract_crate_name("lz4-sys-1.11.1+lz4-1.10.0"), "lz4-sys");
        assert_eq!(extract_crate_name("x86_64-0.15.2"), "x86_64");
        assert_eq!(extract_crate_name("aarch64-0.1.0"), "aarch64");
        assert_eq!(extract_crate_name("base64-0.22.1"), "base64");
        assert_eq!(extract_crate_name("sha2-0.10.9"), "sha2");
        assert_eq!(extract_crate_name("sha3-0.10.8"), "sha3");
        assert_eq!(extract_crate_name("p256-0.13.2"), "p256");
        assert_eq!(extract_crate_name("p384-0.13.1"), "p384");
        assert_eq!(extract_crate_name("h2-0.4.13"), "h2");
        assert_eq!(extract_crate_name("md-5-0.10.6"), "md-5");
        assert_eq!(extract_crate_name("blake3-1.8.2"), "blake3");
        assert_eq!(extract_crate_name("adler32-1.2.0"), "adler32");
        assert_eq!(extract_crate_name("anymap3-1.0.1"), "anymap3");
        assert_eq!(extract_crate_name("akd-0.12.0-pre.11"), "akd");
        assert_eq!(extract_crate_name("sha2-0.11.0-pre.4"), "sha2");
    }

    #[test]
    fn test_build_checksum_filter_split_true_no_other_excludes() {
        // When split=true and no other excludes, the filter must still be built
        // and BUCK must be in the remove set (to align with on-disk exclusion).
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(true, "BUCK", &[], dir.path())
            .expect("build_checksum_filter should succeed");

        let filter = filter.expect("filter should be Some when split=true");
        assert!(
            filter.remove_globs.is_match("BUCK"),
            "BUCK must be in checksum exclusion when split=true"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_false_no_excludes() {
        // When split=false and no other excludes, no filter is needed.
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = build_checksum_filter(false, "BUCK", &[], dir.path())
            .expect("build_checksum_filter should succeed");

        assert!(
            filter.is_none(),
            "filter should be None when split=false and no excludes"
        );
    }

    #[test]
    fn test_build_checksum_filter_split_true_invalid_buck_glob_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = build_checksum_filter(true, "[", &[], dir.path());

        assert!(result.is_err(), "invalid buck.file_name glob should error");
        let err = result.err().expect("error should be present");

        assert!(
            format!("{err:#}").contains("Invalid buck.file_name glob `[`"),
            "error should point at invalid buck.file_name glob: {err:#}"
        );
    }

    #[test]
    fn test_checksum_excludes_buck_entry() {
        // A BUCK file present in the extracted directory should be excluded
        // from the checksum map (it was skipped at extraction time by the
        // include filter, so it won't be on disk here -- but even if it were,
        // a glob on "BUCK" would exclude it).
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join("Cargo.toml"), b"[package]").unwrap();

        // Use a glob filter that matches the BUCK file name.
        let filter = glob_filter("BUCK");
        let pkgdir = std::path::Path::new("vendor/sourdough-starter-1.0.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        // lib.rs and Cargo.toml should be present; BUCK should not be.
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should be in checksum map"
        );
        assert!(
            !cksums.contains_key("BUCK"),
            "BUCK should be excluded from checksum map"
        );
    }

    #[test]
    fn test_checksum_computation_skips_checksum_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();
        fs::write(root.join(".cargo-checksum.json"), b"not source").unwrap();

        let cksums =
            compute_dir_checksums_filtered(root, Path::new("vendor/example-0.1.0"), None).unwrap();

        assert!(cksums.contains_key("lib.rs"));
        assert!(
            !cksums.contains_key(".cargo-checksum.json"),
            "checksum metadata is generated, not source content"
        );
    }

    #[test]
    fn test_checksum_filter_glob_keeps_file_on_disk() {
        // Files matched by checksum_exclude globs should remain on disk
        // but be absent from the checksum map.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("pancake-stack.h"), b"// header").unwrap();
        fs::write(root.join("lib.rs"), b"fn main() {}").unwrap();

        // Filter that excludes all .h files.
        let filter = glob_filter("*.h");
        let pkgdir = std::path::Path::new("vendor/flux-capacitor-1.21.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("pancake-stack.h"),
            ".h files should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("lib.rs"),
            "lib.rs should be in checksum map"
        );

        // File must still exist on disk (checksum_exclude only affects the map).
        assert!(
            root.join("pancake-stack.h").exists(),
            ".h file should remain on disk"
        );
    }

    #[test]
    fn test_checksum_filter_gitignore_excludes_source_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(
            root.join("Cargo.toml.orig"),
            b"[package]\nname = \"orig\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.toml"),
            b"[package]\nname = \"normalized\"\n",
        )
        .unwrap();

        let filter = gitignore_filter("vendor/*/Cargo.toml.orig");
        let pkgdir = std::path::Path::new("vendor/fb-procfs-0.9.0");

        let cksums = compute_dir_checksums_filtered(root, pkgdir, Some(&filter))
            .expect("checksums computed");

        assert!(
            !cksums.contains_key("Cargo.toml.orig"),
            "gitignore-matched Cargo.toml.orig should be excluded from checksum map"
        );
        assert!(
            cksums.contains_key("Cargo.toml"),
            "Cargo.toml should remain in checksum map"
        );
    }

    #[test]
    fn test_generate_vendor_config_uses_relative_vendor_dir() {
        let mut sources = BTreeSet::new();
        sources.insert(
            cargo::core::SourceId::from_url(
                "registry+https://github.com/rust-lang/crates.io-index",
            )
            .expect("valid registry URL"),
        );

        let config =
            generate_vendor_config(&sources, Path::new("/tmp/absolute/path/vendor")).unwrap();

        assert!(
            config.contains("directory = \"vendor\""),
            "config should write a relative vendor directory: {config}"
        );
        assert!(
            !config.contains("/tmp/absolute/path/vendor"),
            "config should not embed an absolute vendor path: {config}"
        );
    }

    #[test]
    fn test_generate_vendor_config_path_only_workspace() {
        // A workspace with only path dependencies has an empty sources set.
        // generate_vendor_config must still emit [source.vendored-sources] so
        // that is_vendored() returns true after fast_vendor() runs.
        let sources = BTreeSet::new();
        let vendor_dir = Path::new("/jellyfish-cove/vendor");
        let config = generate_vendor_config(&sources, vendor_dir).unwrap();
        assert!(
            config.contains("vendored-sources"),
            "config must contain vendored-sources even for path-only workspaces: {config}",
        );
    }

    // Invariant: vendor_this excludes .gitattributes, .gitignore, .git, .cargo-ok
    #[test]
    fn test_vendor_this_excludes_dotfiles() {
        assert!(!vendor_this(Path::new(".gitattributes")));
        assert!(!vendor_this(Path::new(".gitignore")));
        assert!(!vendor_this(Path::new(".git")));
        assert!(!vendor_this(Path::new(".cargo-ok")));
        assert!(!vendor_this(Path::new("nested/.gitattributes")));
        assert!(!vendor_this(Path::new("nested/.gitignore")));
        assert!(!vendor_this(Path::new("nested/.git/config")));
        assert!(!vendor_this(Path::new("nested/.cargo-ok")));
    }

    // Invariant: vendor_this includes normal source files and nested paths
    #[test]
    fn test_vendor_this_includes_normal_files() {
        assert!(vendor_this(Path::new("src/lib.rs")));
        assert!(vendor_this(Path::new("Cargo.toml")));
        assert!(vendor_this(Path::new("README.md")));
        assert!(vendor_this(Path::new("build.rs")));
        assert!(vendor_this(Path::new(".cargo/config.toml")));
    }

    // Invariant: generate_vendor_config emits git source replacement with branch/tag/rev fields
    #[test]
    fn test_generate_vendor_config_git_source() {
        let mut sources = BTreeSet::new();
        let sid =
            cargo::core::SourceId::from_url("git+https://github.com/example/crate.git?branch=main")
                .expect("valid git URL");
        sources.insert(sid);

        let config = generate_vendor_config(&sources, Path::new("/vendor")).unwrap();
        assert!(
            config.contains("git = \"https://github.com/example/crate.git\""),
            "config must contain git URL: {config}",
        );
        assert!(
            config.contains("branch = \"main\""),
            "config must contain branch: {config}",
        );
        assert!(
            config.contains("replace-with = \"vendored-sources\""),
            "config must redirect to vendored-sources: {config}",
        );
    }

    // Invariant: make_staging_destination creates a unique directory under the parent
    #[test]
    fn test_make_staging_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let dst = tmp.path().join("my-crate-1.0.0");

        let (staging_root, staging_dst) = make_staging_destination(&dst).unwrap();
        assert!(staging_root.exists());
        assert!(staging_root.starts_with(tmp.path()));
        assert_eq!(staging_dst.file_name().unwrap(), "my-crate-1.0.0");
        assert!(staging_dst.starts_with(&staging_root));

        std::fs::remove_dir_all(&staging_root).unwrap();
    }

    // Invariant: compute_dir_checksums_filtered without a filter produces SHA256 hashes for all
    // files in a tree.
    #[test]
    fn test_compute_dir_checksums() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), b"world").unwrap();

        let cksums =
            compute_dir_checksums_filtered(tmp.path(), Path::new("vendor/example-0.1.0"), None)
                .unwrap();
        assert_eq!(cksums.len(), 2);
        assert!(cksums.contains_key("a.txt"));
        assert!(cksums.contains_key("sub/b.txt"));

        let expected_a = format!("{:x}", sha2::Sha256::digest(b"hello"));
        assert_eq!(cksums["a.txt"], expected_a);
    }

    #[test]
    fn test_write_regular_file_replaces_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".cargo-config");
        fs::create_dir(&path).unwrap();

        super::write_regular_file(&path, b"[source]\n").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[source]\n",
            "write_regular_file should replace preexisting directories"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_regular_file_replaces_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        let path = dir.path().join(".cargo-config");
        fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();

        super::write_regular_file(&path, b"[source]\n").unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[source]\n",
            "write_regular_file should replace preexisting symlinks"
        );
        assert!(
            fs::symlink_metadata(&path).unwrap().file_type().is_file(),
            "repaired path should be a regular file"
        );
    }

    #[test]
    fn test_replace_vendor_dir_replaces_preexisting_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        fs::write(&dst, b"old").unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair preexisting files at the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_vendor_dir_replaces_preexisting_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        let target = dir.path().join("target");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair preexisting symlinks at the destination"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_replace_vendor_dir_replaces_dangling_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged_dst = dir.path().join("staged");
        let dst = dir.path().join("vendor-crate");
        let target = dir.path().join("missing-target");
        fs::create_dir(&staged_dst).unwrap();
        fs::write(staged_dst.join("lib.rs"), b"pub fn rise() {}\n").unwrap();
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        super::replace_vendor_dir(&staged_dst, &dst).unwrap();

        assert_eq!(
            fs::read_to_string(dst.join("lib.rs")).unwrap(),
            "pub fn rise() {}\n",
            "replace_vendor_dir should repair dangling symlinks at the destination"
        );
    }

    #[test]
    fn test_is_split_buck_file_matches_only_root_configured_name() {
        assert!(is_split_buck_file(Path::new("BUCK"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("src/BUCK"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("BUCK.v2"), Some("BUCK")));
        assert!(!is_split_buck_file(Path::new("BUCK"), None));
    }

    #[test]
    fn test_find_cached_registry_archive_uses_matching_crate_archive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cargo_home = dir.path();
        let cache_a = cargo_home.join("registry/cache/index-a");
        let cache_b = cargo_home.join("registry/cache/index-b");
        fs::create_dir_all(&cache_a).unwrap();
        fs::create_dir_all(&cache_b).unwrap();

        let tarball_name = "example-0.1.0.crate";
        fs::write(cache_a.join(tarball_name), b"wrong archive").unwrap();
        let archive = cache_b.join(tarball_name);
        fs::write(&archive, b"right archive").unwrap();
        let checksum = file_sha256(&archive).unwrap();

        assert_eq!(
            find_cached_registry_archive_by_tarball(cargo_home, tarball_name, &checksum).unwrap(),
            Some(archive),
            "the cache lookup should select the archive matching the lockfile checksum"
        );
    }

    #[test]
    fn test_find_cached_registry_archive_treats_checksum_mismatch_as_miss() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("registry/cache/index");
        fs::create_dir_all(&cache).unwrap();

        let tarball_name = "example-0.1.0.crate";
        fs::write(cache.join(tarball_name), b"wrong archive").unwrap();

        assert!(
            find_cached_registry_archive_by_tarball(dir.path(), tarball_name, "expected-checksum")
                .unwrap()
                .is_none(),
            "checksum mismatches should fall back to Cargo package loading"
        );
    }

    #[test]
    fn test_find_cached_registry_archive_returns_none_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("registry/cache/index")).unwrap();

        assert!(
            find_cached_registry_archive_by_tarball(dir.path(), "missing-0.1.0.crate", "checksum",)
                .unwrap()
                .is_none(),
            "missing cached archives should fall back to Cargo package loading"
        );
    }

    #[test]
    fn test_collect_vendor_cleanup_entries_removes_old_stamp_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        fs::write(vendor_dir.join(".reindeer-vendor-stamp"), b"{}").unwrap();
        fs::write(vendor_dir.join(".reindeer-vendor-stamps"), b"{}").unwrap();
        fs::write(vendor_dir.join(".junk"), b"crumbs").unwrap();
        fs::create_dir(vendor_dir.join(".reindeer-staging-123-0")).unwrap();
        fs::create_dir(vendor_dir.join("sourdough-1.0.0")).unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".junk"),
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join(".reindeer-vendor-stamps"),
            vendor_dir.join(".reindeer-staging-123-0"),
            vendor_dir.join("sourdough-1.0.0"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "repair cleanup should remove hidden junk, stale staging dirs, and old stamp files"
        );
    }

    #[test]
    fn test_collect_vendor_cleanup_entries_removes_invalid_bookkeeping_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        fs::create_dir(vendor_dir.join(".reindeer-vendor-stamp")).unwrap();
        fs::create_dir(vendor_dir.join(".reindeer-vendor-stamps")).unwrap();
        fs::write(vendor_dir.join(".junk"), b"crumbs").unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".junk"),
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join(".reindeer-vendor-stamps"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "old bookkeeping directories should be removed as stale entries"
        );
    }

    #[test]
    fn test_remove_expected_vendor_entries_from_cleanup_preserves_split_buck_package() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        let source_dir = vendor_dir.join("example-0.1.0");
        let split_buck_dir = vendor_dir.join("example");
        let stale_dir = vendor_dir.join("stale-0.1.0");
        let mut to_remove = BTreeSet::from([
            source_dir.clone(),
            split_buck_dir.clone(),
            stale_dir.clone(),
        ]);

        remove_expected_vendor_entries_from_cleanup(
            &mut to_remove,
            vendor_dir,
            &source_dir,
            "example",
            Some("BUCK"),
        );

        assert_eq!(
            to_remove,
            BTreeSet::from([stale_dir]),
            "current source and split BUCK package dirs should survive cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_vendor_cleanup_entries_removes_invalid_bookkeeping_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let vendor_dir = dir.path();
        let target = vendor_dir.join("outside");
        fs::write(&target, b"crumbs").unwrap();
        std::os::unix::fs::symlink(&target, vendor_dir.join(".reindeer-vendor-stamp")).unwrap();

        let expected = BTreeSet::from([
            vendor_dir.join(".reindeer-vendor-stamp"),
            vendor_dir.join("outside"),
        ]);
        assert_eq!(
            collect_vendor_cleanup_entries(vendor_dir).unwrap(),
            expected,
            "old bookkeeping symlinks should be treated as drift and removed"
        );
    }
}
