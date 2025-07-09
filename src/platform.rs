/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error;
use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;

use nom_language::error::VerboseError;
use nom_language::error::convert_error;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Deserializer;
use serde::de::MapAccess;
use serde::de::Visitor;

use crate::cfg;
use crate::config::Config;

/// A single PlatformConfig represents a single platform. Each field represents a set of
/// platform attributes which are true for this platform. A non-present attribute means
/// "doesn't matter" or "all possible values".
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    pub is_execution_platform: Option<bool>,
    /// Set of features enabled in this platform. If omitted, the "default"
    /// feature will be enabled if one exists.
    pub features: Option<BTreeSet<String>>,
    pub execution_platforms: BTreeSet<PlatformName>,
    pub cfg: HashMap<String, HashSet<String>>,
}

impl<'de> Deserialize<'de> for PlatformConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PlatformConfigVisitor;

        impl<'de> Visitor<'de> for PlatformConfigVisitor {
            type Value = PlatformConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct PlatformConfig")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut is_execution_platform = None;
                let mut features = None;
                let mut cfg = HashMap::new();

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "execution-platform" => is_execution_platform = map.next_value()?,
                        "features" => features = map.next_value()?,
                        _ => {
                            let values: HashSet<String> = map.next_value()?;
                            cfg.insert(key, values);
                        }
                    }
                }

                Ok(PlatformConfig {
                    is_execution_platform,
                    features,
                    // Populated later.
                    execution_platforms: BTreeSet::new(),
                    cfg,
                })
            }
        }

        deserializer.deserialize_map(PlatformConfigVisitor)
    }
}

pub fn platform_names_for_expr<'config>(
    config: &'config Config,
    expr: &PlatformExpr,
) -> anyhow::Result<Vec<&'config PlatformName>> {
    let pred = PlatformPredicate::parse(expr)?;

    let res = config
        .platform
        .iter()
        .filter(|(_name, platconfig)| pred.eval(platconfig))
        .map(|(name, _config)| name)
        .collect();
    Ok(res)
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
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlatformExpr(String);

impl From<String> for PlatformExpr {
    fn from(s: String) -> Self {
        PlatformExpr(s)
    }
}

impl Display for PlatformExpr {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, fmt)
    }
}

/// Platform predicate which can be matched against a PlatformConfig
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PlatformPredicate<'a> {
    // Operators
    Any(Vec<PlatformPredicate<'a>>),
    All(Vec<PlatformPredicate<'a>>),
    Not(Box<PlatformPredicate<'a>>),

    // Predicates
    Value { key: &'a str, value: &'a str },
    Bool { key: &'a str },

    // Helpers
    Unix,
    Windows,
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

impl<'a> PlatformPredicate<'a> {
    pub fn parse(input: &'a PlatformExpr) -> anyhow::Result<PlatformPredicate<'a>> {
        let err = match cfg::parse::<VerboseError<&str>>(&input.0) {
            Ok(("", pred)) => return Ok(pred),
            Ok((rest, _)) => PredicateParseError::TrailingJunk(rest.to_string()),
            Err(nom::Err::Incomplete(_)) => PredicateParseError::Incomplete,
            Err(nom::Err::Error(err)) | Err(nom::Err::Failure(err)) => {
                PredicateParseError::ParseError(convert_error(input.0.as_str(), err))
            }
        };
        Err(anyhow::Error::new(err).context(format!("Bad platform expression `{input}`")))
    }

    pub fn eval(&self, config: &PlatformConfig) -> bool {
        use PlatformPredicate::*;

        match self {
            Bool { key } => config.cfg.contains_key(*key),
            Value { key: "feature", .. } => {
                // [target.'cfg(feature = "...")'.dependencies] never get applied by Cargo
                false
            }
            Value { key, value } => config.cfg.get(*key).is_some_and(|set| set.contains(*value)),
            Not(pred) => !pred.eval(config),
            Any(preds) => preds.iter().any(|pred| pred.eval(config)),
            All(preds) => preds.iter().all(|pred| pred.eval(config)),
            Unix => PlatformPredicate::Value {
                key: "target_family",
                value: "unix",
            }
            .eval(config),
            Windows => PlatformPredicate::Value {
                key: "target_family",
                value: "windows",
            }
            .eval(config),
        }
    }
}
