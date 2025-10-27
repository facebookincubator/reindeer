/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::error;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::ops::Not as _;

use foldhash::HashMap;
use foldhash::HashSet;
use nom_language::error::VerboseError;
use nom_language::error::convert_error;
use semver::Version;
use semver::VersionReq;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeSeed;
use serde::de::Deserializer;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;

use crate::cfg;

/// A single PlatformConfig represents a single platform. Each field represents a set of
/// platform attributes which are true for this platform. A non-present attribute means
/// "doesn't matter" or "all possible values".
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    pub target: Option<String>,
    pub is_execution_platform: Option<bool>,
    /// Set of features enabled in this platform. If omitted, the "default"
    /// feature will be enabled if one exists.
    pub features: Option<BTreeSet<String>>,
    pub execution_platforms: BTreeSet<PlatformName>,
    pub cfg: HashMap<String, HashSet<String>>,
}

pub fn deserialize_platforms<'de, D>(
    deserializer: D,
) -> Result<HashMap<PlatformName, PlatformConfig>, D::Error>
where
    D: Deserializer<'de>,
{
    struct PlatformsVisitor;

    impl<'de> Visitor<'de> for PlatformsVisitor {
        type Value = HashMap<PlatformName, PlatformConfig>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("map of platform name to PlatformConfig")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut platforms = HashMap::default();
            while let Some(platform_name) = map.next_key()? {
                let seed = PlatformConfigVisitor(&platform_name);
                let platform_config = map.next_value_seed(seed)?;
                platforms.insert(platform_name, platform_config);
            }
            Ok(platforms)
        }
    }

    struct PlatformConfigVisitor<'a>(&'a PlatformName);

    impl<'de> DeserializeSeed<'de> for PlatformConfigVisitor<'_> {
        type Value = PlatformConfig;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_map(self)
        }
    }

    impl<'de> Visitor<'de> for PlatformConfigVisitor<'_> {
        type Value = PlatformConfig;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("struct PlatformConfig")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut target = None;
            let mut is_execution_platform = None;
            let mut features = None;
            let mut cfg = HashMap::default();

            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "target" => target = map.next_value()?,
                    "execution-platform" => is_execution_platform = map.next_value()?,
                    "features" => {
                        let seed = FeaturesVisitor(self.0);
                        features = Some(map.next_value_seed(seed)?);
                    }
                    _ => {
                        let values: HashSet<String> = map.next_value()?;
                        cfg.insert(key, values);
                    }
                }
            }

            Ok(PlatformConfig {
                target,
                is_execution_platform,
                features,
                // Populated later.
                execution_platforms: BTreeSet::new(),
                cfg,
            })
        }
    }

    struct FeaturesVisitor<'a>(&'a PlatformName);

    impl<'de> DeserializeSeed<'de> for FeaturesVisitor<'_> {
        type Value = BTreeSet<String>;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_seq(self)
        }
    }

    impl<'de> Visitor<'de> for FeaturesVisitor<'_> {
        type Value = BTreeSet<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("array of strings")
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: SeqAccess<'de>,
        {
            let mut features = BTreeSet::new();
            while let Some(feature) = seq.next_element_seed(FeatureVisitor(self.0))? {
                features.insert(feature);
            }
            Ok(features)
        }
    }

    struct FeatureVisitor<'a>(&'a PlatformName);

    impl<'de> DeserializeSeed<'de> for FeatureVisitor<'_> {
        type Value = String;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_str(self)
        }
    }

    impl<'de> Visitor<'de> for FeatureVisitor<'_> {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string")
        }

        fn visit_str<E>(self, string: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if string.contains('/') {
                return Err(E::custom(format!(
                    "Platform {platform_name} specifies feature {string:?}. \
                     Features containing '/' are not supported in platform configuration. \
                     Move this to a feature in the [features] section of Cargo.toml.",
                    platform_name = self.0,
                )));
            }
            Ok(string.to_owned())
        }
    }

    deserializer.deserialize_map(PlatformsVisitor)
}

/// A name of a platform, as used in Config.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlatformName(String);

impl Display for PlatformName {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, fmt)
    }
}

impl Debug for PlatformName {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        // More compact than a derived Debug impl
        let PlatformName(name) = self;
        write!(fmt, "PlatformName({name:?})")
    }
}

/// A Cargo-style platform predicate expression
/// such as `cfg(target_arch = "z80")`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum PlatformExpr {
    // Operators
    Any(Vec<PlatformExpr>),
    All(Vec<PlatformExpr>),
    Not(Box<PlatformExpr>),

    // Predicates
    Value { key: String, value: String },
    Bool { key: String },
    Version(VersionReq),
    Target(String),

    // Helpers
    Unix,
    Windows,
}

