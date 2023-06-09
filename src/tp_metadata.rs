/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Generate third-party metadata corresponding to METADATA.bzl

use std::collections::HashMap;
use std::fmt;
use std::fmt::Display;
use std::io;
use std::io::BufWriter;
use std::io::Write as _;

use anyhow::Result;
use serde::ser::SerializeMap as _;
use serde::ser::SerializeSeq as _;
use serde::Serialize;
use serde::Serializer;

use crate::cargo::Manifest;
use crate::cargo::Source;
use crate::config::BuckConfig;
use crate::index::ExtraMetadata;

struct TpMetadata<'a> {
    licenses: Vec<License>,
    maintainers: Vec<&'a str>,
    name: &'a str,
    owner: Option<&'a str>,
    upstream_address: &'a str,
    upstream_hash: &'a str,
    upstream_type: &'a str,
    version: &'a semver::Version,
}

impl Serialize for TpMetadata<'_> {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        let TpMetadata {
            licenses,
            maintainers,
            name,
            owner,
            upstream_address,
            upstream_hash,
            upstream_type,
            version,
        } = self;
        let mut map = ser.serialize_map(None)?;
        map.serialize_entry("licenses", &OneLineList(licenses))?;
        map.serialize_entry("maintainers", &OneLineList(maintainers))?;
        if let Some(owner) = owner {
            map.serialize_entry("owner", owner)?;
        }
        map.serialize_entry("name", name)?;
        map.serialize_entry("upstream_address", upstream_address)?;
        map.serialize_entry("upstream_hash", upstream_hash)?;
        map.serialize_entry("upstream_type", upstream_type)?;
        map.serialize_entry("version", version)?;
        map.end()
    }
}

pub fn write(
    config: &BuckConfig,
    pkg: &Manifest,
    extra: &HashMap<&str, ExtraMetadata>,
    out: &mut impl io::Write,
) -> Result<()> {
    let name = &pkg.name;
    let version = &pkg.version;

    let licenses = pkg
        .license
        .as_deref()
        .map_or_else(Vec::new, split_spdx_license_list);

    let owner = extra.get(pkg.name.as_str()).map(|m| m.oncall.as_str());

    let maintainers = owner.into_iter().collect();

    let cratesio_url;
    let mut upstream_address = "";
    let mut upstream_hash = "";
    let upstream_type = match &pkg.source {
        Source::CratesIo => {
            cratesio_url = format!("https://crates.io/crates/{}/{}", name, version);
            upstream_address = &cratesio_url;
            "crates.io"
        }
        Source::Git {
            repo, commit_hash, ..
        } => {
            upstream_address = repo.strip_suffix(".git").unwrap_or(repo);
            upstream_hash = commit_hash;
            if repo.starts_with("https://github.com/") {
                "github"
            } else if repo.starts_with("https://gitlab.com/") {
                "gitlab"
            } else if repo.starts_with("https://gitlab.redox-os.org/") {
                "gitlab.redox-os.org"
            } else {
                ""
            }
        }
        Source::Unrecognized(source) => source,
        Source::Local => "",
    };

    let metadata = TpMetadata {
        licenses,
        maintainers,
        name,
        owner,
        upstream_address,
        upstream_hash,
        upstream_type,
        version,
    };

    let mut out = BufWriter::new(out);

    out.write_all(config.generated_file_header.as_bytes())?;
    if !config.generated_file_header.is_empty() {
        out.write_all(b"\n")?;
    }

    let metadata_assignment = serde_starlark::Assignment::new("METADATA", metadata);
    write!(out, "{}", serde_starlark::to_string(&metadata_assignment)?)?;

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
