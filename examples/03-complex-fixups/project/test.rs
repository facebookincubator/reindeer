/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use once_cell::sync::Lazy;

// Use these crates without really using them
extern crate libc;
extern crate crossbeam_queue;

const MAGIC: &str = "this is a magic string";

static SPECIAL: Lazy<String> = Lazy::new(|| {
    let hash = blake3::hash(MAGIC.as_bytes());
    hash.to_hex().to_string()
});

fn main() {
    println!("blake3({:?}) = {}", MAGIC, &*SPECIAL);
}
