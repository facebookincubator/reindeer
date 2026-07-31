/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::ErrorKind;

use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;

use crate::Paths;
use crate::config::VendorSourceConfig;

pub(crate) fn load_gitignore(
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<Gitignore> {
    let mut gitignore = GitignoreBuilder::new(&paths.third_party_dir);

    for path in &source_config.gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(paths.third_party_dir.join(path)) {
            log::warn!("Failed to read ignore file {}: {}", path.display(), err);
        }
    }

    let mut parent_dirs = Vec::new();
    let mut up = paths.third_party_dir.as_path();
    for _ in 0..=paths.buck_package.0.components().count() {
        parent_dirs.push(up);
        match up.parent() {
            Some(next) => up = next,
            None => break,
        }
    }

    // Add outermost directories first to match git's precedence for `!` patterns.
    for parent_dir in parent_dirs.iter().rev() {
        if let Some(err) = gitignore.add(parent_dir.join(".gitignore"))
            && !is_not_found(&err)
        {
            log::warn!("Failed to read ignore file: {}", err);
        }
    }

    let gitignore = gitignore.build()?;

    log::debug!("gitignore {:#?}", gitignore);

    Ok(gitignore)
}

fn is_not_found(mut err: &ignore::Error) -> bool {
    loop {
        match err {
            ignore::Error::Io(err) => return err.kind() == ErrorKind::NotFound,
            ignore::Error::WithPath {
                path: _,
                err: inner,
            } => err = inner,
            _ => return false,
        }
    }
}
