/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use monostate::MustBe;
use serde::Deserialize;
use serde::Serialize;

use crate::buck;
use crate::buck::Rule;
use crate::config::StringWithDefault;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UniverseConfig {
    /// A set of additional features to enable in this universe only.
    /// If omitted, the "default" feature will be enabled if one exists.
    pub features: Option<BTreeSet<String>>,
    /// The Buck2 `select` key to use for this universe.
    pub constraint: StringWithDefault<MustBe!("DEFAULT")>,
    /// When present, the universe will contain only crates reachable from this
    /// set. Crates outside the set will not be considered during feature
    /// resolution.
    #[serde(default)]
    pub include_crates: BTreeSet<String>,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct UniverseName(String);

impl Default for UniverseName {
    fn default() -> Self {
        Self("DEFAULT".to_owned())
    }
}

impl fmt::Display for UniverseName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for UniverseName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // More compact than a derived Debug impl
        let UniverseName(name) = self;
        write!(f, "UniverseName({name:?})")
    }
}

impl From<String> for UniverseName {
    fn from(s: String) -> Self {
        Self(s)
    }
}

pub fn merge_universes(
    config: &BTreeMap<UniverseName, UniverseConfig>,
    universes: BTreeMap<UniverseName, BTreeSet<Rule>>,
) -> anyhow::Result<BTreeSet<Rule>> {
    use crate::buck::BuildscriptGenrule;
    use crate::buck::PlatformRustCommon;
    use crate::buck::RustBinary;
    use crate::buck::RustLibrary;

    /// Map `rule(features = ["std"])` to `rule(features = select({<key>: ["std"]}))`
    fn set_select_keys(key: UniverseName, attrs: &mut PlatformRustCommon) {
        attrs.features.set_key(key.clone());
        attrs.deps.set_key(key.clone());
        attrs.named_deps.set_key(key.clone());
        attrs.env.set_key(key);
    }
    fn set_library_keys(key: UniverseName, rule: &mut RustLibrary) {
        for platform in rule.common.platform.values_mut() {
            set_select_keys(key.clone(), platform);
        }
        set_select_keys(key, &mut rule.common.base);
    }
    fn set_binary_keys(key: UniverseName, rule: &mut RustBinary) {
        for platform in rule.common.platform.values_mut() {
            set_select_keys(key.clone(), platform);
        }
        set_select_keys(key, &mut rule.common.base);
    }
    fn set_genrule_keys(key: UniverseName, rule: &mut BuildscriptGenrule) {
        rule.features.set_key(key.clone());
    }

    /// Given
    /// ```
    /// old = rule(features = select({"DEFAULT": ["std"]}))
    /// new = rule(features = select({"no_std": []}))
    /// ```
    /// produce
    /// ```
    /// rule(features = select({"DEFAULT": ["std"], "no_std": []}))
    /// ```
    fn merge_selects(old: &mut PlatformRustCommon, new: PlatformRustCommon) {
        old.features.merge(new.features);
        old.deps.merge(new.deps);
        old.named_deps.merge(new.named_deps);
        old.env.merge(new.env);
    }
    fn merge_library(old: &mut RustLibrary, new: RustLibrary) {
        for (platform, new_select) in new.common.platform {
            match old.common.platform.get_mut(&platform) {
                Some(old_select) => {
                    merge_selects(old_select, new_select);
                }
                None => {
                    old.common.platform.insert(platform, new_select);
                }
            }
        }
        merge_selects(&mut old.common.base, new.common.base);
    }
    fn merge_binary(old: &mut RustBinary, new: RustBinary) {
        for (platform, new_select) in new.common.platform {
            match old.common.platform.get_mut(&platform) {
                Some(old_select) => {
                    merge_selects(old_select, new_select);
                }
                None => {
                    old.common.platform.insert(platform, new_select);
                }
            }
        }
        merge_selects(&mut old.common.base, new.common.base);
    }
    fn merge_genrule(old: &mut BuildscriptGenrule, new: BuildscriptGenrule) {
        old.features.merge(new.features);
    }

    /// Simplify select maps to values when all universes have the same value.
    /// Remap select keys from universe names to universe constraints.
    fn finalize<T: PartialEq>(
        config: &BTreeMap<UniverseName, UniverseConfig>,
        map: &mut buck::Selectable<UniverseName, T>,
    ) {
        map.simplify();
        map.map_keys(|k| config[k].constraint.as_str().to_owned().into())
    }
    let finalize_select_keys = |attrs: &mut PlatformRustCommon| {
        finalize(config, &mut attrs.features);
        finalize(config, &mut attrs.deps);
        finalize(config, &mut attrs.named_deps);
        finalize(config, &mut attrs.env);
    };
    let finalize_library_keys = |rule: &mut RustLibrary| {
        for platform in rule.common.platform.values_mut() {
            finalize_select_keys(platform);
        }
        finalize_select_keys(&mut rule.common.base);
    };
    let finalize_binary_keys = |rule: &mut RustBinary| {
        for platform in rule.common.platform.values_mut() {
            finalize_select_keys(platform);
        }
        finalize_select_keys(&mut rule.common.base);
    };
    let finalize_genrule_keys = |rule: &mut BuildscriptGenrule| {
        finalize(config, &mut rule.features);
    };

    // set select keys to universe name; construct Name -> Rule map for merging
    let mut universes = universes.into_iter().map(|(name, rules)| {
        rules
            .into_iter()
            .map(|mut rule| {
                match &mut rule {
                    Rule::Library(rule) | Rule::RootPackage(rule) => {
                        set_library_keys(name.clone(), rule)
                    }
                    Rule::Binary(rule) | Rule::BuildscriptBinary(rule) => {
                        set_binary_keys(name.clone(), rule)
                    }
                    Rule::BuildscriptGenrule(rule) => set_genrule_keys(name.clone(), rule),
                    _ => {}
                }
                (rule.get_name().clone(), rule)
            })
            .collect::<BTreeMap<buck::Name, Rule>>()
    });

    // merge universes
    let mut rules = universes.next().unwrap();
    for universe_rules in universes {
        for (name, rule) in universe_rules {
            if let Some(old_rule) = rules.get_mut(&name) {
                match old_rule {
                    Rule::Library(old) | Rule::RootPackage(old) => {
                        let (Rule::Library(new) | Rule::RootPackage(new)) = rule else {
                            panic!("expected library")
                        };
                        merge_library(old, new);
                    }
                    Rule::Binary(old) | Rule::BuildscriptBinary(old) => {
                        let (Rule::Binary(new) | Rule::BuildscriptBinary(new)) = rule else {
                            panic!("expected binary")
                        };
                        merge_binary(old, new);
                    }
                    Rule::BuildscriptGenrule(old) => {
                        let Rule::BuildscriptGenrule(new) = rule else {
                            panic!("expected buildscript genrule")
                        };
                        merge_genrule(old, new);
                    }
                    Rule::Alias(old) => {
                        let Rule::Alias(new) = rule else {
                            panic!("expected alias")
                        };
                        if *old != new {
                            panic!("expected alias rules to be identical in every universe")
                        }
                    }
                    Rule::HttpArchive(old) => {
                        let Rule::HttpArchive(new) = rule else {
                            panic!("expected http_archive")
                        };
                        if *old != new {
                            panic!("expected http_archive rules to be identical in every universe")
                        }
                    }
                    Rule::GitFetch(old) => {
                        let Rule::GitFetch(new) = rule else {
                            panic!("expected git_fetch")
                        };
                        if *old != new {
                            panic!("expected git_fetch rules to be identical in every universe")
                        }
                    }
                    _ => {
                        log::warn!(
                            "Skipping unhandled rule while merging universes: {:?}",
                            rule
                        );
                    }
                }
            } else {
                rules.insert(name, rule);
            }
        }
    }

    // finalize
    for rule in rules.values_mut() {
        match rule {
            Rule::Library(rule) | Rule::RootPackage(rule) => finalize_library_keys(rule),
            Rule::Binary(rule) | Rule::BuildscriptBinary(rule) => finalize_binary_keys(rule),
            Rule::BuildscriptGenrule(rule) => finalize_genrule_keys(rule),
            _ => {}
        }
    }

    Ok(rules.into_values().collect())
}

pub fn validate_universe_config(
    universe_name: &UniverseName,
    config: &UniverseConfig,
    index: &crate::index::Index,
) -> anyhow::Result<()> {
    for feature in config.features.iter().flatten() {
        if feature.contains('/') {
            anyhow::bail!(
                "Universe {universe_name} specifies features {feature:?}. \
                 Features containing '/' are not supported in universe configuration. \
                 Move this to a feature in the [features] section of Cargo.toml."
            );
        }
    }
    for krate in &config.include_crates {
        if !index.is_public_package_name(krate) {
            anyhow::bail!(
                "Universe {universe_name} specifies in `include_crates` \
                 a crate which is not a public package: {krate:?}. \
                 Add it as a dependency of the root crate \
                 or remove it from the `include_crates` list in Reindeer config."
            );
        };
    }
    Ok(())
}
