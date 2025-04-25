/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::collections::HashMap;

use serde::Serialize;
use serde::Serializer;

use crate::buck::BuckPath;
use crate::buck::Name;
use crate::buck::SubtargetOrPath;

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Subtarget {
    pub target: Name,
    // Can be empty string. TODO: consider replacing with Option<BuckPath>, or
    // adding a variant to SubtargetOrPath that holds only a target name without
    // subtarget.
    pub relative: BuckPath,
}

impl Serialize for Subtarget {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        if self.relative.is_empty() {
            ser.collect_str(&format_args!(":{}", self.target))
        } else {
            ser.collect_str(&format_args!(":{}[{}]", self.target, self.relative))
        }
    }
}

pub struct CollectSubtargets(HashMap<Name, BTreeSet<BuckPath>>);

impl CollectSubtargets {
    pub fn new() -> Self {
        CollectSubtargets(HashMap::new())
    }

    pub fn insert(&mut self, item: &impl MaybeHasSubtarget) {
        item.insert_into(self);
    }

    pub fn insert_all<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a (impl MaybeHasSubtarget + 'a)>,
    ) {
        for item in items {
            item.insert_into(self);
        }
    }

    pub fn remove(&mut self, name: &Name) -> Option<BTreeSet<BuckPath>> {
        self.0.remove(name)
    }
}

pub trait MaybeHasSubtarget {
    fn insert_into(&self, collect: &mut CollectSubtargets);
}

impl MaybeHasSubtarget for Subtarget {
    fn insert_into(&self, collect: &mut CollectSubtargets) {
        if !self.relative.is_empty() {
            collect
                .0
                .entry(self.target.clone())
                .or_insert_with(BTreeSet::new)
                .insert(self.relative.clone());
        }
    }
}

impl MaybeHasSubtarget for SubtargetOrPath {
    fn insert_into(&self, collect: &mut CollectSubtargets) {
        if let SubtargetOrPath::Subtarget(subtarget) = self {
            subtarget.insert_into(collect);
        }
    }
}
