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

    // 1. Uncomment this line
    // 2. cargo add -p project rand
	// 3. reindeer buckify
    // 4. modify the BUCK file to include "//third-party:rand"
    // 5. buck2 run //project:test
    //
    // println!("random {}", rand::random::<i32>());
}
