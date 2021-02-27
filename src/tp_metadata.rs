/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Generate third-party metadata corresponding to METADATA.bzl

use anyhow::Result;
use serde::{Serialize, Serializer};
use std::{
    collections::HashMap,
    fmt::{self, Display},
    io::{BufWriter, Write},
};

use crate::{cargo::Manifest, config::BuckConfig, index::ExtraMetadata};

#[derive(Serialize)]
struct TpMetadata<'a> {
    name: &'a str,
    version: &'a semver::Version,
    licenses: Vec<License>,
    maintainers: Vec<&'a str>,
    upstream_address: &'a str,
    upstream_hash: &'a str,
    upstream_type: &'a str,
}

pub fn write(
    config: &BuckConfig,
    pkg: &Manifest,
    extra: &HashMap<&str, ExtraMetadata>,
    out: &mut impl Write,
) -> Result<()> {
    let name = &pkg.name;
    let version = &pkg.version;

    let licenses = pkg
        .license
        .as_deref()
        .map_or_else(Vec::new, split_spdx_license_list);

    let maintainers = extra
        .get(pkg.name.as_str())
        .map(|m| m.oncall.as_str())
        .into_iter()
        .collect();

    let mut is_cratesio = false;
    let upstream_type = match &pkg.source {
        Some(source) if source == "registry+https://github.com/rust-lang/crates.io-index" => {
            is_cratesio = true;
            "crates.io"
        }
        Some(source) => source,
        None => "",
    };

    let cratesio_url;
    let upstream_address = match &pkg.repository {
        Some(repository) => repository.as_str(),
        None if is_cratesio => {
            cratesio_url = format!("https://crates.io/crates/{}/{}", name, version);
            &cratesio_url
        }
        None => "",
    };

    let metadata = TpMetadata {
        name,
        version,
        licenses,
        maintainers,
        upstream_address,
        upstream_hash: "",
        upstream_type,
    };

    let mut out = BufWriter::new(out);

    out.write_all(config.generated_file_header.as_bytes())?;
    if !config.generated_file_header.is_empty() {
        out.write_all(b"\n")?;
    }

    out.write_all(b"METADATA = ")?;
    let json_formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, json_formatter);
    metadata.serialize(&mut serializer)?;
    out.write_all(b"\n")?;

    out.flush()?;
    Ok(())
}

enum License {
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
fn split_spdx_license_list(spdx: &str) -> Vec<License> {
    let mode = spdx::ParseMode::Lax; // designed for crates.io compatibility
    let expr = match spdx::Expression::parse_mode(spdx, mode) {
        Ok(expr) => expr,
        Err(_) => return vec![License::Verbatim(spdx.to_owned())],
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

    if let License::Associative {
        op: spdx::expression::Operator::Or,
        nodes,
    } = license
    {
        nodes
    } else {
        vec![license]
    }
}
