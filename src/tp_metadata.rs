/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Display;

use anyhow::Result;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap as _;
use serde::ser::SerializeSeq as _;

use crate::buck::Metadata;
use crate::cargo::Manifest;
use crate::cargo::Source;
use crate::purl;
use crate::purl::Purl;

#[derive(Clone)]
pub struct TpMetadata {
    pub name: String,
    pub version: semver::Version,
    pub licenses: Vec<License>,
    pub namespace: Option<String>,
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
            namespace,
        } = self;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("name", name)?;
        if let Some(ns) = namespace {
            map.serialize_entry("namespace", ns)?;
        }
        map.serialize_entry("version", version)?;
        map.serialize_entry("licenses", &OneLineList(licenses))?;
        map.end()
    }
}

/// Extract the namespace (user/org) from a GitHub/GitLab/Bitbucket URL.
///
/// Per purl-spec, for github type packages, namespace is the user or organization.
/// See: third-party/purl-spec/main/src/types-doc/github-definition.md
fn extract_namespace_from_url(url: &str) -> Option<String> {
    for host in ["github.com/", "gitlab.com/", "bitbucket.org/"] {
        if let Some(after_host) = url.split(host).nth(1) {
            if let Some(namespace) = after_host.split('/').next() {
                if !namespace.is_empty() {
                    return Some(namespace.to_lowercase());
                }
            }
        }
    }
    None
}

impl TpMetadata {
    pub fn new(pkg: &Manifest) -> Self {
        let namespace = match &pkg.source {
            Source::Git { repo, .. } => extract_namespace_from_url(repo),
            _ => None,
        };
        TpMetadata {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            licenses: match &pkg.license {
                None => Vec::new(),
                Some(expr) => vec![format_spdx_license(expr)],
            },
            namespace,
        }
    }
}

/// Insert `supply_chain.*` metadata describing `pkg`, for SBOM generation:
///
/// - `supply_chain.purl`: Package URL, absent for packages with no location
///   outside this checkout (path dependencies).
/// - `supply_chain.license`: normalized SPDX license expression. A package
///   declaring only a `license-file` gets a `LicenseRef-` identifier for it,
///   whose text is in the target's `licenses`.
/// - `supply_chain.license_unparsed`: the declared license, verbatim, when it
///   is not a valid SPDX expression.
/// - `supply_chain.source_repository`, `supply_chain.homepage`: the URLs from
///   Cargo.toml's `repository` and `homepage`. These are declared by the
///   package and not verified by anyone, and the source repository is not
///   pinned to this version. Not to be confused with the PURL's
///   `repository_url`, which is the registry the package was fetched from.
pub fn insert_supply_chain_metadata(
    pkg: &Manifest,
    metadata: &mut BTreeMap<String, Box<dyn Metadata>>,
) {
    match Purl::for_cargo_package(&pkg.name, &pkg.version, &pkg.source) {
        Some(purl) => {
            metadata.insert("supply_chain.purl".to_owned(), Box::new(purl));
        }
        None if matches!(pkg.source, Source::Local) => {}
        None => log::warn!(
            "no package URL for {} {}: unsupported source {:?}",
            pkg.name,
            pkg.version,
            pkg.source,
        ),
    }
    for (key, field, value) in [
        (
            "supply_chain.source_repository",
            "repository",
            &pkg.repository,
        ),
        ("supply_chain.homepage", "homepage", &pkg.homepage),
    ] {
        let Some(value) = value else { continue };
        match purl::sanitize_url(value) {
            Some(url) => {
                metadata.insert(key.to_owned(), Box::new(url));
            }
            None => log::warn!(
                "{} {}: `{field}` is not a URL, omitting it: {value:?}",
                pkg.name,
                pkg.version,
            ),
        }
    }
    let license = match (&pkg.license, &pkg.license_file) {
        (Some(license), _) => match parse_spdx_license(license) {
            Some(license) => license.to_string(),
            None => {
                metadata.insert(
                    "supply_chain.license_unparsed".to_owned(),
                    Box::new(license.clone()),
                );
                return;
            }
        },
        (None, Some(_)) => license_ref(&pkg.name, &pkg.version),
        (None, None) => return,
    };
    metadata.insert("supply_chain.license".to_owned(), Box::new(license));
}

/// SPDX identifier for a package's custom license. Unique per package version,
/// since the license text may change between versions.
///
/// An idstring may contain only ASCII letters, digits, `.` and `-`. Crate
/// names never contain `.`, so `_` becomes `.` to keep `foo_bar` and `foo-bar`
/// distinct. The `+` before semver build metadata becomes `-`, which can only
/// collide with a prerelease version of the same crate.
fn license_ref(name: &str, version: &semver::Version) -> String {
    let name = name.replace('_', ".");
    let version = version.to_string().replace('+', "-");
    format!("LicenseRef-{name}-{version}")
}

#[derive(Clone)]
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
    parse_spdx_license(spdx).unwrap_or_else(|| License::Verbatim(spdx.to_owned()))
}

/// Normalize a license expression, or `None` if it isn't SPDX.
fn parse_spdx_license(spdx: &str) -> Option<License> {
    let mode = spdx::ParseMode::LAX; // designed for crates.io compatibility
    let expr = spdx::Expression::parse_mode(spdx, mode).ok()?;

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
    Some(license)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_ref_is_a_valid_spdx_expression() {
        for (name, version, expected) in [
            ("ring", "0.17.8", "LicenseRef-ring-0.17.8"),
            (
                "foo_bar",
                "1.0.0+build.5",
                "LicenseRef-foo.bar-1.0.0-build.5",
            ),
            ("foo-bar", "1.0.0", "LicenseRef-foo-bar-1.0.0"),
        ] {
            let version = semver::Version::parse(version).unwrap();
            let license_ref = license_ref(name, &version);
            assert_eq!(license_ref, expected);
            let roundtrip = parse_spdx_license(&license_ref).map(|l| l.to_string());
            assert_eq!(roundtrip.as_deref(), Some(expected));
        }
    }
}
