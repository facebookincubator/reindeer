/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use nom::IResult;
use nom::Parser;
use nom::branch::alt;
use nom::bytes::complete::escaped;
use nom::bytes::complete::tag;
use nom::bytes::complete::take_while1;
use nom::character::complete::char;
use nom::character::complete::multispace0;
use nom::character::complete::one_of;
use nom::character::complete::satisfy;
use nom::combinator::cut;
use nom::combinator::map;
use nom::combinator::map_res;
use nom::combinator::opt;
use nom::combinator::recognize;
use nom::combinator::verify;
use nom::error::ContextError;
use nom::error::FromExternalError;
use nom::error::ParseError;
use nom::error::context;
use nom::multi::many0_count;
use nom::multi::separated_list0;
use nom::multi::separated_list1;
use nom::sequence::delimited;
use nom::sequence::pair;
use nom::sequence::preceded;
use nom::sequence::separated_pair;
use nom::sequence::terminated;
use unicode_ident::is_xid_continue;
use unicode_ident::is_xid_start;

use crate::platform::PlatformExpr;

fn sp<'a, E>(i: &'a str) -> IResult<&'a str, &'a str, E>
where
    E: ParseError<&'a str>,
{
    multispace0(i)
}

fn graphic<'a, E>(i: &'a str) -> IResult<&'a str, &'a str, E>
where
    E: ParseError<&'a str>,
{
    take_while1(|c: char| (c.is_ascii_graphic() || c == ' ') && !(c == '\\' || c == '"')).parse(i)
}

fn parse_str<'a, E>(i: &'a str) -> IResult<&'a str, &'a str, E>
where
    E: ParseError<&'a str>,
{
    escaped(graphic, '\\', one_of(r#"n"\"#)).parse(i)
}

fn string<'a, E>(i: &'a str) -> IResult<&'a str, &'a str, E>
where
    E: ParseError<&'a str> + ContextError<&'a str>,
{
    context(
        "string",
        preceded(
            char('\"'),
            cut(terminated(
                map(opt(parse_str), |v| v.unwrap_or("")),
                char('\"'),
            )),
        ),
    )
    .parse(i)
}

fn sep<'a, E>(sep: char) -> impl Parser<&'a str, Output = (), Error = E>
where
    E: ParseError<&'a str>,
{
    map(preceded(sp, char(sep)), drop)
}

fn keyword<'a, E>(kw: &'static str) -> impl Parser<&'a str, Output = (), Error = E>
where
    E: ParseError<&'a str>,
{
    map(verify(atom, move |s: &str| s == kw), drop)
}

// Parse an atom comprising one or more hyphen-separated words. Each word
// has a first character that satisfies is_xid_start and the rest satisfy
// is_xid_continue.
//
// For example `target_os` or `x86_64-unknown-linux-gnu`
fn atom<'a, E>(i: &'a str) -> IResult<&'a str, &'a str, E>
where
    E: ParseError<&'a str>,
{
    preceded(
        sp,
        recognize(separated_list1(
            tag("-"),
            pair(
                satisfy(|ch| is_xid_start(ch) || ch == '_'),
                many0_count(satisfy(is_xid_continue)),
            ),
        )),
    )
    .parse(i)
}

// Parses: `keyword` '(' inner ')'
fn operator<'a, F>(
    kw: &'static str,
    inner: F,
) -> impl Parser<&'a str, Output = F::Output, Error = F::Error>
where
    F: Parser<&'a str, Error: ContextError<&'a str>>,
{
    context(
        kw,
        preceded(keyword(kw), cut(delimited(sep('('), inner, sep(')')))),
    )
}

fn parse_predicate<'a, E>(i: &'a str) -> IResult<&'a str, PlatformExpr, E>
where
    E: ParseError<&'a str> + ContextError<&'a str> + FromExternalError<&'a str, semver::Error>,
{
    context(
        "predicate",
        alt((
            map(
                operator("all", separated_list0(sep(','), parse_predicate)),
                PlatformExpr::All,
            ),
            map(
                operator("any", separated_list0(sep(','), parse_predicate)),
                PlatformExpr::Any,
            ),
            map(operator("not", parse_predicate), |pred| {
                PlatformExpr::Not(Box::new(pred))
            }),
            map(
                preceded(
                    (keyword("version"), sep('='), sp),
                    cut(map_res(string, semver::VersionReq::parse)),
                ),
                PlatformExpr::Version,
            ),
            map(keyword("unix"), |_| PlatformExpr::Unix),
            map(keyword("windows"), |_| PlatformExpr::Windows),
            map(
                separated_pair(atom, sep('='), cut(preceded(sp, string))),
                |(key, value)| PlatformExpr::Value {
                    key: key.to_owned(),
                    value: value.to_owned(),
                },
            ),
            map(atom, |key| PlatformExpr::Bool {
                key: key.to_owned(),
            }),
        )),
    )
    .parse(i)
}

