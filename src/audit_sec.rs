/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use anyhow::Context;
use anyhow::Result;

use crate::config::Config;
use crate::Paths;
use rustsec::advisory::Informational;
use rustsec::lockfile::Lockfile;
use rustsec::report::Report;
use rustsec::report::Settings;
use rustsec::warning::Kind;
use rustsec::warning::Warning;
use rustsec::Database;
use rustsec::Fixer;
use rustsec::Repository;
use std::io::Write;
use termcolor::Color;
use termcolor::ColorChoice;
use termcolor::ColorSpec;
use termcolor::StandardStream;
use termcolor::WriteColor;

/// Check crates for known security problems. Requires an existing Cargo.lock.
pub fn audit_sec(config: &Config, paths: &Paths, no_fetch: bool, autofix: bool) -> Result<()> {
    let stdout = &mut StandardStream::stdout(ColorChoice::Auto);
    let default = ColorSpec::new();
    let mut red = ColorSpec::new();
    red.set_fg(Some(Color::Red));
    let mut yellow = ColorSpec::new();
    yellow.set_fg(Some(Color::Yellow));
    let mut bold = ColorSpec::new();
    bold.set_bold(true);

    let cargo_toml = paths.third_party_dir.join("Cargo.toml");
    let cargo_lock = paths.third_party_dir.join("Cargo.lock");

    let lockfile = Lockfile::load(&cargo_lock)
        .with_context(|| format!("loading lockfile {}", cargo_lock.display()))?;
    let db = if no_fetch {
        Database::open(&Repository::default_path()).context("opening repository")?
    } else {
        Database::fetch().context("fetching database")?
    };

    let settings = Settings {
        informational_warnings: vec![Informational::Notice, Informational::Unmaintained],
        ..Settings::default()
    };
    let report = Report::generate(&db, &lockfile, &settings);

    let mut fixer = Fixer::new(&cargo_toml).context("initializing fixer")?;

    for v in &report.vulnerabilities.list {
        let adv = &v.advisory;
        let pkg = &v.package;
        let _ = || -> Result<_> {
            stdout.set_color(&red)?;
            writeln!(
                stdout,
                "VULNERABILITY {} - {}: {}",
                adv.id,
                adv.date.as_str(),
                adv.title
            )?;
            stdout.set_color(&default)?;
            writeln!(stdout, "Package: {} {}", pkg.name, pkg.version)?;
            writeln!(stdout, "\n{}", adv.description)?;
            Ok(())
        }();

        if !config.audit.never_autofix.contains(v.package.name.as_str()) {
            if let Err(err) = fixer.fix(v, !autofix) {
                let _ = || -> Result<_> {
                    stdout.set_color(&red)?;
                    writeln!(
                        stdout,
                        "Failed to find fix for {} ({} {}): {}\n",
                        adv.id, pkg.name, pkg.version, err
                    )?;
                    Ok(())
                }();
            }
        }

        let _ = writeln!(stdout, "\n");
    }

    for warning in report.warnings.values().flatten() {
        let pkg = &warning.package;
        let adv = match &warning {
            Warning {
                kind: Kind::Notice,
                advisory: Some(advisory),
                ..
            } => Some((advisory, "WARNING")),
            Warning {
                kind: Kind::Unmaintained,
                advisory: Some(advisory),
                ..
            } => Some((advisory, "UNMAINTAINED")),
            _ => None,
        };

        if let Some((adv, msg)) = adv {
            let _ = || -> Result<_> {
                stdout.set_color(&yellow)?;
                writeln!(
                    stdout,
                    "{msg} {id} - {date}: {title}",
                    msg = msg,
                    id = adv.id,
                    date = adv.date.as_str(),
                    title = adv.title
                )?;
                stdout.set_color(&default)?;
                writeln!(stdout, "Package: {} {}", pkg.name, pkg.version)?;
                writeln!(stdout, "\n{}", adv.description)?;
                Ok(())
            }();
        } else {
            let _ = || -> Result<_> {
                stdout.set_color(&yellow)?;
                writeln!(stdout, "Yanked Package: {} {}", pkg.name, pkg.version)?;
                Ok(())
            }();
        }
    }

    let _ = || -> Result<_> {
        stdout.set_color(&bold)?;
        writeln!(
            stdout,
            "{} vulnerabilities, {} warnings in {} packages",
            report.vulnerabilities.list.len(),
            report.warnings.len(),
            lockfile.packages.len()
        )?;
        stdout.set_color(&default)?;
        Ok(())
    }();

    Ok(())
}
