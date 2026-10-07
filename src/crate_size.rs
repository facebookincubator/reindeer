/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Resolve the size in bytes of a crates.io tarball.
//!
//! Buck2's `http_archive` accepts a `size_bytes` attribute. Providing it is the
//! only way for Buck2 to cache the downloaded crate across daemon restarts, so
//! reindeer emits it on every non-vendored `http_archive` it generates.

use std::fs;
use std::time::Duration;

use anyhow::Context;
use anyhow::bail;
use semver::Version;

use crate::Paths;
use crate::fast_vendor::find_cached_registry_archive_by_tarball;

/// Builds the HTTP client used for tarball size lookups.
///
/// Callers build this once and share it across all lookups so connections to
/// crates.io are reused.
pub fn build_http_client() -> anyhow::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent("reindeer (https://github.com/facebookincubator/reindeer)")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

/// Returns the size in bytes of the `.crate` tarball for `name`-`version`.
///
/// Prefers the size of the checksum-verified archive in the Cargo registry
/// cache under `paths.cargo_home`. When no cached archive is available,
/// issues an HTTPS HEAD request against `download_url` and reads the
/// tarball's `Content-Length`.
///
/// Fails when the size cannot be determined, e.g. when offline with a cold
/// Cargo cache.
pub fn crate_size_bytes(
    client: &reqwest::blocking::Client,
    paths: &Paths,
    name: &str,
    version: &Version,
    checksum: &str,
    download_url: &str,
) -> anyhow::Result<u64> {
    if let Some(size) = local_crate_size_bytes(paths, name, version, checksum) {
        return Ok(size);
    }
    remote_crate_size_bytes(client, name, version, download_url)
}

fn local_crate_size_bytes(
    paths: &Paths,
    name: &str,
    version: &Version,
    checksum: &str,
) -> Option<u64> {
    let tarball_name = format!("{name}-{version}.crate");
    let archive =
        match find_cached_registry_archive_by_tarball(&paths.cargo_home, &tarball_name, checksum) {
            Ok(archive) => archive,
            Err(err) => {
                log::debug!(
                    "failed to search Cargo cache {}: {:?}",
                    paths.cargo_home.display(),
                    err,
                );
                return None;
            }
        };
    let archive = archive?;
    match fs::metadata(&archive) {
        Ok(metadata) => Some(metadata.len()),
        Err(err) => {
            log::debug!("failed to stat {}: {:?}", archive.display(), err);
            None
        }
    }
}

// NB: `Response::content_length()` reports the response body's size hint,
// which is always 0 for HEAD. Read the header directly instead.
fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

fn remote_crate_size_bytes(
    client: &reqwest::blocking::Client,
    name: &str,
    version: &Version,
    download_url: &str,
) -> anyhow::Result<u64> {
    let response = client
        .head(download_url)
        .send()
        .with_context(|| format!("failed to fetch tarball size for {name}-{version}"))?;
    if !response.status().is_success() {
        bail!(
            "failed to fetch tarball size for {name}-{version}: HEAD {download_url} returned {}",
            response.status(),
        );
    }
    parse_content_length(response.headers()).with_context(|| {
        format!("HEAD {download_url} returned no usable Content-Length for {name}-{version}")
    })
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use sha2::Digest as _;

    use super::local_crate_size_bytes;
    use super::parse_content_length;
    use crate::Paths;
    use crate::buck::BuckPath;

    fn paths_with_cargo_home(cargo_home: PathBuf) -> Paths {
        Paths {
            buck_package: BuckPath(PathBuf::from("third-party/rust")),
            third_party_dir: PathBuf::new(),
            manifest_path: PathBuf::new(),
            lockfile_path: PathBuf::new(),
            cargo_home,
        }
    }

    fn sha256_hex(contents: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(contents))
    }

    #[test]
    fn local_size_uses_checksum_verified_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cargo_home = dir.path().join(".cargo");
        let cache_dir = cargo_home.join("registry/cache/index.crates.io-abcdef");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let contents = vec![0x42u8; 1234];
        std::fs::write(cache_dir.join("reindeer-size-test-1.2.3.crate"), &contents).unwrap();

        let paths = paths_with_cargo_home(cargo_home);
        let version = semver::Version::new(1, 2, 3);
        assert_eq!(
            local_crate_size_bytes(
                &paths,
                "reindeer-size-test",
                &version,
                &sha256_hex(&contents),
            ),
            Some(1234),
        );
    }

    #[test]
    fn local_size_ignores_checksum_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cargo_home = dir.path().join(".cargo");
        let cache_dir = cargo_home.join("registry/cache/index.crates.io-abcdef");
        std::fs::create_dir_all(&cache_dir).unwrap();

        std::fs::write(
            cache_dir.join("reindeer-size-test-1.2.3.crate"),
            b"stale contents",
        )
        .unwrap();

        let paths = paths_with_cargo_home(cargo_home);
        let version = semver::Version::new(1, 2, 3);
        assert_eq!(
            local_crate_size_bytes(
                &paths,
                "reindeer-size-test",
                &version,
                &sha256_hex(b"expected contents"),
            ),
            None,
        );
    }

    #[test]
    fn content_length_header_parsing() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_content_length(&headers), None);

        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("13366"),
        );
        assert_eq!(parse_content_length(&headers), Some(13366));

        headers.insert(
            reqwest::header::CONTENT_LENGTH,
            reqwest::header::HeaderValue::from_static("not-a-number"),
        );
        assert_eq!(parse_content_length(&headers), None);
    }

    #[test]
    fn remote_size_reads_content_length() {
        use std::io::Read as _;
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 1234\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let client = super::build_http_client().unwrap();
        let size = super::remote_crate_size_bytes(
            &client,
            "some-crate",
            &semver::Version::new(1, 0, 0),
            &format!("http://{addr}/download"),
        );
        server.join().unwrap();
        assert_eq!(size.unwrap(), 1234);
    }

    #[test]
    fn remote_size_failure_is_an_error() {
        let client = super::build_http_client().unwrap();
        // Nothing listens on the discard port, so this fails fast with
        // connection refused and no external network involved.
        let err = super::remote_crate_size_bytes(
            &client,
            "some-crate",
            &semver::Version::new(1, 0, 0),
            "http://127.0.0.1:9/download",
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("some-crate-1.0.0"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn local_size_missing_without_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_with_cargo_home(dir.path().join(".cargo"));
        let version = semver::Version::new(1, 2, 3);
        assert_eq!(
            local_crate_size_bytes(
                &paths,
                "reindeer-size-test-definitely-absent",
                &version,
                &sha256_hex(b"whatever"),
            ),
            None,
        );
    }
}
