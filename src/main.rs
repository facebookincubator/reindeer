/*
 * Copyright (c) Facebook, Inc. and its affiliates.
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
    Buckify {},
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
}

fn try_main() -> Result<()> {
    let args = Args::from_args();

    let paths = {
        let tpd = args.third_party_dir.canonicalize()?;
        Paths {
            manifest_path: tpd.join("Cargo.toml"),
            third_party_dir: tpd,
        }
    };

    let config = config::read_config(&paths.third_party_dir)?;

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

        SubCommand::Buckify {} => buckify::buckify(&config, &args, &paths)?,
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
