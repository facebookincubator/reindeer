/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::error;
use std::fmt;
use std::fmt::Display;

use nom::error::VerboseError;
use nom::error::convert_error;
use serde::Deserialize;
use serde::Serialize;

use crate::cfg;
use crate::config::Config;

/// A single PlatformConfig represents a single platform. Each field represents a set of
/// platform attributes which are true for this platform. A non-present attribute means
/// "doesn't matter" or "all possible values".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PlatformConfig(HashMap<String, HashSet<String>>);

pub fn platform_names_for_expr<'config>(
    config: &'config Config,
    expr: &PlatformExpr,
) -> Result<Vec<&'config PlatformName>, PredicateParseError> {
    let pred = PlatformPredicate::parse(expr)?;

    let res = config
        .platform
        .iter()
        .filter(|(_name, platconfig)| pred.eval(platconfig))
        .map(|(name, _config)| name)
        .collect();
    Ok(res)
}

// This platform just has common `deps` deps (not `platform_deps`)
const DEFAULT_PLATFORM: &str = "DEFAULT";

/// A name of a platform, as used in Config.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlatformName(String);

impl PlatformName {
    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_PLATFORM
    }
}

impl Display for PlatformName {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(fmt)
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
        self.0.fmt(fmt)
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
    pub fn parse(input: &'a PlatformExpr) -> Result<PlatformPredicate<'a>, PredicateParseError> {
        match cfg::parse::<VerboseError<&str>>(&input.0) {
            Ok(("", pred)) => Ok(pred),
            Ok((rest, _)) => Err(PredicateParseError::TrailingJunk(rest.to_string())),
            Err(nom::Err::Incomplete(_)) => Err(PredicateParseError::Incomplete),
            Err(nom::Err::Error(err)) | Err(nom::Err::Failure(err)) => Err(
                PredicateParseError::ParseError(convert_error(input.0.as_str(), err)),
            ),
        }
    }

    pub fn eval(&self, config: &PlatformConfig) -> bool {
        use PlatformPredicate::*;

        match self {
            Bool { key } => config.0.contains_key(*key),
            Value { key: "feature", .. } => {
                // [target.'cfg(feature = "...")'.dependencies] never get applied by Cargo
                false
            }
            Value { key, value } => config.0.get(*key).map_or(false, |set| set.contains(*value)),
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

impl<'a> Display for PlatformPredicate<'a> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        use PlatformPredicate::*;

        match self {
            Bool { key } => fmt.write_str(key),
            Value { key, value } => write!(fmt, "{} = \"{}\"", key, value),
            Any(preds) => write!(fmt, "any({})", itertools::join(preds.iter(), ", ")),
            All(preds) => write!(fmt, "all({})", itertools::join(preds.iter(), ", ")),
            Not(pred) => write!(fmt, "not({})", pred),
            Unix => fmt.write_str("unix"),
            Windows => fmt.write_str("windows"),
        }
    }
}
