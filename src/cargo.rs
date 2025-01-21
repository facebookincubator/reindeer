/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Interface with Cargo
//!
//! This module defines invocations of `cargo` to either perform actions (vendor, update) or
//! get metadata about a crate. It also defines all the types for deserializing from Cargo's
//! JSON output.

use std::collections::BTreeSet;
use std::env;
use std::fmt;
use std::fmt::Display;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::thread;

use anyhow::Context;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::Args;
use crate::Paths;
use crate::config::Config;
use crate::config::VendorConfig;
use crate::lockfile::Lockfile;
use crate::platform::PlatformExpr;

pub fn cargo_get_lockfile_and_metadata(
    config: &Config,
    args: &Args,
    paths: &Paths,
    features: String,
    default_features: bool,
) -> anyhow::Result<(Lockfile, Metadata)> {
    let mut cargo_flags = vec![
        "metadata",
        "--format-version",
        "1",
        "--manifest-path",
        paths.manifest_path.to_str().unwrap(),
        "--features",
        &features,
    ];

    // When requested, don't include the default features of the root crate
    // (which make up the selected feature set of the DEFAULT universe).
    if !default_features {
        cargo_flags.push("--no-default-features");
    }

    let cargo_home;
    let lockfile;
    if !matches!(config.vendor, VendorConfig::Source(_)) {
        cargo_home = None;

        // Whether or not there is a Cargo.lock already, do not read it yet.
        // Read it after the `cargo metadata` invocation. In non-vendoring mode
        // we allow `reindeer buckify` to make changes to the lockfile. In
        // vendoring mode `reindeer vendor` would have done the same changes.
        lockfile = None;
    } else {
        cargo_home = Some(paths.cargo_home.as_path());

        // The Cargo.lock should already have been updated by the vendor step.
        // We must not change it during buckify or else we'd be generating Buck
        // targets for not the same crate versions that were put in the vendor
        // directory.
        cargo_flags.extend(["--frozen", "--locked", "--offline"]);
        lockfile = Some(Lockfile::load(paths)?);
    };

    let metadata: Metadata =
        run_cargo_json(config, cargo_home, None, args, &cargo_flags).context("parsing metadata")?;

    let lockfile = match lockfile {
        Some(existing_lockfile) => existing_lockfile,
        None => Lockfile::load(paths)?,
    };

    Ok((lockfile, metadata))
}

// Run a cargo command
pub(crate) fn run_cargo(
    config: &Config,
    cargo_home: Option<&Path>,
    current_dir: Option<&Path>,
    args: &Args,
    opts: &[&str],
) -> anyhow::Result<Vec<u8>> {
    let mut cmdline: Vec<_> = args
        .cargo_options
        .iter()
        .map(String::as_str)
        .chain(opts.iter().cloned())
        .collect();
    let mut envs = Vec::new();
    if config.cargo.bindeps {
        cmdline.push("-Zbindeps");
        envs.push(("RUSTC_BOOTSTRAP", "1"));
    }

    log::debug!(
        "Running Cargo command {:?} in dir {:?}",
        cmdline,
        current_dir
    );

    let mut cargo_command = if let Some(cargo_path) = args.cargo_path.as_ref() {
        Command::new(cargo_path)
    } else if let Some(bin) = config.cargo.cargo.as_ref() {
        Command::new(config.config_dir.join(bin))
    } else {
        Command::new("cargo")
    };

    // Priority:
    // 1. `--rustc-path` arg.
    // 2. `RUSTC` env.
    // 3. `reindeer.toml` config value.
    if let Some(rustc_path) = args.rustc_path.as_ref() {
        cargo_command.env("RUSTC", rustc_path);
    } else if let Some(rustc_path) = env::var_os("RUSTC") {
        // Any relative RUSTC path in Reindeer's env is meant to be interpreted
        // relative to the directory Reindeer is running in, which is different
        // from the one we are about to invoke Cargo in. Patch up the RUSTC
        // in the env.
        if Path::new(&rustc_path).components().nth(1).is_some() {
            if let Ok(current_dir) = env::current_dir() {
                let rustc_path = current_dir.join(rustc_path);
                cargo_command.env("RUSTC", rustc_path);
            }
        }
    } else if let Some(bin) = config.cargo.rustc.as_ref() {
        cargo_command.env("RUSTC", config.config_dir.join(bin));
    }

    if let Some(cargo_home) = cargo_home {
        cargo_command.env("CARGO_HOME", cargo_home);
    }

    if let Some(current_dir) = current_dir {
        cargo_command.current_dir(current_dir);
    }

    cargo_command
        .args(&cmdline)
        .envs(envs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cargo_command
        .spawn()
        .with_context(|| format!("Failed to execute `{:?}`", cargo_command))?;

    let stdout_thr = thread::spawn({
        let stdout = BufReader::new(child.stdout.take().unwrap());
        move || {
            let mut buf = String::new();
            for line in stdout.lines() {
                let line = line.expect("malformed stdout from cargo");
                log::trace!("STDOUT: {}", line);
                buf += &line;
                buf += "\n";
            }
            buf
        }
    });
    let stderr_thr = thread::spawn({
        let stderr = BufReader::new(child.stderr.take().unwrap());
        move || {
            let mut buf = String::new();
            for line in stderr.lines() {
                let line = line.expect("malformed stderr from cargo");
                log::trace!("cargo: {}", line);
                buf += &line;
                buf += "\n";
            }
            buf
        }
    });

    let stdout = stdout_thr.join().expect("stdout thread join failed");
    let stderr = stderr_thr.join().expect("stderr thread join failed");

    if !child.wait()?.success() {
        anyhow::bail!("`{:?}` failed:\n{}", cargo_command, stderr);
    }

    Ok(stdout.into_bytes())
}

// Run a cargo command, assuming it returns a json output of some form.
pub(crate) fn run_cargo_json<T: DeserializeOwned>(
    config: &Config,
    cargo_home: Option<&Path>,
    current_dir: Option<&Path>,
    args: &Args,
    opts: &[&str],
) -> anyhow::Result<T> {
    let json = run_cargo(config, cargo_home, current_dir, args, opts).context("running cargo")?;

    let res = serde_json::from_slice::<T>(&json).context("deserializing json")?;

    Ok(res)
}

fn deserialize_default_from_null<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct PkgId(pub String);

impl Display for PkgId {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        String::fmt(&self.0, fmt)
    }
}

