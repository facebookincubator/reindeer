/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Package URLs, per ECMA-427 (https://github.com/package-url/purl-spec).

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Display;
use std::fmt::Write as _;

use serde::Serialize;
use serde::Serializer;

use crate::cargo::Source;

/// The source of a Cargo package, as far as a PURL is concerned: where a
/// consumer of an SBOM would go to fetch this exact package.
#[derive(Debug)]
enum CargoOrigin<'a> {
    CratesIo,
    /// An alternate registry, identified by its index URL with no
    /// credentials in it.
    Registry {
        index: String,
    },
    /// A git repository with no credentials in its URL.
    Git {
        repo: String,
        commit: &'a str,
    },
}

/// The subset of PURL components that reindeer produces. Subpaths are not
/// modeled because nothing generates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Purl {
    /// Lowercase ASCII per clause 5.6.2; only constructed from constants.
    ty: &'static str,
    namespace: Vec<String>,
    name: String,
    version: Option<String>,
    /// Keys are constants satisfying clause 5.6.6. Sorted by key, which is
    /// the order the spec recommends for building.
    qualifiers: BTreeMap<&'static str, String>,
}

impl Purl {
    fn new(ty: &'static str, name: impl Into<String>) -> Self {
        Purl {
            ty,
            namespace: Vec::new(),
            name: name.into(),
            version: None,
            qualifiers: BTreeMap::new(),
        }
    }

    fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// A qualifier with an empty value is equivalent to its absence (clause
    /// 5.6.6), so it is dropped.
    fn qualifier(mut self, key: &'static str, value: impl Into<String>) -> Self {
        let value = value.into();
        if !value.is_empty() {
            self.qualifiers.insert(key, value);
        }
        self
    }

    /// PURL for a Cargo package, or `None` if the package has no location
    /// outside of this checkout (a path dependency or workspace member) or
    /// its source can't be represented faithfully.
    pub fn for_cargo_package(
        name: &str,
        version: &semver::Version,
        source: &Source,
    ) -> Option<Purl> {
        let origin = match source {
            Source::Local => return None,
            Source::CratesIo => CargoOrigin::CratesIo,
            Source::Git { repo, commit_hash } => CargoOrigin::Git {
                repo: sanitize_url(repo)?,
                commit: commit_hash,
            },
            Source::Unrecognized(source) => {
                let index = source
                    .strip_prefix("registry+")
                    .or_else(|| source.strip_prefix("sparse+"))?;
                // Cargo reports this when a dependency names crates.io's
                // sparse index as an alternate registry. It is the same
                // registry, so must have the same PURL.
                if index == "https://index.crates.io/" {
                    CargoOrigin::CratesIo
                } else {
                    CargoOrigin::Registry {
                        index: sanitize_url(index)?,
                    }
                }
            }
        };

        let purl = Purl::new("cargo", name).version(version.to_string());
        Some(match origin {
            CargoOrigin::CratesIo => purl,
            CargoOrigin::Registry { index } => purl.qualifier("repository_url", index),
            CargoOrigin::Git { repo, commit } => {
                purl.qualifier("vcs_url", format!("git+{repo}@{commit}"))
            }
        })
    }
}

/// Normalize a URL for supply chain metadata, or `None` if it does not parse
/// as an absolute URL.
///
/// Annex B.3.5: user names and passwords in a vcs_url are not supported, and
/// a credential in any URL must not end up in an SBOM. Cargo commonly sees
/// `ssh://git@github.com/...`, which identifies the same repository without
/// the username, so that one is removed quietly.
pub fn sanitize_url(url: &str) -> Option<String> {
    let mut url = url::Url::parse(url).ok()?;
    let conventional_ssh_user = url.scheme() == "ssh" && url.username() == "git";
    if url.password().is_some() || !(url.username().is_empty() || conventional_ssh_user) {
        log::warn!(
            "removing credentials from {}://{} in supply chain metadata",
            url.scheme(),
            url.host_str().unwrap_or_default(),
        );
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    Some(url.into())
}

/// Clause 5.4: everything other than alphanumerics, `.-_~`, and `:` is
/// percent-encoded as UTF-8.
fn encode(f: &mut fmt::Formatter, component: &str) -> fmt::Result {
    for byte in component.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~' | b':' => {
                f.write_char(byte as char)?
            }
            _ => write!(f, "%{byte:02X}")?,
        }
    }
    Ok(())
}

impl Display for Purl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "pkg:{}/", self.ty)?;
        for segment in &self.namespace {
            encode(f, segment)?;
            f.write_char('/')?;
        }
        encode(f, &self.name)?;
        if let Some(version) = &self.version {
            f.write_char('@')?;
            encode(f, version)?;
        }
        for (i, (key, value)) in self.qualifiers.iter().enumerate() {
            f.write_char(if i == 0 { '?' } else { '&' })?;
            write!(f, "{key}=")?;
            encode(f, value)?;
        }
        Ok(())
    }
}

