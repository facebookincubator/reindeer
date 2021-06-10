/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Interface with Cargo
//!
//! This module defines invocations of `cargo` to either perform actions (vendor, update) or
//! get metadata about a crate. It also defines all the types for deserializing from Cargo's
//! JSON output.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fmt::{self, Display},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize};

use crate::{config::Config, platform::PlatformExpr, Args, Paths};

pub fn cargo_get_metadata(config: &Config, args: &Args, paths: &Paths) -> Result<Metadata> {
    let metadata: Metadata = self::run_cargo_json(
        config,
        &paths.third_party_dir,
        &args,
        &[
            "metadata",
            "--format-version",
            "1",
            "--frozen",
            "--locked",
            "--offline",
            "--manifest-path",
            paths.manifest_path.to_str().unwrap(),
        ],
    )
    .context("parsing metadata")?;

    Ok(metadata)
}

// Run a cargo command
pub(crate) fn run_cargo(
    config: &Config,
    cargo_home: impl AsRef<Path>,
    args: &Args,
    opts: &[&str],
) -> Result<Vec<u8>> {
    let cargo_home = cargo_home.as_ref();
    let cmdline: Vec<_> = args
        .cargo_options
        .iter()
        .map(String::as_str)
        .chain(opts.iter().cloned())
        .collect();
    let debug = args.debug;

    log::debug!(
        "Running Cargo command {:?} in {}",
        cmdline,
        cargo_home.display()
    );

    let cargo_path = args
        .cargo_path
        .clone()
        .or_else(|| {
            config
                .cargo
                .cargo
                .as_ref()
                .map(|bin| config.config_path.join(bin))
        })
        .unwrap_or_else(|| PathBuf::from("cargo"));

    let mut cargo_command = Command::new(&cargo_path);
    cargo_command
        .env("CARGO_HOME", cargo_home.join(".cargo"))
        .current_dir(cargo_home) // make sure it doesn't see any stray .cargo/config files
        .args(&cmdline)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Any relative RUSTC path in Reindeer's env is meant to be interpreted
    // relative to the directory Reindeer is running in, which is different from
    // the one we are about to invoke Cargo in. Patch up the RUSTC in the env.
    if let Some(rustc_path) = env::var_os("RUSTC") {
        if rustc_path.to_string_lossy().contains('/') {
            if let Ok(current_dir) = env::current_dir() {
                let rustc_path = current_dir.join(rustc_path);
                cargo_command.env("RUSTC", rustc_path);
            }
        }
    }

    let mut child = cargo_command
        .spawn()
        .with_context(|| format!("Failed to execute {}", cargo_path.display()))?;

    let stdout_thr = thread::spawn({
        let stdout = BufReader::new(child.stdout.take().unwrap());
        move || {
            let mut buf = String::new();
            for line in stdout.lines() {
                let line = line.expect("malformed stdout from cargo");
                if debug {
                    log::trace!("STDOUT: {}", line);
                }
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
        anyhow::bail!(
            "{} {} failed:\n{}",
            cargo_path.display(),
            cmdline.join(" "),
            stderr
        );
    }

    Ok(stdout.into_bytes())
}

// Run a cargo command, assuming it returns a json output of some form.
pub(crate) fn run_cargo_json<T: DeserializeOwned>(
    config: &Config,
    cargo_home: impl AsRef<Path>,
    args: &Args,
    opts: &[&str],
) -> Result<T> {
    let json = run_cargo(config, cargo_home, args, opts).context("running cargo")?;

    if args.debug {
        std::fs::write("dump.json", &json)?;
    }

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
    pub target_directory: PathBuf,
    pub version: u32,
    pub workspace_root: PathBuf,
    pub workspace_members: Vec<PkgId>,
    /// Resolved dependency graph
    pub resolve: Resolve,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum VecStringOrBool {
    VecString(Vec<String>),
    Bool(bool),
}

/// Package manifest
#[derive(Debug, Deserialize)]
pub struct Manifest {
    /// Package name
    pub name: String,
    /// Package version
    pub version: semver::Version,
    /// Canonical ID for package
    pub id: PkgId,
    /// License name
    pub license: Option<String>,
    /// Path to license
    pub license_file: Option<PathBuf>,
    /// Package description
    pub description: Option<String>,
    /// Source registry for package
    pub source: Option<String>,
    /// Package dependencies (unresolved)
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub dependencies: Vec<ManifestDep>,
    /// Targets in package
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub targets: BTreeSet<ManifestTarget>,
    /// Features - mapping from feature name to additional features or dependencies
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub features: BTreeMap<String, Vec<String>>,
    /// Path to Cargo.toml
    pub manifest_path: PathBuf,
    /// Additional arbitrary metadata
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// List of authors
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub authors: Vec<String>,
    /// List of categories in crates.io
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub categories: BTreeSet<String>,
    /// List of keywords in crates.io
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub keywords: BTreeSet<String>,
    /// Path to README file
    pub readme: Option<PathBuf>,
    /// Source repository
    pub repository: Option<String>,
    /// Default edition for the package (if targets don't have it)
    pub edition: Edition,
    /// Native library which should be linked to package(? All targets?)
    #[serde(deserialize_with = "deserialize_default_from_null")]
    pub links: Option<String>,
    pub publish: Option<VecStringOrBool>,
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
        self.id.partial_cmp(&other.id)
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
    /// Target platform for target-specific dependencies
    pub target: Option<PlatformExpr>,
    /// Registry this dependency is from
    pub registry: Option<String>,
}

/// Kind of dependency
#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DepKind {
    /// Normal dependency
    Normal,
    /// Dev dependency
    Dev,
    /// Build dependency
    Build,
}

impl Default for DepKind {
    fn default() -> Self {
        DepKind::Normal
    }
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

    #[allow(dead_code)]
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
    /// Bare package dependencies (without rename information)
    pub dependencies: BTreeSet<PkgId>,
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
    /// Local name for dependency (local name for crate)
    pub name: String,
    #[serde(default)]
    pub dep_kinds: Vec<NodeDepKind>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
pub struct NodeDepKind {
    #[serde(deserialize_with = "deserialize_default_from_null")]
    kind: DepKind,
    /// Platform config
    target: Option<PlatformExpr>,
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
    PartialOrd
)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Bench,
    Bin,
    CustomBuild,
    Example,
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
}

impl Display for Edition {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let edition = match self {
            Edition::Rust2015 => "2015",
            Edition::Rust2018 => "2018",
        };
        fmt.write_str(edition)
    }
}
