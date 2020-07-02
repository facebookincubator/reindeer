/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use once_cell::sync::Lazy;

const MAGIC: &str = "this is a magic string";

static SPECIAL: Lazy<String> = Lazy::new(|| {
    let hash = blake3::hash(MAGIC.as_bytes());
    hash.to_hex().to_string()
});

fn main() {
    println!("blake3({:?}) = {}", MAGIC, &*SPECIAL);
}
