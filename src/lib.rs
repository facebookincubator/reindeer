/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Library surface for integration tests under `tests/`. Reindeer is primarily
//! a binary; only modules that integration tests need are re-exposed here.
//! The binary in `src/main.rs` keeps its own `mod` declarations and does not
//! depend on this lib.

pub mod glob;
pub mod unused;