impl Serialize for Purl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo(name: &str, version: &str, source: Source) -> Option<String> {
        let version = semver::Version::parse(version).unwrap();
        Purl::for_cargo_package(name, &version, &source).map(|purl| purl.to_string())
    }

    fn git(repo: &str, commit_hash: &str) -> Source {
        Source::Git {
            repo: repo.to_owned(),
            commit_hash: commit_hash.to_owned(),
        }
    }

    #[test]
    fn crates_io_needs_no_qualifiers() {
        assert_eq!(
            cargo("rand", "0.7.2", Source::CratesIo).as_deref(),
            Some("pkg:cargo/rand@0.7.2"),
        );
    }

    #[test]
    fn semver_build_metadata_is_encoded() {
        assert_eq!(
            cargo("libz-sys", "1.1.0+zlib-1.3", Source::CratesIo).as_deref(),
            Some("pkg:cargo/libz-sys@1.1.0%2Bzlib-1.3"),
        );
    }

    #[test]
    fn git_source_is_a_vcs_url_pinned_to_the_commit() {
        assert_eq!(
            cargo(
                "rustls-native-certs",
                "0.8.1",
                git("https://github.com/rustls/rustls-native-certs", "9734b4cc"),
            )
            .as_deref(),
            Some(
                "pkg:cargo/rustls-native-certs@0.8.1\
                 ?vcs_url=git%2Bhttps:%2F%2Fgithub.com%2Frustls%2Frustls-native-certs%409734b4cc"
            ),
        );
    }

    #[test]
    fn git_source_credentials_are_dropped() {
        assert_eq!(
            cargo(
                "foo",
                "0.1.0",
                git("ssh://git@github.com/owner/foo.git", "abc")
            )
            .as_deref(),
            Some("pkg:cargo/foo@0.1.0?vcs_url=git%2Bssh:%2F%2Fgithub.com%2Fowner%2Ffoo.git%40abc"),
        );
    }

    #[test]
    fn alternate_registry_is_a_repository_url() {
        let source = |s: &str| Source::Unrecognized(s.to_owned());
        assert_eq!(
            cargo(
                "foo",
                "1.0.0",
                source("sparse+https://cargo.example.com/index/")
            )
            .as_deref(),
            Some("pkg:cargo/foo@1.0.0?repository_url=https:%2F%2Fcargo.example.com%2Findex%2F"),
        );
        assert_eq!(
            cargo(
                "foo",
                "1.0.0",
                source("registry+https://example.com/index.git")
            )
            .as_deref(),
            Some("pkg:cargo/foo@1.0.0?repository_url=https:%2F%2Fexample.com%2Findex.git"),
        );
    }

    #[test]
    fn registry_credentials_are_dropped() {
        let source = Source::Unrecognized("sparse+https://token@cargo.example.com/".to_owned());
        assert_eq!(
            cargo("foo", "1.0.0", source).as_deref(),
            Some("pkg:cargo/foo@1.0.0?repository_url=https:%2F%2Fcargo.example.com%2F"),
        );
    }

    #[test]
    fn crates_io_sparse_index_is_crates_io() {
        let source = Source::Unrecognized("sparse+https://index.crates.io/".to_owned());
        assert_eq!(
            cargo("itoa", "1.0.15", source).as_deref(),
            Some("pkg:cargo/itoa@1.0.15"),
        );
    }

    #[test]
    fn packages_without_a_locator_have_no_purl() {
        // A bare `pkg:cargo/foo@0.1.0` would claim to be on crates.io.
        assert_eq!(cargo("foo", "0.1.0", Source::Local), None);
        let unknown = Source::Unrecognized("directory+file:///vendor".to_owned());
        assert_eq!(cargo("foo", "0.1.0", unknown), None);
    }

    /// Build cases from purl-spec's tests/spec/specification-test.json and
    /// Annex B, covering encoding rules that Cargo packages don't exercise.
    #[test]
    fn spec_conformance() {
        let openssl = Purl::new("generic", "openssl")
            .version("1.1.10g")
            .qualifier(
                "checksum",
                "sha1:ad9503c3e994a4f,sha256:41bf9088b3a1e6c1ef1d",
            );
        assert_eq!(
            openssl.to_string(),
            "pkg:generic/openssl@1.1.10g?checksum=sha1:ad9503c3e994a4f%2Csha256:41bf9088b3a1e6c1ef1d",
        );

        let mut sorted = Purl::new("generic", "a b")
            .qualifier("vcs_url", "x")
            .qualifier("download_url", "y")
            .qualifier("file_name", "");
        sorted.namespace = vec!["ns".to_owned(), "é".to_owned()];
        assert_eq!(
            sorted.to_string(),
            "pkg:generic/ns/%C3%A9/a%20b?download_url=y&vcs_url=x",
        );
    }
}
