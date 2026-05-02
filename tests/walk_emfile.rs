/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Verifies `Globs::walk` surfaces EMFILE rather than silently dropping entries.
//!
//! Lives as an integration test because it lowers RLIMIT_NOFILE process-wide
//! and exhausts file descriptors. Cargo gives each integration-test file its
//! own binary, so this test runs in a process separate from the unit tests
//! in `src/`.

use std::fs::File;

use reindeer::glob::GlobSetKind;
use reindeer::glob::Globs;
use reindeer::glob::NO_EXCLUDE;

#[test]
fn walk_propagates_emfile_under_low_rlimit() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let deep = tmp.path().join("a").join("b").join("c").join("d").join("e");
    std::fs::create_dir_all(&deep).expect("create_dir_all");
    std::fs::write(deep.join("file.rs"), b"").expect("write file");

    let (_, hard) = rlimit::Resource::NOFILE.get().expect("get nofile");
    rlimit::Resource::NOFILE
        .set(64, hard)
        .expect("lower nofile");

    let mut sentinels = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(f) => sentinels.push(f),
            Err(_) => break,
        }
        if sentinels.len() > 200 {
            break;
        }
    }
    eprintln!("opened {} sentinel files", sentinels.len());

    let globs = Globs::new(GlobSetKind::from_iter(["**/*.rs"]).unwrap(), NO_EXCLUDE);
    let result = globs.walk(tmp.path());
    drop(sentinels);

    let err = result.expect_err("walk should have failed under EMFILE");
    let msg = format!("{err:#}").to_lowercase();
    eprintln!("got expected error: {msg}");
    assert!(
        msg.contains("too many open files") || msg.contains("emfile"),
        "expected EMFILE-style error, got: {msg}",
    );
}
