/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::config::Config;
use serde::de::Deserializer;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::error;
use std::fmt;
use std::fmt::Display;

use nom::error::convert_error;
use nom::error::VerboseError;

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

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(Serialize)]
/// A name of a platform, as used in Config.
pub struct PlatformName(String);

impl PlatformName {
    pub fn is_default(&self) -> bool {
        self.0 == DEFAULT_PLATFORM
    }
}

impl<'de> Deserialize<'de> for PlatformName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(PlatformName)
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
#[derive(Serialize)]
pub struct PlatformExpr(String);

impl From<String> for PlatformExpr {
    fn from(s: String) -> Self {
        PlatformExpr(s)
    }
}

impl<'de> Deserialize<'de> for PlatformExpr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(PlatformExpr)
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
        match parser::parse_cfg::<VerboseError<&str>>(&input.0) {
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
            Value { key, value } => config.0.get(*key).map_or(true, |set| set.contains(*value)),
            Not(pred) => !pred.eval(config),
            Any(preds) => preds.iter().any(|pred| pred.eval(config)),
            All(preds) => preds.iter().all(|pred| pred.eval(config)),
            Unix => self.target_family_bool("unix", config),
            Windows => self.target_family_bool("windows", config),
        }
    }

    fn target_family_bool(&self, family: &str, config: &PlatformConfig) -> bool {
        PlatformPredicate::Bool { key: family }.eval(config)
            || PlatformPredicate::Value {
                key: "target_family",
                value: family,
            }
            .eval(config)
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

mod parser {
    use nom::branch::alt;
    use nom::bytes::complete::escaped;
    use nom::bytes::complete::tag;
    use nom::bytes::complete::take_while1;
    use nom::character::complete::char;
    use nom::character::complete::multispace0;
    use nom::character::complete::one_of;
    use nom::combinator::cut;
    use nom::combinator::map;
    use nom::combinator::opt;
    use nom::error::context;
    use nom::error::ContextError;
    use nom::error::ParseError;
    use nom::multi::separated_list0;
    use nom::sequence::delimited;
    use nom::sequence::preceded;
    use nom::sequence::separated_pair;
    use nom::sequence::terminated;
    use nom::AsChar;
    use nom::IResult;

    use super::PlatformPredicate;

    fn sp<'a, E: ParseError<&'a str>>(i: &'a str) -> IResult<&'a str, &'a str, E> {
        multispace0(i)
    }

    fn graphic<'a, E: ParseError<&'a str>>(i: &'a str) -> IResult<&'a str, &'a str, E> {
        take_while1(|c: char| c.is_ascii_graphic() && !(c == '\\' || c == '"'))(i)
    }

    fn parse_str<'a, E: ParseError<&'a str>>(i: &'a str) -> IResult<&'a str, &'a str, E> {
        escaped(graphic, '\\', one_of(r#"n"\"#))(i)
    }

    fn string<'a, E: ParseError<&'a str> + ContextError<&'a str>>(
        i: &'a str,
    ) -> IResult<&'a str, &'a str, E> {
        context(
            "string",
            preceded(
                char('\"'),
                cut(terminated(
                    map(opt(parse_str), |v| v.unwrap_or("")),
                    char('\"'),
                )),
            ),
        )(i)
    }

    fn sep<'a, E: ParseError<&'a str>>(
        sep: char,
    ) -> impl FnMut(&'a str) -> IResult<&'a str, (), E> {
        map(preceded(sp, char(sep)), |_| ())
    }

    fn keyword<'a, E: ParseError<&'a str>>(
        kw: &'static str,
    ) -> impl FnMut(&'a str) -> IResult<&'a str, (), E> {
        map(preceded(sp, tag(kw)), |_| ())
    }

    fn atom<'a, E: ParseError<&'a str>>(i: &'a str) -> IResult<&'a str, &'a str, E> {
        let extras = "-_";
        preceded(
            sp,
            take_while1(move |c: char| c.is_alphanum() || extras.contains(c)),
        )(i)
    }

    // Parses: `keyword` '(' inner ')'
    fn operator<'a, T, E: ParseError<&'a str> + ContextError<&'a str>>(
        kw: &'static str,
        inner: impl FnMut(&'a str) -> IResult<&'a str, T, E>,
    ) -> impl FnMut(&'a str) -> IResult<&'a str, T, E> {
        context(
            kw,
            preceded(keyword(kw), cut(delimited(sep('('), inner, sep(')')))),
        )
    }

    fn parse_predicate<'a, E: ParseError<&'a str> + ContextError<&'a str>>(
        i: &'a str,
    ) -> IResult<&'a str, PlatformPredicate<'a>, E> {
        use PlatformPredicate::*;

        context(
            "predicate",
            alt((
                map(
                    operator("all", separated_list0(sep(','), parse_predicate)),
                    All,
                ),
                map(
                    operator("any", separated_list0(sep(','), parse_predicate)),
                    Any,
                ),
                map(operator("not", parse_predicate), |pred| Not(Box::new(pred))),
                map(keyword("unix"), |_| Unix),
                map(keyword("windows"), |_| Windows),
                map(
                    separated_pair(atom, sep('='), cut(preceded(sp, string))),
                    |(key, value)| Value { key, value },
                ),
                map(atom, |key| Bool { key }),
            )),
        )(i)
    }

    pub fn parse_cfg<'a, E: ParseError<&'a str> + ContextError<&'a str>>(
        i: &'a str,
    ) -> IResult<&'a str, PlatformPredicate<'a>, E> {
        context(
            "cfg",
            alt((
                preceded(
                    keyword("cfg"),
                    cut(delimited(sep('('), parse_predicate, sep(')'))),
                ),
                map(atom, |triple| PlatformPredicate::Bool { key: triple }),
            )),
        )(i)
    }

    #[cfg(test)]
    mod test {
        use super::super::PlatformPredicate::*;
        use super::*;

        #[test]
        fn test_unix() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(unix)");
            println!("res = {:?}", res);
            assert_eq!(res, Ok(("", Unix)))
        }

        #[test]
        fn test_windows() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(windows)");
            println!("res = {:?}", res);
            assert_eq!(res, Ok(("", Windows)))
        }

        #[test]
        fn test_any() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(any(windows, unix))");
            println!("res = {:?}", res);
            assert_eq!(res, Ok(("", Any(vec![Windows, Unix]))))
        }

        #[test]
        fn test_all() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(all(windows, unix))");
            println!("res = {:?}", res);
            assert_eq!(res, Ok(("", All(vec![Windows, Unix]))))
        }

        #[test]
        fn test_atom() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(  foobar  )");
            println!("res = {:?}", res);
            assert_eq!(res, Ok(("", Bool { key: "foobar" })))
        }

        #[test]
        fn test_value() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>("cfg(feature = \"bloop\")");
            println!("res = {:?}", res);
            assert_eq!(
                res,
                Ok((
                    "",
                    Value {
                        key: "feature",
                        value: "bloop"
                    }
                ))
            )
        }

        #[test]
        fn test_emptystr() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>(r#"cfg(target_env = "")"#);
            println!("res = {:?}", res);
            assert_eq!(
                res,
                Ok((
                    "",
                    Value {
                        key: "target_env",
                        value: ""
                    }
                ))
            )
        }

        #[test]
        fn test_complex() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>(
                "cfg(all(not(target_os = \"macos\"), not(windows), not(target_os = \"ios\")))",
            );
            println!("res = {:?}", res);
            assert_eq!(
                res,
                Ok((
                    "",
                    All(vec!(
                        Not(Box::new(Value {
                            key: "target_os",
                            value: "macos"
                        })),
                        Not(Box::new(Windows)),
                        Not(Box::new(Value {
                            key: "target_os",
                            value: "ios"
                        }))
                    ))
                ))
            )
        }

        #[test]
        fn test_numcpus() {
            let res = parse_cfg::<(_, nom::error::ErrorKind)>(
                "cfg(all(any(target_arch = \"x86_64\", target_arch = \"aarch64\"), target_os = \"hermit\"))",
            );
            println!("res = {:?}", res);
            assert_eq!(
                res,
                Ok((
                    "",
                    All(vec![
                        Any(vec![
                            Value {
                                key: "target_arch",
                                value: "x86_64"
                            },
                            Value {
                                key: "target_arch",
                                value: "aarch64"
                            }
                        ]),
                        Value {
                            key: "target_os",
                            value: "hermit"
                        }
                    ])
                ))
            )
        }
    }
}
