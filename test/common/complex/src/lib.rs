/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub fn complex() {
    println!("I'm complex because I have a dependency.");
    println!("Hello {}!", whoami::realname());

    simple::simple();
}