pub(crate) fn parse<'a, E>(i: &'a str) -> IResult<&'a str, PlatformExpr, E>
where
    E: ParseError<&'a str> + ContextError<&'a str> + FromExternalError<&'a str, semver::Error>,
{
    context(
        "cfg",
        alt((
            preceded(
                keyword("cfg"),
                cut(delimited(sep('('), parse_predicate, sep(')'))),
            ),
            map(atom, |triple| PlatformExpr::Bool {
                key: triple.to_owned(),
            }),
        )),
    )
    .parse(i)
}

#[cfg(test)]
mod test {
    use crate::cfg;
    use crate::platform::PlatformExpr::*;

    #[test]
    fn test_unix() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(unix)");
        println!("res = {:?}", res);
        assert_eq!(res, Ok(("", Unix)))
    }

    #[test]
    fn test_windows() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(windows)");
        println!("res = {:?}", res);
        assert_eq!(res, Ok(("", Windows)))
    }

    #[test]
    fn test_any() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(any(windows, unix))");
        println!("res = {:?}", res);
        assert_eq!(res, Ok(("", Any(vec![Windows, Unix]))))
    }

    #[test]
    fn test_all() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(all(windows, unix))");
        println!("res = {:?}", res);
        assert_eq!(res, Ok(("", All(vec![Windows, Unix]))))
    }

    #[test]
    fn test_atom() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(  foobar  )");
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                Bool {
                    key: "foobar".to_owned(),
                }
            ))
        )
    }

    #[test]
    fn test_atom_with_keyword_prefix() {
        let res =
            cfg::parse::<(_, nom::error::ErrorKind)>("cfg(any(windows_raw_dylib, windows-xp))");
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                Any(vec![
                    Bool {
                        key: "windows_raw_dylib".to_owned(),
                    },
                    Bool {
                        key: "windows-xp".to_owned(),
                    },
                ]),
            )),
        )
    }

    #[test]
    fn test_value() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>("cfg(feature = \"bloop\")");
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                Value {
                    key: "feature".to_owned(),
                    value: "bloop".to_owned(),
                }
            ))
        )
    }

    #[test]
    fn test_emptystr() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>(r#"cfg(target_env = "")"#);
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                Value {
                    key: "target_env".to_owned(),
                    value: "".to_owned(),
                }
            ))
        )
    }

    #[test]
    fn test_complex() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>(
            "cfg(all(not(target_os = \"macos\"), not(windows), not(target_os = \"ios\")))",
        );
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                All(vec!(
                    Not(Box::new(Value {
                        key: "target_os".to_owned(),
                        value: "macos".to_owned(),
                    })),
                    Not(Box::new(Windows)),
                    Not(Box::new(Value {
                        key: "target_os".to_owned(),
                        value: "ios".to_owned(),
                    }))
                ))
            ))
        )
    }

    #[test]
    fn test_numcpus() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>(
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
                            key: "target_arch".to_owned(),
                            value: "x86_64".to_owned(),
                        },
                        Value {
                            key: "target_arch".to_owned(),
                            value: "aarch64".to_owned(),
                        }
                    ]),
                    Value {
                        key: "target_os".to_owned(),
                        value: "hermit".to_owned(),
                    }
                ])
            ))
        )
    }

    #[test]
    fn test_version() {
        let res =
            cfg::parse::<(_, nom::error::ErrorKind)>(r#"cfg(all(version = ">=6.0, <8.0", unix))"#);
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok((
                "",
                All(vec![
                    Version(semver::VersionReq {
                        comparators: vec![
                            semver::Comparator {
                                op: semver::Op::GreaterEq,
                                major: 6,
                                minor: Some(0),
                                patch: None,
                                pre: semver::Prerelease::EMPTY,
                            },
                            semver::Comparator {
                                op: semver::Op::Less,
                                major: 8,
                                minor: Some(0),
                                patch: None,
                                pre: semver::Prerelease::EMPTY,
                            },
                        ],
                    }),
                    Unix,
                ]),
            )),
        );
    }
}
