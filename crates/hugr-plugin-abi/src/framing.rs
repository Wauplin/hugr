//! Newline-delimited JSON framing shared by stdio transports (ARCHITECTURE §8.2).
//!
//! One JSON value per line is the wire format both the subprocess plugin
//! transport ([`SubprocessPlugin`](crate::SubprocessPlugin)) and stdio
//! JSON-RPC-style clients (e.g. the host's MCP client) speak. The helpers here
//! are the single implementation of that framing: serialize-then-`\n` on the
//! way out, skip-blank-lines-then-parse on the way in. Callers keep their own
//! protocol semantics (what a message *means*) and error taxonomy — a
//! [`FramingError`] splits cleanly into an IO half and a JSON half so it maps
//! onto whatever error enum the caller already has.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, Lines};

/// A framing-level failure: either the underlying stream broke (IO) or a line
/// was not valid JSON for the expected type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FramingError {
    /// Reading from or writing to the underlying stream failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Serializing the outgoing value or parsing an incoming line failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Serialize `value` as a single JSON line (`<json>\n`), write it, and flush.
pub async fn write_json_line<W, T>(writer: &mut W, value: &T) -> Result<(), FramingError>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read the next non-blank line and parse it as JSON. Blank lines (including
/// lines that are only whitespace/CR) are skipped; `Ok(None)` signals EOF.
/// `Lines::next_line` already strips the trailing `\n`/`\r\n`.
pub async fn read_json_line<R, T>(lines: &mut Lines<R>) -> Result<Option<T>, FramingError>
where
    R: AsyncBufRead + Unpin,
    T: DeserializeOwned,
{
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(&line)?));
    }
    Ok(None)
}
