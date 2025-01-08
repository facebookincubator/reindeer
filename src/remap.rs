/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap as Map;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RemapConfig {
    #[serde(rename = "source", default)]
    pub sources: Map<String, RemapSource>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RemapSource {
    pub registry: Option<String>,
    pub directory: Option<PathBuf>,
    pub git: Option<String>,
    pub rev: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    #[serde(rename = "replace-with")]
    pub replace_with: Option<String>,
    #[serde(rename = "local-registry")]
    pub local_registry: Option<PathBuf>,
}
