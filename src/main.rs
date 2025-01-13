/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! # Manage Rust third-party crates
//!
//! This tool takes a specification of third-party packages to be exported to fbsource,
//! and updates the Buck build files for them.
//!
//! ## Directory layout
//!
//! This works in a directory with the following layout:
//!
//! - Cargo.toml - specification of crates
//! - Cargo.lock - locked version
//! - vendor/ - vendored sources
//!
//! (TBD - rest of it)

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use clap::Subcommand;

use crate::config::VendorConfig;

mod audit_sec;
mod buck;
mod buckify;
mod cargo;
mod cfg;
mod collection;
mod config;
mod fixups;
mod glob;
mod index;
mod lockfile;
mod platform;
mod remap;
mod srcfiles;
mod universe;
mod vendor;

#[derive(Debug, Parser)]
#[command(bin_name = "reindeer")]
pub struct Args {
    /// Path to `cargo` command
    #[arg(long, value_name = "PATH")]
    cargo_path: Option<PathBuf>,
    /// Path to `rustc` command
    #[arg(long, value_name = "PATH")]
    rustc_path: Option<PathBuf>,
    /// Extra cargo options
    #[arg(long, value_name = "ARGUMENT")]
    cargo_options: Vec<String>,
    /// Path to third-party dir. Overrides configuration from `reindeer.toml`.
    ///
    /// Path is relative to current dir, whereas the one in `reindeer.toml` is relative to the
    /// parent directory of the `reindeer.toml` file.
    #[arg(long, value_name = "PATH")]
    third_party_dir: Option<PathBuf>,
    #[command(subcommand)]
    subcommand: SubCommand,
    /// Path to the `Cargo.toml to generate from. Overrides configuration from `reindeer.toml`.
    ///
    /// Path is relative to current dir, whereas the one in `reindeer.toml` is relative to the
    /// parent directory of the `reindeer.toml` file.
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,
    /// Path to a `reindeer.toml` file to read configuration from.
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum SubCommand {
    /// Update Cargo.lock with new dependencies
    Update {},
    /// Vendor crate needed for build
    Vendor {
        /// Don't delete older crates in the vendor directory
        #[arg(long)]
        no_delete: bool,
        /// Show reported security problems for crates as they're being vendored
        #[arg(long)]
        audit_sec: bool,
        /// Use cached version of the advisory repo
        #[arg(long)]
        no_fetch: bool,
    },
    /// Generate Buck build rules for Cargo packages
    Buckify {
        /// Emit generated build rules to stdout, not overwriting existing file.
        ///
        /// Suppresses generation of other output files.
        #[arg(long)]
        stdout: bool,
    },
    /// Show security report for vendored crates
    Auditsec {
        /// Use cached version of the advisory repo
        #[arg(long, short = 'n')]
        no_fetch: bool,
    },
}

/// Computed paths
#[derive(Debug)]
pub struct Paths {
    third_party_dir: PathBuf,
    manifest_path: PathBuf,
    lockfile_path: PathBuf,
    cargo_home: PathBuf,
}

fn try_main() -> anyhow::Result<()> {
    let args = Args::parse();

    let paths;
    let mut config;

    if let Some(from_args) = args.third_party_dir.as_deref() {
        let third_party_dir = dunce::canonicalize(&from_args)?;
        config = config::read_config(&third_party_dir.join("reindeer.toml"))?;

        let manifest_path = args
            .manifest_path
            .clone()
            .unwrap_or_else(|| third_party_dir.join("Cargo.toml"));

        paths = Paths {
            lockfile_path: manifest_path.with_file_name("Cargo.lock"),
            manifest_path,
            cargo_home: third_party_dir.join(".cargo"),
            third_party_dir,
        };
    } else {
        let config_path = args.config.as_deref().unwrap_or(Path::new("reindeer.toml"));
        config = config::read_config(config_path)?;

        // These unwrap sequences look gnarly, but essentially
        // - `reindeer` (no flags) == `reindeer -c reindeer.toml`
        // - config.third_party_dir and config.manifest_path are new, and many projects
        //   don't have them
        // - third_party_dir defaulted to . before, and remains so
        // - manifest_path was not configurable before, so absent configuration remains
        //   {third_party_dir}/Cargo.toml
        let third_party_dir = args
            .third_party_dir
            .clone()
            .or_else(|| Some(config.config_dir.join(config.third_party_dir.as_deref()?)))
            .unwrap_or_else(|| ".".into());
        let third_party_dir = dunce::canonicalize(&third_party_dir)?;

        let manifest_path = args
            .manifest_path
            .clone()
            .or_else(|| Some(config.config_dir.join(config.manifest_path.as_deref()?)))
            .unwrap_or_else(|| third_party_dir.join("Cargo.toml"));

        let lockfile_path = manifest_path.with_file_name("Cargo.lock");

        paths = Paths {
            lockfile_path,
            manifest_path,
            cargo_home: third_party_dir.join(".cargo"),
            third_party_dir,
        };
    }

    if !paths.manifest_path.is_file() {
        return Err(anyhow::anyhow!(
            "Path {} is not a file",
            paths.manifest_path.display()
        ));
    }

    if !paths.third_party_dir.exists() {
        std::fs::create_dir_all(&paths.third_party_dir)
            .with_context(|| format!("Creating directory {}", paths.third_party_dir.display()))?;
    } else if !paths.third_party_dir.is_dir() {
        return Err(anyhow::anyhow!(
            "Path {} must be a directory",
            paths.manifest_path.display()
        ));
    }

    log::debug!("Args = {:#?}, paths {:#?}", args, paths);

    match &args.subcommand {
        SubCommand::Vendor {
            no_delete,
            audit_sec,
            no_fetch,
        } => {
            vendor::cargo_vendor(&config, *no_delete, *audit_sec, *no_fetch, &args, &paths)?;
        }

        SubCommand::Auditsec { no_fetch } => {
            audit_sec::audit_sec(&paths, *no_fetch)?;
        }

        SubCommand::Update { .. } => {
            let _ = cargo::run_cargo(&config, Some(&paths.cargo_home), None, &args, &[
                "generate-lockfile",
                "--manifest-path",
                paths.manifest_path.to_str().unwrap(),
            ])?;
        }

        SubCommand::Buckify { stdout } => {
            if matches!(
                config.vendor,
                VendorConfig::LocalRegistry | VendorConfig::Source(_)
            ) && !vendor::is_vendored(&config, &paths)?
            {
                // If you ran `reindeer buckify` without `reindeer vendor`, then
                // default to generating non-vendored targets.
                config.vendor = VendorConfig::Off;
            }
            buckify::buckify(&config, &args, &paths, *stdout)?;
        }
    }

    Ok(())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .init();

    if let Err(err) = try_main() {
        log::error!("{:?}", err);
        std::process::exit(1);
    }
}
