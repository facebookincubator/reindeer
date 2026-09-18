/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

unsafe extern "C" {
    static reindeer_linker_flag: u8;
}

#[unsafe(no_mangle)]
pub extern "C" fn with_linker_flags() -> *const u8 {
    &raw const reindeer_linker_flag
}
