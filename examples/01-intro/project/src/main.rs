/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use once_cell::sync::Lazy;

const MAGIC: &str = "this is a magic string";

static SPECIAL: Lazy<String> = Lazy::new(|| MAGIC.to_string());

fn main() {
    println!("static {}", &*SPECIAL);
}
