# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under both the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree and the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree.

load("@prelude//cfg/modifier:cfg_constructor.bzl", "cfg_constructor_post_constraint_analysis", "cfg_constructor_pre_constraint_analysis")
load("@prelude//cfg/modifier:common.bzl", "MODIFIER_METADATA_KEY")

def set_cfg_constructor(aliases = dict()):
    project_root_cell = read_root_config("cell_aliases", "root")
    current_root_cell = read_config("cell_aliases", "root")
    if project_root_cell == current_root_cell:
        native.set_cfg_constructor(
            stage0 = cfg_constructor_pre_constraint_analysis,
            stage1 = cfg_constructor_post_constraint_analysis,
            key = MODIFIER_METADATA_KEY,
            aliases = struct(**aliases),
            extra_data = struct(),
        )
