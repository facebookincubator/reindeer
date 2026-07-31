/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use globset::Glob;
use globset::GlobSet;
use globset::GlobSetBuilder;
use ignore::gitignore::Gitignore;
use ignore::gitignore::GitignoreBuilder;

use crate::Paths;
use crate::config::Config;
use crate::config::VendorSourceConfig;

/// Filtering parameters passed into `fast_vendor`.
///
/// Controls which files are excluded from the vendor directory and checksums.
pub(crate) struct VendorFilter {
    pub remove_globs: GlobSet,
    pub gitignore: Gitignore,
}

pub(crate) fn build_filter(
    config: &Config,
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<VendorFilter> {
    do_build_filter(
        config.buck.split,
        &config.buck.file_name,
        &source_config.gitignore_checksum_exclude,
        &paths.third_party_dir,
    )
}

/// Build the checksum filter from primitive inputs.
///
/// Extracted from `build_filters` so the logic can be tested without
/// constructing `Config` or `Paths`.
fn do_build_filter(
    buck_split: bool,
    buck_file_name: &str,
    gitignore_checksum_exclude: &[PathBuf],
    third_party_dir: &Path,
) -> anyhow::Result<VendorFilter> {
    log::debug!(
        "vendor.gitignore_checksum_exclude = {:?}",
        gitignore_checksum_exclude,
    );

    let mut remove_globs = GlobSetBuilder::new();
    // Exclude the BUCK file from checksums only when split mode is enabled.
    // This keeps checksum exclusion aligned with on-disk exclusion: both are
    // gated on the same split-mode condition.
    if buck_split {
        let buck_glob = Glob::new(buck_file_name)
            .with_context(|| format!("Invalid buck.file_name glob `{}`", buck_file_name))?;
        remove_globs.add(buck_glob);
    }
    let remove_globs = remove_globs.build()?;

    let mut gitignore = GitignoreBuilder::new(third_party_dir);
    for ignore in gitignore_checksum_exclude {
        if let Some(err) = gitignore.add(third_party_dir.join(ignore)) {
            log::warn!(
                "Failed to read ignore file {}: {}; skipping",
                ignore.display(),
                err
            );
        }
    }
    let gitignore = gitignore.build()?;

    log::debug!(
        "remove_globs {:#?}, gitignore {:#?}",
        remove_globs,
        gitignore
    );

    Ok(VendorFilter {
        remove_globs,
        gitignore,
    })
}

#[cfg(test)]
mod tests {
    use crate::fast_vendor::filter::do_build_filter;

    #[test]
    fn test_build_filter_split_true_no_other_excludes() {
        // When split=true and no other excludes, the filter must still be built
        // and BUCK must be in the remove set (to align with on-disk exclusion).
        let dir = tempfile::tempdir().expect("tempdir");
        let filter =
            do_build_filter(true, "BUCK", &[], dir.path()).expect("do_build_filter should succeed");

        assert!(
            filter.remove_globs.is_match("BUCK"),
            "BUCK must be in checksum exclusion when split=true"
        );
    }

    #[test]
    fn test_build_filter_split_false_no_excludes() {
        // When split=false and no other excludes, no filter is needed.
        let dir = tempfile::tempdir().expect("tempdir");
        let filter = do_build_filter(false, "BUCK", &[], dir.path())
            .expect("do_build_filter should succeed");

        assert!(
            !filter.remove_globs.is_match("BUCK"),
            "BUCK must not be in checksum exclusion when split=false"
        );
    }

    #[test]
    fn test_build_filter_split_true_invalid_buck_glob_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = do_build_filter(true, "[", &[], dir.path());

        assert!(result.is_err(), "invalid buck.file_name glob should error");
        let err = result.err().expect("error should be present");

        assert!(
            format!("{err:#}").contains("Invalid buck.file_name glob `[`"),
            "error should point at invalid buck.file_name glob: {err:#}"
        );
    }
}
