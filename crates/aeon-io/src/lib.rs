//! Tokio I/O abstraction layer for Aeon.
//!
//! All I/O in Aeon goes through this crate. Standard tokio by default,
//! with io_uring behind a feature flag for Linux 5.11+.
//!
//! **Rule**: No `std::fs`, `std::net`, `std::io::Read/Write` on the hot path.
//! Use `aeon_io::read()` / `aeon_io::write()` instead.

// FT-10: no-panic policy. Production code in this crate must not use
// `.unwrap()` or `.expect()` except for explicitly-documented startup-time
// invariants, which must carry an `#[allow(...)]` attribute with rationale.
// Test modules and benches are exempt (`cfg(not(test))`).
#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

use aeon_types::AeonError;
use bytes::Bytes;

/// Read bytes from a tokio `AsyncRead` source into a `Bytes` buffer.
pub async fn read<R>(reader: &mut R, buf: &mut [u8]) -> Result<usize, AeonError>
where
    R: tokio::io::AsyncReadExt + Unpin,
{
    reader.read(buf).await.map_err(|e| AeonError::Connection {
        message: format!("read error: {e}"),
        source: Some(Box::new(e)),
        retryable: true,
    })
}

/// Write bytes from a `Bytes` buffer to a tokio `AsyncWrite` sink.
pub async fn write<W>(writer: &mut W, data: &Bytes) -> Result<(), AeonError>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    writer
        .write_all(data)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("write error: {e}"),
            source: Some(Box::new(e)),
            retryable: true,
        })
}

/// Flush a tokio `AsyncWrite` sink.
pub async fn flush<W>(writer: &mut W) -> Result<(), AeonError>
where
    W: tokio::io::AsyncWriteExt + Unpin,
{
    writer.flush().await.map_err(|e| AeonError::Connection {
        message: format!("flush error: {e}"),
        source: Some(Box::new(e)),
        retryable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_write_roundtrip() {
        let data = Bytes::from_static(b"hello aeon");
        let mut buf = Vec::new();

        // Write
        write(&mut buf, &data).await.unwrap();
        assert_eq!(&buf, b"hello aeon");

        // Read back
        let mut cursor = std::io::Cursor::new(buf);
        let mut read_buf = [0u8; 64];
        let n = read(&mut cursor, &mut read_buf).await.unwrap();
        assert_eq!(&read_buf[..n], b"hello aeon");
    }
}
