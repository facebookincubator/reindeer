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

use std::path::PathBuf;

use anyhow::Result;
use structopt::StructOpt;

mod audit_sec;
mod buck;
mod buckify;
mod cargo;
mod collection;
mod config;
mod fixups;
mod index;
mod platform;
mod tp_metadata;
mod vendor;

#[derive(Debug, StructOpt)]
pub struct Args {
    /// Enable debug output
    #[structopt(long, short = "D")]
    debug: bool,
    /// Path to `cargo` command
    #[structopt(long)]
    cargo_path: Option<PathBuf>,
    /// Extra cargo options
    #[structopt(long)]
    cargo_options: Vec<String>,
    /// Path to third-party dir
    #[structopt(long, default_value = ".")]
    third_party_dir: PathBuf,
    #[structopt(subcommand)]
    subcommand: SubCommand,
}

#[derive(Debug, StructOpt)]
enum SubCommand {
    /// Update Cargo.lock with new dependencies
    Update {},
    /// Vendor crate needed for build
    Vendor {
        /// Don't delete older crates in the vendor directory
        #[structopt(long)]
        no_delete: bool,
        /// Show reported security problems for crates as they're being vendored
        #[structopt(long)]
        audit_sec: bool,
        /// Use cached version of the advisory repo
        #[structopt(long)]
        no_fetch: bool,
    },
    /// Generate Buck build rules for Cargo packages
    Buckify {
        /// Emit generated build rules to stdout, not overwriting existing file.
        ///
        /// Suppresses generation of other output files.
        #[structopt(long)]
        stdout: bool,
    },
    /// Show security report for vendored crates
    Auditsec {
        /// Use cached version of the advisory repo
        #[structopt(long, short = "n")]
        no_fetch: bool,
        /// Attempt to fix problems by updating crates (often not very well)
        #[structopt(long)]
        autofix: bool,
    },
}

/// Computed paths
#[derive(Debug)]
pub struct Paths {
    third_party_dir: PathBuf,
    manifest_path: PathBuf,
    /// Path of the Buck cell root
    cell_dir: PathBuf,
}

fn try_main() -> Result<()> {
    let args = Args::from_args();

    let third_party_dir = args.third_party_dir.canonicalize()?;
    let config = config::read_config(&third_party_dir)?;

    let paths = {
        let mut cell_dir = third_party_dir.clone();
        if let Some(x) = &config.buck_cell_root {
            cell_dir = cell_dir.join(x).canonicalize()?;
        }
        Paths {
            manifest_path: third_party_dir.join("Cargo.toml"),
            third_party_dir,
            cell_dir,
        }
    };

    log::debug!("Args = {:#?}, paths {:#?}", args, paths);

    match &args.subcommand {
        SubCommand::Vendor {
            no_delete,
            audit_sec,
            no_fetch,
        } => {
            vendor::cargo_vendor(&config, *no_delete, *audit_sec, *no_fetch, &args, &paths)?;
        }

        SubCommand::Auditsec { no_fetch, autofix } => {
            audit_sec::audit_sec(&config, &paths, *no_fetch, *autofix)?;
        }

        SubCommand::Update { .. } => {
            let _ = cargo::run_cargo(
                &config,
                &paths.third_party_dir,
                &args,
                &[
                    "generate-lockfile",
                    "--manifest-path",
                    paths.manifest_path.to_str().unwrap(),
                ],
            )?;
        }

        SubCommand::Buckify { stdout } => buckify::buckify(&config, &args, &paths, *stdout)?,
    }

    Ok(())
}

fn main() {
    env_logger::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .init();

    if let Err(err) = try_main() {
        log::error!("{:?}", err);
        std::process::exit(1);
    }
}