/// Top-level structure from `cargo metadata`
#[derive(Debug, Deserialize, PartialEq)]
pub struct Metadata {
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub packages: BTreeSet<Manifest>,
    pub version: u32,
    pub workspace_default_members: Vec<PkgId>,
    /// Resolved dependency graph
    pub resolve: Resolve,
}

/// Package manifest
// https://doc.rust-lang.org/cargo/reference/manifest.html#the-package-section
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Package name
    pub name: String,
    /// Package version
    pub version: semver::Version,
    /// Canonical ID for package
    pub id: PkgId,
    /// Path to license
    pub license_file: Option<PathBuf>,
    /// Package description
    pub description: Option<String>,
    /// Source registry for package
    pub source: Source,
    /// Package dependencies (unresolved)
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub dependencies: Vec<ManifestDep>,
    /// Targets in package
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub targets: BTreeSet<ManifestTarget>,
    /// Path to Cargo.toml
    pub manifest_path: PathBuf,
    /// List of authors
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub authors: Vec<String>,
    /// Source repository
    pub repository: Option<String>,
    /// Default edition for the package (if targets don't have it)
    pub edition: Edition,
}

impl Manifest {
    /// Find the target that other packages can depend on. There's supposed to be at most one.
    pub fn dependency_target(&self) -> Option<&ManifestTarget> {
        self.targets
            .iter()
            .find(|tgt| tgt.kind_lib() || tgt.kind_proc_macro())
    }

    /// Return full path to manifest dir (ie, top of package)
    pub fn manifest_dir(&self) -> &Path {
        self.manifest_path.parent().unwrap()
    }
}

impl Eq for Manifest {}
impl PartialEq for Manifest {
    fn eq(&self, other: &Manifest) -> bool {
        self.id.eq(&other.id)
    }
}

impl Ord for Manifest {
    fn cmp(&self, other: &Manifest) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for Manifest {
    fn partial_cmp(&self, other: &Manifest) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Display for Manifest {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "{}-{}", self.name, self.version)
    }
}

/// Package dependency (unresolved)
#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ManifestDep {
    /// Dependency name
    pub name: String,
    /// The source ID of the dependency. May be null, see description for the package source
    pub source: Option<String>,
    /// Dependency version requirement
    pub req: String,
    /// If renamed, local name for dependency
    pub rename: Option<String>,
    /// Dependency kind ("dev", "build" or "normal")
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: DepKind,
    /// Whether dependency is optional or not
    pub optional: bool,
    /// Whether or not to use default features
    pub uses_default_features: bool,
    /// Set of (additional) features
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub features: BTreeSet<String>,
    /// An "artifact dependency" for depending on a binary instead of (or in
    /// addition to) a package's library crate
    pub artifact: Option<ArtifactDep>,
    /// Target platform for target-specific dependencies
    pub target: Option<PlatformExpr>,
    /// Registry this dependency is from
    pub registry: Option<String>,
}

