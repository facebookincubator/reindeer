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

use serde::de::value::MapAccessDeserializer;
use serde::de::value::SeqAccessDeserializer;
use serde::de::Deserialize;
use serde::de::Deserializer;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde::ser::Serialize;
use serde::ser::Serializer;

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
