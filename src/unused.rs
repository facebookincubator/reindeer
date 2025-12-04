/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fmt::Display;

use globset::Glob;

use crate::config::Config;
use crate::config::VendorConfig;

#[derive(Debug)]
pub struct UnusedFixups {
    // key = (pkg, byte offset) for nicely sorted output
    // value = line
    pub buildscripts: BTreeMap<(String, usize), usize>,
    // key = (pkg, byte offset)
    // value = (line, glob)
    pub globs: BTreeMap<(String, usize), (usize, Glob)>,
    // key = (pkg, byte offset)
    // value = line
    pub visibilities: BTreeMap<(String, usize), usize>,
}

impl UnusedFixups {
    pub fn new() -> Self {
        UnusedFixups {
            buildscripts: BTreeMap::new(),
            globs: BTreeMap::new(),
            visibilities: BTreeMap::new(),
        }
    }

    pub fn check(mut self, config: &Config) -> anyhow::Result<()> {
        if !matches!(config.vendor, VendorConfig::Source(..)) {
            self.globs.clear();
        }
        let UnusedFixups {
            buildscripts,
            globs,
            visibilities,
        } = &self;
        if buildscripts.is_empty() && globs.is_empty() && visibilities.is_empty() {
            Ok(())
        } else {
            Err(anyhow::Error::new(self))
        }
    }
}

impl Error for UnusedFixups {}

impl Display for UnusedFixups {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        let mut needs_blank_line_between_sections = false;

        if !self.buildscripts.is_empty() {
            formatter.write_str("Unused buildscript fixups:")?;
            for ((pkg, _offset), line) in &self.buildscripts {
                write!(
                    formatter,
                    "\nfixups/{pkg}/fixups.toml line {line}: unused buildscript fixup for a package that has no build script",
                )?;
            }
            needs_blank_line_between_sections = true;
        }

        if !self.globs.is_empty() {
            if needs_blank_line_between_sections {
                formatter.write_str("\n\n")?;
            }
            formatter.write_str("Unused globs:")?;
            for ((pkg, _offset), (line, glob)) in &self.globs {
                write!(
                    formatter,
                    "\nfixups/{pkg}/fixups.toml line {line}: {glob:?} matches no files",
                    glob = glob.glob(),
                )?;
            }
            needs_blank_line_between_sections = true;
        }

        if !self.visibilities.is_empty() {
            if needs_blank_line_between_sections {
                formatter.write_str("\n\n")?;
            }
            formatter.write_str("Unused visibilities:")?;
            for ((pkg, _offset), line) in &self.visibilities {
                write!(
                    formatter,
                    "\nfixups/{pkg}/fixups.toml line {line}: only public packages can have a visibility fixup",
                )?;
            }
        }

        Ok(())
    }
}