/// Kind of dependency
#[derive(Debug, Default, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DepKind {
    /// Normal dependency
    #[default]
    Normal,
    /// Dev dependency
    Dev,
    /// Build dependency
    Build,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ArtifactDep {
    kinds: Vec<ArtifactKind>,
    lib: bool,
    target: Option<String>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Bin,
    Staticlib,
    Cdylib,
}

/// Package build target
#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct ManifestTarget {
    /// Name of target (crate name)
    pub name: String,
    /// Kind of build(s?) target is for
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: BTreeSet<TargetKind>,
    /// Crate types generate by target
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub crate_types: BTreeSet<CrateType>,
    /// Absolute path to root source file of target
    pub src_path: PathBuf,
    /// Edition, defaulting to package edition
    pub edition: Option<Edition>,
    /// Required features
    #[serde(deserialize_with = "deserialize_default_from_null")]
    #[serde(rename = "required-features")]
    #[serde(default)]
    pub required_features: BTreeSet<String>,
    /// Target has doctests that `cargo test` should extract and build/run
    pub doctest: bool,
}

impl ManifestTarget {
    pub fn kind(&self) -> &TargetKind {
        self.kind.iter().next().expect("need at least one kind")
    }

    pub fn kind_bench(&self) -> bool {
        self.kind.contains(&TargetKind::Bench)
    }

    pub fn kind_bin(&self) -> bool {
        self.kind.contains(&TargetKind::Bin)
    }

    pub fn kind_rlib(&self) -> bool {
        self.kind.contains(&TargetKind::Rlib)
    }

    pub fn kind_lib(&self) -> bool {
        self.kind_rlib() || self.kind.contains(&TargetKind::Lib)
    }

    pub fn kind_proc_macro(&self) -> bool {
        self.kind.contains(&TargetKind::ProcMacro)
    }

    pub fn kind_custom_build(&self) -> bool {
        self.kind.contains(&TargetKind::CustomBuild)
    }

    pub fn kind_test(&self) -> bool {
        self.kind.contains(&TargetKind::Test)
    }

    pub fn kind_example(&self) -> bool {
        self.kind.contains(&TargetKind::Example)
    }

    pub fn kind_staticlib(&self) -> bool {
        self.kind.contains(&TargetKind::Staticlib)
    }

    pub fn kind_cdylib(&self) -> bool {
        self.kind.contains(&TargetKind::Cdylib)
    }

    #[allow(dead_code)]
    pub fn kind_native_lib(&self) -> bool {
        self.kind_staticlib() || self.kind_cdylib()
    }

    pub fn crate_rlib(&self) -> bool {
        self.crate_types.contains(&CrateType::Rlib)
    }

    pub fn crate_dylib(&self) -> bool {
        self.crate_types.contains(&CrateType::Dylib)
    }

    pub fn crate_staticlib(&self) -> bool {
        self.crate_types.contains(&CrateType::Staticlib)
    }

    pub fn crate_cdylib(&self) -> bool {
        self.crate_types.contains(&CrateType::Cdylib)
    }

    pub fn crate_lib(&self) -> bool {
        self.crate_rlib() || self.crate_dylib() || self.crate_types.contains(&CrateType::Lib)
    }

    pub fn crate_bin(&self) -> bool {
        self.crate_types.contains(&CrateType::Bin)
    }

    pub fn crate_proc_macro(&self) -> bool {
        self.crate_types.contains(&CrateType::ProcMacro)
    }
}

/// Resolved dependencies
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub struct Resolve {
    /// Root package of the workspace
    pub root: Option<PkgId>,
    pub nodes: Vec<Node>,
}

/// Resolved dependencies for a particular package
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub struct Node {
    /// Package
    pub id: PkgId,
    /// Dependencies with rename information
    pub deps: BTreeSet<NodeDep>,
    /// Features selected for package
    pub features: BTreeSet<String>,
}

/// Resolved dependencies with rename information
#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct NodeDep {
    /// Package id for dependency
    pub pkg: PkgId,
    /// Local manifest's name for the dependency. Deprecated -- if using `-Z
    /// bindeps`, use the `extern_name` from `NodeDepKind` instead of this.
    #[serde(deserialize_with = "deserialize_empty_string_as_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub dep_kinds: Vec<NodeDepKind>,
}

