/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io::ErrorKind;
use std::path::Path;

use ignore::Match;
use ignore::gitignore::Gitignore as Ignore;
use ignore::gitignore::GitignoreBuilder;

use crate::Paths;
use crate::config::VendorSourceConfig;
use crate::path::normalize_path;

/// Gitignore rules for the third-party tree.
///
/// Configured files have lowest precedence, followed by `.gitignore` files
/// from the Buck cell root through `third_party_dir`. Descendant files are not
/// discovered. Each matcher retains its file's root so anchored patterns keep
/// Git semantics.
#[derive(Debug, Default)]
pub(crate) struct Gitignore {
    matchers: Vec<Ignore>,
}

impl Gitignore {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Returns whether `path` is excluded, including exclusions on its parents.
    pub(crate) fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        debug_assert!(
            path.is_absolute(),
            "gitignore matching requires an absolute path, got {}",
            path.display(),
        );
        let mut ignored = false;
        for matcher in &self.matchers {
            // Some non-vendored manifests live outside the third-party tree.
            if !path.starts_with(matcher.path()) {
                continue;
            }
            match matcher.matched_path_or_any_parents(path, is_dir) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }
        ignored
    }
}

pub(crate) fn load_gitignore(
    paths: &Paths,
    source_config: &VendorSourceConfig,
) -> anyhow::Result<Gitignore> {
    let mut gitignore = Gitignore::empty();
    let third_party_dir = paths.third_party_dir.as_path();

    // Extra rule files are rooted at third_party_dir regardless of their location.
    for path in &source_config.gitignore_checksum_exclude {
        let ignore_file = normalize_path(&third_party_dir.join(path));
        push_matcher(&mut gitignore.matchers, third_party_dir, &ignore_file)?;
    }

    let mut ancestors = Vec::new();
    let mut up = third_party_dir;
    for _ in 0..=paths.buck_package.0.components().count() {
        ancestors.push(up);
        match up.parent() {
            Some(next) => up = next,
            None => break,
        }
    }

    for dir in ancestors.iter().rev() {
        push_matcher(&mut gitignore.matchers, dir, &dir.join(".gitignore"))?;
    }

    log::debug!("gitignore {:#?}", gitignore);

    Ok(gitignore)
}

fn push_matcher(matchers: &mut Vec<Ignore>, root: &Path, ignore_file: &Path) -> anyhow::Result<()> {
    let mut builder = GitignoreBuilder::new(root);
    if let Some(err) = builder.add(ignore_file) {
        if is_not_found(&err) {
            return Ok(());
        }
        // Parse errors are per-line; valid patterns from the file remain usable.
        log::warn!(
            "Failed to read ignore file {}: {}",
            ignore_file.display(),
            err
        );
    }

    let matcher = builder.build()?;
    if !matcher.is_empty() {
        matchers.push(matcher);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::buck::BuckPath;

    fn test_paths(third_party_dir: &Path, buck_package: PathBuf) -> Paths {
        Paths {
            buck_package: BuckPath(buck_package),
            third_party_dir: third_party_dir.to_path_buf(),
            manifest_path: PathBuf::new(),
            lockfile_path: PathBuf::new(),
            cargo_home: PathBuf::new(),
        }
    }

    fn load_at(third_party_dir: &Path) -> Gitignore {
        load_gitignore(
            &test_paths(third_party_dir, PathBuf::new()),
            &VendorSourceConfig::default(),
        )
        .expect("gitignore should load")
    }

    #[test]
    fn load_gitignore_preserves_roots_and_precedence() {
        let cell = tempfile::tempdir().expect("temp directory should be created");
        let third_party = cell.path().join("1.96.0");
        fs::create_dir_all(&third_party).expect("third-party directory should be created");
        fs::write(
            cell.path().join(".gitignore"),
            "/*/vendor/*/Cargo.lock\n/*/vendor/*/generated.rs\n",
        )
        .expect("ancestor gitignore should be written");
        fs::write(
            third_party.join(".gitignore"),
            "!/vendor/example/generated.rs\n",
        )
        .expect("nearer gitignore should be written");

        let filter = load_gitignore(
            &test_paths(&third_party, PathBuf::from("rust-toolchain")),
            &VendorSourceConfig::default(),
        )
        .expect("gitignore chain should load");
        let crate_dir = third_party.join("vendor/example");

        assert!(
            filter.is_ignored(&crate_dir.join("Cargo.lock"), false),
            "the ancestor's anchored rule should apply"
        );
        assert!(
            !filter.is_ignored(&crate_dir.join("generated.rs"), false),
            "the nearer rule should override the ancestor"
        );
    }

    #[test]
    fn load_gitignore_applies_configured_rules_before_tree_rules() {
        let third_party = tempfile::tempdir().expect("temp directory should be created");
        fs::write(third_party.path().join("configured-rules"), "*.orig\n")
            .expect("configured rules should be written");
        fs::write(third_party.path().join(".gitignore"), "!Cargo.toml.orig\n")
            .expect("gitignore should be written");

        let source_config = VendorSourceConfig {
            gitignore_checksum_exclude: vec![PathBuf::from("configured-rules")],
            ..VendorSourceConfig::default()
        };
        let filter = load_gitignore(
            &test_paths(third_party.path(), PathBuf::new()),
            &source_config,
        )
        .expect("gitignore should load");

        assert!(
            !filter.is_ignored(&third_party.path().join("Cargo.toml.orig"), false),
            "tree rules should override configured rules"
        );
        assert!(
            filter.is_ignored(&third_party.path().join("stale.orig"), false),
            "configured rules should remain active without an override"
        );
    }

    #[test]
    fn load_gitignore_does_not_discover_other_ignore_files() {
        let third_party = tempfile::tempdir().expect("temp directory should be created");
        let crate_dir = third_party.path().join("vendor/example");
        fs::create_dir_all(&crate_dir).expect("crate directory should be created");
        fs::write(third_party.path().join(".ignore"), "from-dot-ignore\n")
            .expect("dot-ignore file should be written");
        fs::write(crate_dir.join(".gitignore"), "generated.txt\n")
            .expect("descendant gitignore should be written");

        let filter = load_at(third_party.path());

        assert!(
            !filter.is_ignored(&third_party.path().join("from-dot-ignore"), false),
            ".ignore files should not be loaded"
        );
        assert!(
            !filter.is_ignored(&crate_dir.join("generated.txt"), false),
            "descendant .gitignore files should not be loaded"
        );
    }

    #[test]
    fn paths_outside_matcher_roots_are_not_ignored() {
        let third_party = tempfile::tempdir().expect("filter directory should be created");
        fs::write(third_party.path().join(".gitignore"), "Cargo.lock\n")
            .expect("gitignore should be written");
        let elsewhere = tempfile::tempdir().expect("outside directory should be created");

        let filter = load_at(third_party.path());

        assert!(
            !filter.is_ignored(&elsewhere.path().join("Cargo.lock"), false),
            "rules should not apply outside their matcher root"
        );
    }

    #[test]
    fn load_gitignore_retains_valid_rules_after_partial_error() {
        let third_party = tempfile::tempdir().expect("temp directory should be created");
        fs::write(third_party.path().join(".gitignore"), "ignored.txt\n[\n")
            .expect("gitignore should be written");

        let filter = load_gitignore(
            &test_paths(third_party.path(), PathBuf::new()),
            &VendorSourceConfig::default(),
        )
        .expect("valid rules should survive an invalid rule");

        assert!(
            filter.is_ignored(&third_party.path().join("ignored.txt"), false),
            "valid patterns should remain active after a partial parse error"
        );
    }
}
