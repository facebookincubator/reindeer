/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;

use crate::Paths;
use crate::config::VendorSourceConfig;

pub(crate) fn load_gitignore(
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<Gitignore> {
    let mut gitignore = GitignoreBuilder::new(&paths.third_party_dir);
    for ignore in &source_config.gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(paths.third_party_dir.join(ignore)) {
            log::warn!(
                "Failed to read ignore file {}: {}; skipping",
                ignore.display(),
                err
            );
        }
    }
    let gitignore = gitignore.build()?;

    log::debug!("gitignore {:#?}", gitignore);

    Ok(gitignore)
}