fn deserialize_empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let string = String::deserialize(deserializer)?;

    // Cargo metadata contains "name":"" for artifact dependencies.
    if string.is_empty() {
        Ok(None)
    } else {
        Ok(Some(string))
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct NodeDepKind {
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub kind: DepKind,
    /// Platform config
    pub target: Option<PlatformExpr>,

    // vvvvv The fields below are introduced for `-Z bindeps`.
    /// Artifact's crate type, e.g. staticlib, cdylib, bin...
    pub artifact: Option<ArtifactKind>,
    /// What the manifest calls the crate.
    pub extern_name: Option<String>,
    /// Equivalent to `{ target = "..." }` in an artifact dependency requirement.
    pub compile_target: Option<String>,
    /// Executable name for an artifact binary dependency.
    pub bin_name: Option<String>,
    // ^^^^^ The fields above are introduced for `-Z bindeps`.
}

impl NodeDepKind {
    // The dep_kind assumed for `extra_deps` fixups.
    pub const ORDINARY: Self = NodeDepKind {
        kind: DepKind::Normal,
        target: None,
        artifact: None,
        extern_name: None,
        compile_target: None,
        bin_name: None,
    };

    pub fn target_req(&self) -> TargetReq {
        match self.artifact {
            None => TargetReq::Lib,
            Some(ArtifactKind::Bin) => {
                if let Some(bin_name) = &self.bin_name {
                    TargetReq::Bin(bin_name)
                } else {
                    panic!("missing bin_name for artifact dependency {self:?}");
                }
            }
            Some(ArtifactKind::Staticlib) => TargetReq::Staticlib,
            Some(ArtifactKind::Cdylib) => TargetReq::Cdylib,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TargetReq<'a> {
    Lib,
    Bin(&'a str),
    EveryBin,
    BuildScript,
    Staticlib,
    Cdylib,
    Sources,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum CompileMode {
    Build,
    RunCustomBuild,
    Test,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
#[serde(rename_all = "kebab-case")]
pub enum CrateType {
    Bin,
    Dylib,
    Lib,
    ProcMacro,
    Rlib,
    Staticlib,
    Cdylib,
}

#[derive(
    Copy,
    Clone,
    Debug,
    Serialize,
    Deserialize,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash
)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Bench,
    Bin,
    CustomBuild,
    Example,
    Dylib,
    Lib,
    Rlib,
    ProcMacro,
    Test,
    Staticlib,
    Cdylib,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
pub enum BuildKind {
    Host,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Deserialize,
    Serialize,
    Eq,
    PartialEq,
    Ord,
    PartialOrd
)]
pub enum Edition {
    #[serde(rename = "2015")]
    Rust2015,
    #[serde(rename = "2018")]
    Rust2018,
    #[serde(rename = "2021")]
    Rust2021,
}

impl Display for Edition {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let edition = match self {
            Edition::Rust2015 => "2015",
            Edition::Rust2018 => "2018",
            Edition::Rust2021 => "2021",
        };
        fmt.write_str(edition)
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Source {
    Local,
    CratesIo,
    Git { repo: String, commit_hash: String },
    Unrecognized(String),
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let source: Option<String> = Deserialize::deserialize(deserializer)?;
        Ok(match source {
            None => Source::Local,
            Some(source) => parse_source(&source).unwrap_or(Source::Unrecognized(source)),
        })
    }
}

fn parse_source(source: &str) -> Option<Source> {
    if source == "registry+https://github.com/rust-lang/crates.io-index" {
        Some(Source::CratesIo)
    } else if let Some(rest) = source.strip_prefix("git+") {
        // Git sources look like:
        //   git+https://github.com/owner/repo.git?branch=patchv1#9f8e7d6c5b4a3210
        // The #commithash is always present; the ?urlparam is optional.
        let (address, commit_hash) = rest.split_once('#')?;
        if let Some((repo, _reference)) = address.split_once('?') {
            Some(Source::Git {
                repo: repo.to_owned(),
                commit_hash: commit_hash.to_owned(),
            })
        } else {
            Some(Source::Git {
                repo: address.to_owned(),
                commit_hash: commit_hash.to_owned(),
            })
        }
    } else {
        None
    }
}

#[cfg(test)]
mod test {
    use super::Source;
    use super::parse_source;

    #[test]
    fn test_parses_source_git() {
        assert_eq!(
            parse_source(
                "git+https://github.com/facebookincubator/reindeer?rev=abcdef123#abcdef1234567890abcdef1234567890abcdef00",
            ),
            Some(Source::Git {
                repo: "https://github.com/facebookincubator/reindeer".to_owned(),
                commit_hash: "abcdef1234567890abcdef1234567890abcdef00".to_owned(),
            }),
        );
    }
}
