/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Generate third-party metadata corresponding to METADATA.bzl

use maplit::btreemap;
use serde::ser::{Serialize, Serializer};
use std::{
    collections::{BTreeMap, HashMap},
    fmt::{self, Display},
    iter::FromIterator,
};

use crate::{cargo::Manifest, index::ExtraMetadata};

const METADATA_KV_DELIM: &str = "#";
const METADATA_KEY_PREFIX: &str = "tp_";

/*
"metadata_spec": [
  {"name": "licenses", "type": "list"},
  {"name": "maintainers", "type": "list"},
  {"name": "name", "type": "str"},
  {"name": "upstream_address", "type": "str"},
  {"name": "upstream_hash", "type": "str"},
  {"name": "upstream_type", "type": "str"},
  {"name": "version", "type": "str"}
],
*/

#[derive(Debug, Clone, Eq, PartialEq)]
enum Type<'a> {
    Missing,
    Str(&'a str),
    String(String),
    List(Vec<&'a str>),
}

impl<'a> Serialize for Type<'a> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Type::Missing => ser.serialize_none(),
            Type::Str(s) => ser.serialize_str(s),
            Type::String(ref s) => ser.serialize_str(s),
            Type::List(v) => v.serialize(ser),
        }
    }
}

impl Type<'_> {
    fn is_missing(&self) -> bool {
        self == &Type::Missing
    }
}

impl<'a> Default for Type<'a> {
    fn default() -> Self {
        Type::Missing
    }
}

impl<'a, T> From<Option<&'a T>> for Type<'a>
where
    Type<'a>: From<&'a T>,
{
    fn from(v: Option<&'a T>) -> Self {
        match v {
            None => Type::Missing,
            Some(v) => Type::from(v),
        }
    }
}

impl<'a> From<&'a str> for Type<'a> {
    fn from(s: &'a str) -> Self {
        Type::Str(s)
    }
}

impl<'a> From<&'a String> for Type<'a> {
    fn from(s: &'a String) -> Self {
        Type::Str(s.as_str())
    }
}

impl<'a> From<&'a semver::Version> for Type<'a> {
    fn from(s: &'a semver::Version) -> Self {
        Type::String(s.to_string())
    }
}

impl<'a> FromIterator<&'a str> for Type<'a> {
    fn from_iter<T: IntoIterator<Item = &'a str>>(iter: T) -> Self {
        Type::List(iter.into_iter().collect())
    }
}

impl<'a> FromIterator<&'a String> for Type<'a> {
    fn from_iter<T: IntoIterator<Item = &'a String>>(iter: T) -> Self {
        Type::List(iter.into_iter().map(|s| s.as_str()).collect())
    }
}

impl Display for Type<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Type::Missing => Ok(()),
            Type::Str(s) => fmt.write_str(s),
            Type::String(ref s) => fmt.write_str(s),
            Type::List(l) => write!(fmt, "[{}]", l.join(",")),
        }
    }
}

pub fn gen_metadata<'a>(
    pkg: &'a Manifest,
    extra: &'a HashMap<&'a str, ExtraMetadata>,
) -> Vec<String> {
    let map: BTreeMap<&'static str, Type<'a>> = btreemap! {
        "licenses" => pkg.license.iter().collect(),
        "maintainers" => Type::from(
            extra
                .get(pkg.name.as_str())
                .map(|m| m.oncall.as_str())
                .unwrap_or("<dependency>")),
        "name" => Type::from(&pkg.name),
        "version" => Type::from(&pkg.version),
        "upstream_address" => Type::from(pkg.repository.as_ref()),
        "upstream_type" => Type::from(pkg.source.as_ref()),
    };

    map.into_iter()
        .filter(|(_, v)| !v.is_missing())
        .map(|(k, v)| {
            format!(
                "{}{}{}{}",
                METADATA_KEY_PREFIX,
                k,
                METADATA_KV_DELIM,
                serde_starlark::to_string(&v).unwrap()
            )
        })
        .collect()
}
