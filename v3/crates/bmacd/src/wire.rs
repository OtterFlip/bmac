// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Line framing for the engine socket: one JSON object per line, with a
//! size limit checked before the whole line is held in memory.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("frame longer than {0} bytes")]
    TooLong(usize),
    #[error("connection closed in the middle of a frame")]
    Truncated,
    #[error("frame is not valid: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Reads one line, without its newline. `Ok(None)` at a clean end of stream.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<Vec<u8>>, WireError> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(WireError::Truncated)
            };
        }
        let (chunk, done) = match available.iter().position(|&b| b == b'\n') {
            Some(at) => (&available[..at], at + 1),
            None => (available, available.len()),
        };
        if line.len() + chunk.len() > max {
            return Err(WireError::TooLong(max));
        }
        line.extend_from_slice(chunk);
        let newline = done > chunk.len();
        reader.consume(done);
        if newline {
            return Ok(Some(line));
        }
    }
}

pub async fn read_frame<T: DeserializeOwned, R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max: usize,
) -> Result<Option<T>, WireError> {
    match read_line(reader, max).await? {
        Some(line) => Ok(Some(serde_json::from_slice(&line)?)),
        None => Ok(None),
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    Ok(line)
}

pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
) -> Result<(), WireError> {
    writer.write_all(&encode(value)?).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_lines_across_buffer_boundaries() {
        let data = b"{\"a\":1}\n{\"b\":2}\n".to_vec();
        let mut reader = BufReader::with_capacity(3, &data[..]);
        assert_eq!(
            read_line(&mut reader, 100).await.unwrap().unwrap(),
            b"{\"a\":1}"
        );
        let second: serde_json::Value = read_frame(&mut reader, 100).await.unwrap().unwrap();
        assert_eq!(second["b"], 2);
        assert!(read_line(&mut reader, 100).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refuses_long_and_truncated_frames() {
        let long = [b'x'; 20];
        let mut reader = BufReader::with_capacity(4, &long[..]);
        assert!(matches!(
            read_line(&mut reader, 10).await,
            Err(WireError::TooLong(10))
        ));
        let mut reader = BufReader::new(&b"{\"a\":"[..]);
        assert!(matches!(
            read_line(&mut reader, 10).await,
            Err(WireError::Truncated)
        ));
        // Exactly the limit is allowed.
        let mut reader = BufReader::new(&b"0123456789\n"[..]);
        assert_eq!(read_line(&mut reader, 10).await.unwrap().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn bad_json_is_an_error() {
        let mut reader = BufReader::new(&b"not json\n"[..]);
        assert!(matches!(
            read_frame::<serde_json::Value, _>(&mut reader, 100).await,
            Err(WireError::Json(_))
        ));
    }
}
