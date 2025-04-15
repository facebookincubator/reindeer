/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;

use serde::de::Deserialize;
use serde::de::Deserializer;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::de::value::MapAccessDeserializer;
use serde::de::value::SeqAccessDeserializer;
use serde::ser::Serialize;
use serde::ser::SerializeSeq;
use serde::ser::SerializeTupleStruct;
use serde::ser::Serializer;
use serde_starlark::FunctionCall;
use serde_starlark::MULTILINE;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SetOrMap<T> {
    Set(BTreeSet<T>),
    Map(BTreeMap<String, T>),
}

impl<T> Default for SetOrMap<T>
where
    T: Ord,
{
    fn default() -> Self {
        SetOrMap::Set(BTreeSet::new())
    }
}

impl<T> SetOrMap<T> {
    pub fn is_empty(&self) -> bool {
        match self {
            SetOrMap::Set(set) => set.is_empty(),
            SetOrMap::Map(map) => map.is_empty(),
        }
    }
}

impl<T> Serialize for SetOrMap<T>
where
    T: Ord + Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            SetOrMap::Set(set) => set.serialize(serializer),
            SetOrMap::Map(map) => map.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for SetOrMap<T>
where
    T: Ord + Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SetOrMapVisitor<T>(PhantomData<T>);

        impl<'de, T> Visitor<'de> for SetOrMapVisitor<T>
        where
            T: Ord + Deserialize<'de>,
        {
            type Value = SetOrMap<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("set or map")
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let de = SeqAccessDeserializer::new(seq);
                BTreeSet::deserialize(de).map(SetOrMap::Set)
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let de = MapAccessDeserializer::new(map);
                BTreeMap::deserialize(de).map(SetOrMap::Map)
            }
        }

        let visitor = SetOrMapVisitor(PhantomData);
        deserializer.deserialize_any(visitor)
    }
}

pub struct MultilineArray<'a, A>(&'a A);

impl<'a, A, T> Serialize for MultilineArray<'a, A>
where
    &'a A: IntoIterator<Item = &'a T>,
    T: Serialize + 'a,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut array = serializer.serialize_seq(Some(MULTILINE))?;
        for element in self.0 {
            array.serialize_element(element)?;
        }
        array.end()
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct SelectSet {
    pub common: BTreeSet<String>,
    pub selects: BTreeMap<String, BTreeSet<String>>,
}

impl SelectSet {
    pub fn is_empty(&self) -> bool {
        self.common.is_empty() && self.selects.is_empty()
    }
}

// Inspired by SelectSet (see link) but much simplified. If you stumble here
// and want to extend the implementation below, this link is a good reference
// for ideas:
// https://github.com/bazelbuild/rules_rust/blob/0.40.0/crate_universe/src/utils/starlark/select_set.rs#L15-L27
impl Serialize for SelectSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut plus = serializer.serialize_tuple_struct("+", MULTILINE)?;
        match (self.common.is_empty(), self.selects.is_empty()) {
            (_, true) => {
                plus.serialize_field(&self.common)?;
            }
            (true, false) => {
                plus.serialize_field(&FunctionCall::new("select", [&self.selects]))?;
            }
            (false, false) => {
                // force common to be always serialized over mutliple lines
                plus.serialize_field(&MultilineArray(&self.common))?;
                plus.serialize_field(&FunctionCall::new("select", [&self.selects]))?;
            }
        }
        plus.end()
    }
}

#[cfg(test)]
mod test {
    use indoc::indoc;

    use super::*;

    #[test]
    fn select_set_empty() {
        let select_set = SelectSet::default();

        let expected = indoc! {r#"
            []
        "#};

        assert_eq!(
            select_set.serialize(serde_starlark::Serializer).unwrap(),
            expected,
        );
    }

    #[test]
    fn select_set_only_common_single() {
        let common = BTreeSet::from(["a".to_owned()]);
        let select_set = SelectSet {
            common,
            ..Default::default()
        };

        let expected = indoc! {r#"
            ["a"]
        "#};

        assert_eq!(
            select_set.serialize(serde_starlark::Serializer).unwrap(),
            expected,
        );
    }

    #[test]
    fn select_set_only_common_multiple() {
        let common = BTreeSet::from(["a".to_owned(), "b".to_owned()]);
        let select_set = SelectSet {
            common,
            ..Default::default()
        };

        let expected = indoc! {r#"
            [
                "a",
                "b",
            ]
        "#};

        assert_eq!(
            select_set.serialize(serde_starlark::Serializer).unwrap(),
            expected,
        );
    }

    #[test]
    fn select_set_only_selects() {
        let selects = BTreeMap::from([
            (
                "DEFAULT".to_owned(),
                BTreeSet::from(["a".to_owned(), "b".to_owned()]),
            ),
            (
                "ovr_config//third-party/some/constraints:1".to_owned(),
                BTreeSet::from(["c".to_owned()]),
            ),
            (
                "ovr_config//third-party/some/constraints:2".to_owned(),
                BTreeSet::from(["d".to_owned(), "e".to_owned()]),
            ),
        ]);

        let select_set = SelectSet {
            selects,
            ..Default::default()
        };

        let expected = indoc! {r#"
            select({
                "DEFAULT": [
                    "a",
                    "b",
                ],
                "ovr_config//third-party/some/constraints:1": ["c"],
                "ovr_config//third-party/some/constraints:2": [
                    "d",
                    "e",
                ],
            })
        "#};

        assert_eq!(
            select_set.serialize(serde_starlark::Serializer).unwrap(),
            expected,
        );
    }

    #[test]
    fn select_set_common_and_selects() {
        let common = BTreeSet::from(["a".to_owned()]);
        let selects = BTreeMap::from([
            (
                "DEFAULT".to_owned(),
                BTreeSet::from(["a".to_owned(), "b".to_owned()]),
            ),
            (
                "ovr_config//third-party/some/constraints:1".to_owned(),
                BTreeSet::from(["c".to_owned()]),
            ),
        ]);

        let select_set = SelectSet { common, selects };

        let expected = indoc! {r#"
            [
                "a",
            ] + select({
                "DEFAULT": [
                    "a",
                    "b",
                ],
                "ovr_config//third-party/some/constraints:1": ["c"],
            })
        "#};

        assert_eq!(
            select_set.serialize(serde_starlark::Serializer).unwrap(),
            expected,
        );
    }
}
