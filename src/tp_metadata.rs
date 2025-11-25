/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fmt::Display;

use anyhow::Result;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap as _;
use serde::ser::SerializeSeq as _;

use crate::cargo::Manifest;

pub struct TpMetadata {
    pub name: String,
    pub version: semver::Version,
    pub licenses: Vec<License>,
}

impl Serialize for TpMetadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let TpMetadata {
            name,
            version,
            licenses,
        } = self;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        map.serialize_entry("version", version)?;
        map.serialize_entry("licenses", &OneLineList(licenses))?;
        map.end()
    }
}

impl TpMetadata {
    pub fn new(pkg: &Manifest) -> Self {
        TpMetadata {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            licenses: match &pkg.license {
                None => Vec::new(),
                Some(expr) => vec![format_spdx_license(expr)],
            },
        }
    }
}

pub enum License {
    Single(spdx::LicenseReq),
    Associative {
        op: spdx::expression::Operator,
        nodes: Vec<License>,
    },
    Verbatim(String),
}

impl Display for License {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            License::Single(req) => Display::fmt(req, formatter),
            License::Associative { op, nodes } => {
                for (i, node) in nodes.iter().enumerate() {
                    if i > 0 {
                        formatter.write_str(match op {
                            spdx::expression::Operator::And => " AND ",
                            spdx::expression::Operator::Or => " OR ",
                        })?;
                    }
                    let needs_paren = match node {
                        License::Single(_) => false,
                        License::Associative { .. } | License::Verbatim(_) => true,
                    };
                    if needs_paren {
                        formatter.write_str("(")?;
                    }
                    Display::fmt(node, formatter)?;
                    if needs_paren {
                        formatter.write_str(")")?;
                    }
                }
                Ok(())
            }
            License::Verbatim(s) => formatter.write_str(s),
        }
    }
}

impl Serialize for License {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

// https://spdx.github.io/spdx-spec/appendix-IV-SPDX-license-expressions/
fn format_spdx_license(spdx: &str) -> License {
    let mode = spdx::ParseMode::LAX; // designed for crates.io compatibility
    let expr = match spdx::Expression::parse_mode(spdx, mode) {
        Ok(expr) => expr,
        Err(_) => return License::Verbatim(spdx.to_owned()),
    };

    // Postorder traversal of license expression.
    let mut stack = Vec::new();
    for node in expr.iter() {
        match *node {
            spdx::expression::ExprNode::Req(ref expr) => {
                stack.push(License::Single(expr.req.clone()));
            }
            spdx::expression::ExprNode::Op(op) => {
                let rhs = stack.pop().unwrap();
                let lhs = stack.pop().unwrap();
                match (lhs, rhs) {
                    (
                        License::Associative {
                            op: lhs_op,
                            nodes: lhs_nodes,
                        },
                        License::Associative {
                            op: rhs_op,
                            nodes: rhs_nodes,
                        },
                    ) if lhs_op == op && rhs_op == op => {
                        let mut nodes = lhs_nodes;
                        nodes.extend(rhs_nodes);
                        stack.push(License::Associative { op, nodes });
                    }
                    (
                        License::Associative {
                            op: lhs_op,
                            mut nodes,
                        },
                        rhs,
                    ) if lhs_op == op => {
                        nodes.push(rhs);
                        stack.push(License::Associative { op, nodes });
                    }
                    (
                        lhs,
                        License::Associative {
                            op: rhs_op,
                            mut nodes,
                        },
                    ) if rhs_op == op => {
                        nodes.insert(0, lhs);
                        stack.push(License::Associative { op, nodes });
                    }
                    (lhs, rhs) => {
                        let nodes = vec![lhs, rhs];
                        stack.push(License::Associative { op, nodes });
                    }
                }
            }
        }
    }

    let license = stack.pop().unwrap();
    assert!(stack.is_empty());
    license
}

struct OneLineList<'a, T: Serialize>(&'a [T]);

impl<T: Serialize> Serialize for OneLineList<'_, T> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let mut seq = ser.serialize_seq(Some(serde_starlark::ONELINE))?;
        for element in self.0 {
            seq.serialize_element(element)?;
        }
        seq.end()
    }
}