impl Default for PlatformExpr {
    fn default() -> Self {
        // Always true
        PlatformExpr::All(Vec::new())
    }
}

#[derive(Debug, Clone)]
pub enum PredicateParseError {
    TrailingJunk(String),
    Incomplete,
    ParseError(String),
}

impl Display for PredicateParseError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PredicateParseError::TrailingJunk(junk) => write!(fmt, "trailing junk: {}", junk),
            PredicateParseError::ParseError(msg) => write!(fmt, "parse error: {}", msg),
            PredicateParseError::Incomplete => write!(fmt, "incomplete input"),
        }
    }
}

impl error::Error for PredicateParseError {}

impl PlatformExpr {
    pub fn parse(input: &str) -> anyhow::Result<PlatformExpr> {
        let err = match cfg::parse::<VerboseError<&str>>(input) {
            Ok(("", pred)) => return Ok(pred),
            Ok((rest, _)) => PredicateParseError::TrailingJunk(rest.to_string()),
            Err(nom::Err::Incomplete(_)) => PredicateParseError::Incomplete,
            Err(nom::Err::Error(err)) | Err(nom::Err::Failure(err)) => {
                PredicateParseError::ParseError(convert_error(input, err))
            }
        };
        Err(anyhow::Error::new(err).context(format!("Bad platform expression `{input}`")))
    }

    /// Used for restricting fixups that are allowed to be version-specific but
    /// not platform-specific.
    pub fn uses_cfg_other_than_version(&self) -> bool {
        match self {
            PlatformExpr::Any(inner) | PlatformExpr::All(inner) => {
                inner.iter().any(Self::uses_cfg_other_than_version)
            }
            PlatformExpr::Not(inner) => inner.uses_cfg_other_than_version(),
            PlatformExpr::Value { .. }
            | PlatformExpr::Bool { .. }
            | PlatformExpr::Target(_)
            | PlatformExpr::Unix
            | PlatformExpr::Windows => true,
            PlatformExpr::Version(_) => false,
        }
    }

    pub fn eval(
        &self,
        config: &PlatformConfig,
        version: Option<&Version>,
        extra_cfg: &HashMap<String, HashSet<String>>,
    ) -> bool {
        match self {
            PlatformExpr::Bool { key } => {
                config.cfg.contains_key(key) || extra_cfg.contains_key(key)
            }
            PlatformExpr::Value { key, value } => {
                if key == "feature" {
                    // [target.'cfg(feature = "...")'.dependencies] never get applied by Cargo
                    false
                } else {
                    config.cfg.get(key).is_some_and(|set| set.contains(value))
                        || extra_cfg.get(key).is_some_and(|set| set.contains(value))
                }
            }
            PlatformExpr::Version(req) => match version {
                None => false,
                Some(version) => req.matches(version),
            },
            PlatformExpr::Target(target) => config.target.as_ref() == Some(target),
            PlatformExpr::Not(pred) => !pred.eval(config, version, extra_cfg),
            PlatformExpr::Any(preds) => preds
                .iter()
                .any(|pred| pred.eval(config, version, extra_cfg)),
            PlatformExpr::All(preds) => preds
                .iter()
                .all(|pred| pred.eval(config, version, extra_cfg)),
            PlatformExpr::Unix => PlatformExpr::Value {
                key: "target_family".to_owned(),
                value: "unix".to_owned(),
            }
            .eval(config, version, extra_cfg),
            PlatformExpr::Windows => PlatformExpr::Value {
                key: "target_family".to_owned(),
                value: "windows".to_owned(),
            }
            .eval(config, version, extra_cfg),
        }
    }

    pub fn eval_only_version(&self, version: &Version) -> Option<bool> {
        match self {
            PlatformExpr::Version(req) => Some(req.matches(version)),
            PlatformExpr::Any(inner) => {
                for pred in inner {
                    if pred.eval_only_version(version)? {
                        return Some(true);
                    }
                }
                Some(false)
            }
            PlatformExpr::All(inner) => {
                for pred in inner {
                    if !pred.eval_only_version(version)? {
                        return Some(false);
                    }
                }
                Some(true)
            }
            PlatformExpr::Not(inner) => inner.eval_only_version(version).map(bool::not),
            PlatformExpr::Value { .. }
            | PlatformExpr::Bool { .. }
            | PlatformExpr::Target(_)
            | PlatformExpr::Unix
            | PlatformExpr::Windows => None,
        }
    }
}

impl<'de> Deserialize<'de> for PlatformExpr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PlatformExprVisitor;

        impl<'de> Visitor<'de> for PlatformExprVisitor {
            type Value = PlatformExpr;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("cfg string")
            }

            fn visit_str<E>(self, string: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                PlatformExpr::parse(string).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(PlatformExprVisitor)
    }
}
