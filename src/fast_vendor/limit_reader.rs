/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::io;
use std::io::Read;

/// A [`Read`] wrapper that enforces a byte-count limit and returns an error
/// when exceeded. Guards against zip-bomb attacks in compressed archives.
/// Matches the behavior of cargo's internal `LimitErrorReader`.
pub(super) struct LimitReader<R> {
    inner: io::Take<R>,
}

impl<R: Read> LimitReader<R> {
    pub fn new(r: R, limit: u64) -> Self {
        LimitReader {
            inner: r.take(limit),
        }
    }
}

impl<R: Read> Read for LimitReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(0) if self.inner.limit() == 0 => {
                Err(io::Error::other("maximum limit reached when reading"))
            }
            e => e,
        }
    }
}

#[cfg(test)]
mod test {
    use std::io::Cursor;
    use std::io::Read as _;

    use super::LimitReader;

    // Invariant: LimitReader returns data normally when under the limit
    #[test]
    fn test_limit_reader_under_limit() {
        let data = b"hello world";
        let mut reader = LimitReader::new(Cursor::new(data), 100);
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, data);
    }

    // Invariant: LimitReader returns an error when the byte limit is reached
    #[test]
    fn test_limit_reader_at_limit_errors() {
        let data = b"hello world";
        let mut reader = LimitReader::new(Cursor::new(data), 5);
        let mut buf = [0u8; 32];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        let result = reader.read(&mut buf);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("maximum limit reached"),
        );
    }
}
