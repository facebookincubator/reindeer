/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Raise `RLIMIT_NOFILE` so parallel directory walks don't trip EMFILE.
//!
//! macOS in particular ships a tiny default soft limit (256), and reindeer's
//! fork-join walk over many crate source trees can easily exceed it. The
//! `rlimit` crate handles the OPEN_MAX quirk where requesting `RLIM_INFINITY`
//! is silently capped at `kern.maxfilesperproc`.

pub fn raise_nofile_limit() {
    match rlimit::increase_nofile_limit(rlimit::INFINITY) {
        Ok(new_limit) => log::debug!("RLIMIT_NOFILE soft limit set to {new_limit}"),
        Err(err) => log::warn!("failed to raise RLIMIT_NOFILE: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raise_does_not_lower_limit() {
        let (before, _) = rlimit::Resource::NOFILE.get().unwrap();
        raise_nofile_limit();
        let (after, _) = rlimit::Resource::NOFILE.get().unwrap();
        assert!(
            after >= before,
            "raise_nofile_limit lowered the limit: {before} -> {after}",
        );
    }
}
