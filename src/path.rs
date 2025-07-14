/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

// normalize a/b/../c => a/c and a/./b => a/b
pub fn normalize_path(path: &Path) -> PathBuf {
    let max_len = path.as_os_str().len();
    let mut ret = PathBuf::with_capacity(max_len);

    normalized_extend_path(&mut ret, path);

    if ret.as_os_str().is_empty() {
        ret.push(Component::CurDir.as_os_str());
    }

    ret
}

pub fn normalized_extend_path(base: &mut PathBuf, relative: impl AsRef<Path>) {
    for c in relative.as_ref().components() {
        match c {
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => base.push(c),
            Component::ParentDir => match base.components().next_back() {
                Some(Component::Normal(_)) => {
                    base.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => unimplemented!(),
                Some(Component::CurDir) => unreachable!(),
                Some(Component::ParentDir) | None => base.push(Component::ParentDir),
            },
            Component::CurDir => {}
        };
    }
}

// Compute a path for `to` relative to `base`.
pub fn relative_path(mut base: &Path, to: &Path) -> PathBuf {
    let mut res = PathBuf::new();

    while !to.starts_with(base) {
        log::debug!(
            "relative_path: to={}, base={}, res={}",
            to.display(),
            base.display(),
            res.display()
        );
        res.push("..");
        base = base.parent().expect("root dir not prefix of other?");
    }

    res.join(
        to.strip_prefix(base)
            .expect("already worked out it was a prefix"),
    )
}
