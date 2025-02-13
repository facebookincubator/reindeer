/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use nom::IResult;
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
use nom::combinator::opt;
use nom::combinator::recognize;
use nom::combinator::verify;
use nom::error::ContextError;
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

use crate::platform::PlatformPredicate;

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

fn sep<'a, E: ParseError<&'a str>>(sep: char) -> impl FnMut(&'a str) -> IResult<&'a str, (), E> {
    map(preceded(sp, char(sep)), |_| ())
}

fn keyword<'a, E: ParseError<&'a str>>(
    kw: &'static str,
) -> impl FnMut(&'a str) -> IResult<&'a str, (), E> {
    map(verify(atom, move |s: &str| s == kw), |_| ())
}

// Parse an atom comprising one or more hyphen-separated words. Each word
// has a first character that satisfies is_xid_start and the rest satisfy
// is_xid_continue.
//
// For example `target_os` or `x86_64-unknown-linux-gnu`
fn atom<'a, E: ParseError<&'a str>>(i: &'a str) -> IResult<&'a str, &'a str, E> {
    preceded(
        sp,
        recognize(separated_list1(
            tag("-"),
            pair(
                satisfy(|ch| is_xid_start(ch) || ch == '_'),
                many0_count(satisfy(is_xid_continue)),
            ),
        )),
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

pub(crate) fn parse<'a, E: ParseError<&'a str> + ContextError<&'a str>>(
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
    use crate::cfg;
    use crate::platform::PlatformPredicate::*;

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
        assert_eq!(res, Ok(("", Bool { key: "foobar" })))
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
                        key: "windows_raw_dylib",
                    },
                    Bool { key: "windows-xp" },
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
            Ok(("", Value {
                key: "feature",
                value: "bloop"
            }))
        )
    }

    #[test]
    fn test_emptystr() {
        let res = cfg::parse::<(_, nom::error::ErrorKind)>(r#"cfg(target_env = "")"#);
        println!("res = {:?}", res);
        assert_eq!(
            res,
            Ok(("", Value {
                key: "target_env",
                value: ""
            }))
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
