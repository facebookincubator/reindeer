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

#[derive(Debug)]
pub struct UnusedFixups {
    // key = (pkg, byte offset) for nicely sorted output
    // value = (line, glob)
    pub globs: BTreeMap<(String, usize), (usize, Glob)>,
}

impl UnusedFixups {
    pub fn new() -> Self {
        UnusedFixups {
            globs: BTreeMap::new(),
        }
    }

    pub fn check(self) -> anyhow::Result<()> {
        if self.globs.is_empty() {
            Ok(())
        } else {
            Err(anyhow::Error::new(self))
        }
    }
}

impl Error for UnusedFixups {}

impl Display for UnusedFixups {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Unused globs:")?;
        for ((pkg, _offset), (line, glob)) in &self.globs {
            write!(
                formatter,
                "\nfixups/{pkg}/fixups.toml line {line}: {glob:?} matches no files",
                glob = glob.glob(),
            )?;
        }
        Ok(())
    }
}
